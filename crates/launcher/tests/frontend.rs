use launcher_agent::client::Client;
use launcher_agent::protocol::{FrontendKind, FrontendPollParams, FrontendPollResult};
use launcher_agent::service::{FrontendRouter, Server, Service};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

struct Running {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}
impl Running {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mc-frontend-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let app = dir.path().join("frontend");
        fs::write(&app, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        let service = Service::fixture().unwrap();
        service
            .attach_frontend(
                FrontendRouter::new(FrontendKind::Gpui, app, dir.path().join("rpc")).unwrap(),
            )
            .unwrap();
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || server.serve_until(&worker_stop));
        Self {
            dir,
            stop,
            worker: Some(worker),
        }
    }
    fn client(&self) -> Client {
        Client::connect(self.dir.path().join("rpc")).unwrap()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}

#[test]
fn frontend_poll_is_capability_gated_and_session_drop_releases_control_lease() {
    let running = Running::new();
    let mut owner = running.client();
    assert!(
        owner
            .hello()
            .capabilities
            .iter()
            .any(|cap| cap == "frontendControl")
    );
    let poll = |client: &mut Client| {
        client
            .request::<FrontendPollResult>(
                "frontend.poll",
                FrontendPollParams {
                    kind: FrontendKind::Gpui,
                    ack: None,
                },
            )
            .unwrap()
    };
    assert!(poll(&mut owner).registered);
    let mut other = running.client();
    assert!(!poll(&mut other).registered);
    drop(owner);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if poll(&mut other).registered {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!("service session drop did not release frontend control lease");
}
