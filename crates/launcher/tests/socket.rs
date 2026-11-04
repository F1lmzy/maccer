use launcher_agent::client::Client;
use launcher_agent::protocol::{InvokeActionParams, Presentation, Route, framing};
use launcher_agent::service::{Server, Service};
use std::{
    io::Read,
    os::unix::{fs::PermissionsExt, net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

struct Running {
    directory: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<anyhow::Result<()>>>,
}
impl Running {
    fn start() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("mc-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server =
            Server::bind(directory.path().join("rpc"), Service::fixture().unwrap()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let thread = thread::spawn(move || server.serve_until(&worker_stop));
        Self {
            directory,
            stop,
            thread: Some(thread),
        }
    }
    fn path(&self) -> std::path::PathBuf {
        self.directory.path().join("rpc")
    }
    fn client(&self) -> Client {
        Client::connect(self.path()).unwrap()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.thread.take() {
            worker.join().unwrap().unwrap();
        }
        assert!(
            !self.path().exists(),
            "graceful shutdown cleans up its socket"
        );
    }
}

#[test]
fn two_clients_share_backend_but_not_views_or_actions() {
    let server = Running::start();
    let mut gpui = server.client();
    let mut qml = server.client();
    assert_eq!(gpui.hello().instance_id, qml.hello().instance_id);
    assert_ne!(gpui.hello().session_id, qml.hello().session_id);
    let home = gpui.open_view(Route::Home, "", 20).unwrap();
    let theme = home
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.id == "themes")
        .unwrap();
    let action = InvokeActionParams {
        view_id: home.view_id.clone(),
        revision: home.revision.clone(),
        action_ref: theme.actions[0].action_ref.clone(),
        operation_id: "navigate-themes".into(),
        confirmed: false,
    };
    let other_home = qml.open_view(Route::Home, "", 20).unwrap();
    let forged = InvokeActionParams {
        view_id: other_home.view_id.clone(),
        revision: other_home.revision.clone(),
        ..action.clone()
    };
    assert!(
        qml.invoke(&forged)
            .unwrap_err()
            .to_string()
            .contains("STALE_ACTION")
    );
    let operation = gpui.invoke(&action).unwrap();
    assert!(
        matches!(operation.presentation, Some(Presentation::Navigate { route: Route::Provider { ref provider_id } }) if provider_id == "themes")
    );
    let theme_view = gpui
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "",
            20,
        )
        .unwrap();
    assert!(
        theme_view
            .sections
            .iter()
            .flat_map(|s| &s.items)
            .any(|i| i.title == "Catppuccin Mocha")
    );
    let app_view = qml.query_view(&other_home.view_id, "safari").unwrap();
    assert!(
        app_view
            .sections
            .iter()
            .flat_map(|s| &s.items)
            .any(|i| i.title == "Safari")
    );
    gpui.close_view(&theme_view.view_id).unwrap();
    drop(gpui);
    assert!(
        qml.providers()
            .unwrap()
            .iter()
            .any(|p| p.id == "wallpapers")
    );
    let mut replacement = server.client();
    assert_eq!(replacement.hello().instance_id, qml.hello().instance_id);
    assert_eq!(
        replacement.open_view(Route::Home, "", 20).unwrap().sections[0]
            .items
            .len(),
        8
    );
}

#[test]
fn socket_is_private_and_a_second_server_cannot_replace_it() {
    let server = Running::start();
    let mode = std::fs::metadata(server.path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(Server::bind(server.path(), Service::fixture().unwrap()).is_err());
    assert_eq!(server.client().providers().unwrap().len(), 8);
}

#[test]
fn insecure_directory_and_existing_non_socket_are_not_modified() {
    let directory = tempfile::Builder::new()
        .prefix("mc-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(Server::bind(directory.path().join("rpc"), Service::fixture().unwrap()).is_err());
    assert_eq!(
        std::fs::metadata(directory.path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("rpc");
    std::fs::write(&path, b"do not delete").unwrap();
    assert!(Server::bind(&path, Service::fixture().unwrap()).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"do not delete");
}

#[test]
fn cleanup_does_not_delete_a_replacement_path() {
    let directory = tempfile::Builder::new()
        .prefix("mc-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = directory.path().join("rpc");
    let server = Server::bind(&path, Service::fixture().unwrap()).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    drop(server);
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
}

#[test]
fn malformed_or_oversized_frames_close_only_the_offending_connection() {
    let server = Running::start();
    for bytes in [vec![0xff], b"{bad json".to_vec()] {
        let mut socket = UnixStream::connect(server.path()).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        framing::write_frame(&mut socket, &bytes).unwrap();
        assert_eq!(socket.read(&mut [0u8; 1]).unwrap(), 0);
    }
    use std::io::Write;
    let mut socket = UnixStream::connect(server.path()).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .write_all(&((launcher_agent::protocol::MAX_FRAME_BYTES + 1) as u32).to_be_bytes())
        .unwrap();
    assert_eq!(socket.read(&mut [0u8; 1]).unwrap(), 0);
    assert_eq!(server.client().providers().unwrap().len(), 8);
}

#[test]
fn idle_clients_survive_but_partial_frames_have_a_deadline() {
    use std::io::Write;
    let server = Running::start();
    let mut client = server.client();
    let mut partial = UnixStream::connect(server.path()).unwrap();
    partial.write_all(&[0]).unwrap();
    partial
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    // Start the frame, then stop sending. Idle initialized clients are different.
    thread::sleep(Duration::from_secs(6));
    assert!(
        client.providers().is_ok(),
        "idle connections must not expire at the frame deadline"
    );
    assert_eq!(partial.read(&mut [0u8; 1]).unwrap(), 0);
}

#[test]
fn connection_count_is_bounded_and_disconnect_releases_a_slot() {
    let server = Running::start();
    let mut clients: Vec<_> = (0..launcher_agent::service::MAX_CLIENTS)
        .map(|_| server.client())
        .collect();
    assert!(Client::connect(server.path()).is_err());
    drop(clients.pop());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut replacement = loop {
        if let Ok(client) = Client::connect(server.path()) {
            break client;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "disconnection did not release a client slot"
        );
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(replacement.providers().unwrap().len(), 8);
}

#[test]
fn shutdown_disconnects_idle_clients() {
    let mut server = Running::start();
    let mut client = server.client();
    server.stop.store(true, Ordering::Release);
    server.thread.take().unwrap().join().unwrap().unwrap();
    assert!(client.providers().is_err());
    assert!(!server.path().exists());
}
