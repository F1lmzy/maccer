use launcher_agent::client::Client;
use launcher_agent::service::{Server, Service};
use std::{
    os::unix::fs::PermissionsExt,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

struct Fixture {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_pill_selected(false)
    }
    fn with_pill_selected(pill: bool) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mc-gpui-mode-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let service = Service::fixture().unwrap();
        if pill {
            let target = dir.path().join("Owned.app");
            std::fs::create_dir(&target).unwrap();
            service
                .attach_frontend(
                    launcher_agent::service::FrontendRouter::new(
                        launcher_agent::protocol::FrontendKind::Pill,
                        target,
                        dir.path().join("rpc"),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = Some(std::thread::spawn(move || server.serve_until(&worker_stop)));
        Self { dir, stop, worker }
    }
    fn socket(&self) -> std::path::PathBuf {
        self.dir.path().join("rpc")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}

#[test]
fn headless_service_probe_ignores_personal_config_and_keeps_backend_alive() {
    let fixture = Fixture::new();
    let home = fixture.dir.path().join("home");
    let personal = home.join(".config/maccer/config.toml");
    std::fs::create_dir_all(personal.parent().unwrap()).unwrap();
    std::fs::write(&personal, "this is deliberately not valid TOML").unwrap();
    let sentinel = home.join("history-sentinel");
    std::fs::write(&sentinel, "unchanged").unwrap();
    let before = Client::connect(fixture.socket())
        .unwrap()
        .hello()
        .instance_id
        .clone();

    for explicit in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_maccer"));
        command
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("MACCER_SERVICE_SOCKET", fixture.socket())
            .env("RUST_LOG", "error")
            .arg("--check");
        if explicit {
            command.arg("--service-socket").arg(fixture.socket());
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains(&format!("service instance: {before}")));
        assert!(stdout.contains("providers available: 8"));
    }
    assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "unchanged");
    assert_eq!(
        std::fs::read_to_string(&personal).unwrap(),
        "this is deliberately not valid TOML"
    );
    assert!(!home.join("Library/Application Support/maccer").exists());
    let after = Client::connect(fixture.socket())
        .unwrap()
        .hello()
        .instance_id
        .clone();
    assert_eq!(before, after);
}

#[test]
fn missing_service_probe_fails_without_fallback_or_creating_socket() {
    let dir = tempfile::Builder::new()
        .prefix("mc-gpui-missing-")
        .tempdir_in("/tmp")
        .unwrap();
    let socket = dir.path().join("rpc");
    let output = Command::new(env!("CARGO_BIN_EXE_maccer"))
        .env("MACCER_SERVICE_SOCKET", &socket)
        .arg("--check")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no backend was started"));
    assert!(!socket.exists());
}

#[test]
fn pill_selection_refuses_gpui_startup_without_opening_a_window() {
    let fixture = Fixture::with_pill_selected(true);
    let output = Command::new(env!("CARGO_BIN_EXE_maccer"))
        .args([
            "--service-socket",
            fixture.socket().to_str().unwrap(),
            "--show",
        ])
        .env("RUST_LOG", "warn")
        .output()
        .unwrap();
    assert!(output.status.success()); // A deliberate startup refusal, not a modal error.
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostics.contains("not the selected frontend"),
        "{diagnostics}"
    );
    assert!(
        launcher_agent::macos::FrontendInstance::claim(&fixture.socket())
            .unwrap()
            .is_some()
    );
    assert_eq!(
        Client::connect(fixture.socket())
            .unwrap()
            .providers()
            .unwrap()
            .len(),
        8
    );
}

#[test]
fn configured_service_prevents_an_accidental_embedded_gpui_startup() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[service.frontend]\nkind='pill'\npath='/owned/Pill.app'\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maccer"))
        .env_remove("MACCER_SERVICE_SOCKET")
        .args(["--config", config.to_str().unwrap(), "--show"])
        .env("RUST_LOG", "warn")
        .output()
        .unwrap();
    assert!(output.status.success());
    let diagnostics = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        diagnostics.contains("instead of starting an embedded GPUI"),
        "{diagnostics}"
    );
}

#[test]
fn environment_selected_service_mode_rejects_frontend_provider_config() {
    let dir = tempfile::Builder::new()
        .prefix("mc-gpui-config-")
        .tempdir_in("/tmp")
        .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_maccer"))
        .env("MACCER_SERVICE_SOCKET", dir.path().join("rpc"))
        .args(["--check", "--config", "/does/not/exist"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--config belongs to the backend"));
}
