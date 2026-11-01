//! Independent service poller for backend-owned frontend presentation requests.
use launcher_protocol::{FrontendKind, FrontendPollParams};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::{self, SyncSender},
};
use std::{path::Path, thread, time::Duration};

const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) struct FrontendControl {
    stop: Arc<AtomicBool>,
}

impl FrontendControl {
    pub(crate) fn start(
        path: &Path,
        kind: FrontendKind,
        events: SyncSender<Value>,
    ) -> std::io::Result<Self> {
        let path = path.to_owned();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        thread::Builder::new()
            .name("maccer-frontend-control".into())
            .spawn(move || run(path, kind, events, worker_stop))?;
        Ok(Self { stop })
    }

    pub(crate) fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn run(
    path: std::path::PathBuf,
    kind: FrontendKind,
    events: SyncSender<Value>,
    stop: Arc<AtomicBool>,
) {
    let mut client = match launcher_client::Client::connect(&path) {
        Ok(client) => client,
        Err(error) => {
            report_error(&events, format!("{error:#}"));
            return;
        }
    };
    if !client
        .hello()
        .capabilities
        .iter()
        .any(|capability| capability == "frontendControl")
    {
        return;
    }

    let mut pending: Option<String> = None;
    let mut ack: Option<String> = None;
    while !stop.load(Ordering::Acquire) {
        if let Some(serial) = pending.take() {
            match events.try_send(json!({"type":"present"})) {
                Ok(()) => ack = Some(serial),
                Err(mpsc::TrySendError::Full(_)) => pending = Some(serial),
                Err(mpsc::TrySendError::Disconnected(_)) => return,
            }
        }

        let params = FrontendPollParams {
            kind,
            ack: ack.clone(),
        };
        match client.poll_frontend(&params) {
            Ok(result) => {
                if !result.registered {
                    // Keep polling: another frontend session may release its lease.
                    thread::sleep(POLL_INTERVAL);
                    continue;
                }
                if pending.is_none()
                    && let Some(serial) = result.show
                    && ack.as_deref() != Some(serial.as_str())
                {
                    pending = Some(serial);
                }
            }
            Err(error) => {
                report_error(&events, format!("{error:#}"));
                return;
            }
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn report_error(events: &SyncSender<Value>, message: String) {
    let _ = events.try_send(json!({"type":"presentationError","message":message}));
}

#[cfg(test)]
mod tests {
    use super::*;
    use launcher_protocol::{PROTOCOL_MAJOR, PROTOCOL_MINOR, RpcResponse, framing};
    use serde_json::Value;
    use std::{
        io::{self, Read},
        os::unix::net::UnixListener,
        thread,
        time::Instant,
    };
    use tempfile::tempdir;

    fn read_request(stream: &mut std::os::unix::net::UnixStream) -> Value {
        serde_json::from_slice(&framing::read_frame(stream).unwrap()).unwrap()
    }
    fn reply(stream: &mut std::os::unix::net::UnixStream, id: &str, result: Value) {
        framing::write_frame(
            stream,
            &serde_json::to_vec(&RpcResponse::success(id, result)).unwrap(),
        )
        .unwrap();
    }
    fn wait_recv(receiver: &mpsc::Receiver<Value>) -> Value {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Ok(value) = receiver.try_recv() {
                return value;
            }
            assert!(
                Instant::now() < deadline,
                "presentation event was not delivered"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn show_is_not_acknowledged_until_queue_delivery_and_is_not_duplicated() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (retry_seen, retry_received) = mpsc::channel();
        let (release_retry, retry_release) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let hello = read_request(&mut stream);
            reply(
                &mut stream,
                hello["id"].as_str().unwrap(),
                json!({
                    "major":PROTOCOL_MAJOR,"minor":PROTOCOL_MINOR,"instanceId":"i","sessionId":"s",
                    "capabilities":["frontendControl"]
                }),
            );
            let first = read_request(&mut stream);
            assert_eq!(first["method"], "frontend.poll");
            assert_eq!(first["params"]["kind"], "gpui");
            reply(
                &mut stream,
                first["id"].as_str().unwrap(),
                json!({"registered":true,"show":"serial-1"}),
            );
            let retry = read_request(&mut stream);
            assert!(
                retry["params"].get("ack").is_none(),
                "full queue must not ack event"
            );
            retry_seen.send(()).unwrap();
            retry_release.recv().unwrap();
            reply(
                &mut stream,
                retry["id"].as_str().unwrap(),
                json!({"registered":true,"show":"serial-1"}),
            );
            let ack = read_request(&mut stream);
            assert_eq!(ack["params"]["ack"], "serial-1");
            // The lease may expire between delivery and acknowledgment. A
            // re-offered serial must be acknowledged again, not delivered twice.
            reply(
                &mut stream,
                ack["id"].as_str().unwrap(),
                json!({"registered":true,"show":"serial-1"}),
            );
            let repeated_ack = read_request(&mut stream);
            assert_eq!(repeated_ack["params"]["ack"], "serial-1");
            reply(
                &mut stream,
                repeated_ack["id"].as_str().unwrap(),
                json!({"registered":true,"show":null}),
            );
        });
        let (events, receiver) = mpsc::sync_channel(1);
        events.try_send(json!({"type":"occupy"})).unwrap();
        let control = FrontendControl::start(&path, FrontendKind::Gpui, events).unwrap();
        retry_received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(wait_recv(&receiver), json!({"type":"occupy"}));
        release_retry.send(()).unwrap();
        assert_eq!(wait_recv(&receiver), json!({"type":"present"}));
        server.join().unwrap();
        control.stop();
        assert!(receiver.try_iter().all(|event| event["type"] != "present"));
    }

    #[test]
    fn legacy_service_without_capability_is_left_untouched() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("service.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let hello = read_request(&mut stream);
            reply(
                &mut stream,
                hello["id"].as_str().unwrap(),
                json!({
                    "major":PROTOCOL_MAJOR,"minor":4,"instanceId":"i","sessionId":"s","capabilities":[]
                }),
            );
            stream
                .set_read_timeout(Some(Duration::from_millis(250)))
                .unwrap();
            let mut bytes = [0; 1];
            match stream.read(&mut bytes) {
                Ok(0) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                other => panic!("unexpected request from a non-controller client: {other:?}"),
            }
        });
        let (events, receiver) = mpsc::sync_channel(2);
        let control = FrontendControl::start(&path, FrontendKind::Pill, events).unwrap();
        server.join().unwrap();
        assert!(receiver.try_recv().is_err());
        control.stop();
    }
}
