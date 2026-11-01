use launcher_client::Client;
use launcher_protocol::Route;
use std::{
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn standalone_process_outlives_frontend_connections_and_shuts_down_cleanly() {
    let directory = tempfile::Builder::new()
        .prefix("mc-process-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = directory.path().join("rpc");
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_maccer-service"))
            .arg("--fixture")
            .arg("--socket")
            .arg(&socket)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut first = loop {
        if let Ok(client) = Client::connect(&socket) {
            break client;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "service exited before accepting a client"
        );
        assert!(Instant::now() < deadline, "service never became ready");
        thread::sleep(Duration::from_millis(10));
    };
    let instance = first.hello().instance_id.clone();
    let view = first.open_view(Route::Home, "", 20).unwrap();
    assert_eq!(view.sections[0].items.len(), 8);
    drop(first);
    let mut replacement = Client::connect(&socket).unwrap();
    assert_eq!(replacement.hello().instance_id, instance);
    let view = replacement.open_view(Route::Home, "thmes", 20).unwrap();
    assert!(
        view.sections
            .iter()
            .flat_map(|s| &s.items)
            .any(|i| i.id == "themes")
    );
    drop(replacement);
    assert!(
        process.0.try_wait().unwrap().is_none(),
        "frontend teardown must not terminate service"
    );
    // Signal only the child owned by this test. Normal shutdown removes its socket.
    assert_eq!(
        unsafe { libc::kill(process.0.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "service did not stop after SIGTERM"
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists());
}

#[test]
fn fixture_rejects_frontend_routing_without_initializing_native_hotkey() {
    for arguments in [
        vec!["--frontend", "gpui"],
        vec!["--frontend", "gpui", "--frontend-path", "/tmp/maccer"],
        vec!["--frontend-path", "/tmp/maccer"],
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_maccer-service"));
        command
            .arg("--fixture")
            .arg("--socket")
            .arg("/tmp/maccer-unused.sock");
        command.args(arguments);
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--frontend") || stderr.contains("--frontend-path"),
            "{stderr}"
        );
    }
}

#[cfg(target_os = "macos")]
#[test]
fn live_frontend_selection_rejects_invalid_targets_before_native_registration() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[providers.apps]\nenabled=false\n[providers.themes]\nenabled=false\n[providers.web]\nenabled = false\n[providers.clipboard]\nenabled = false\n",
    )
    .unwrap();
    for (kind, path, expected) in [
        ("unknown", "/missing/frontend", "kind must be"),
        ("gpui", "relative/frontend", "must be absolute"),
        ("gpui", "/missing/frontend", "reading configured frontend"),
        ("pill", "/missing/Pill.app", "reading configured frontend"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_maccer-service"))
            .args(["--apps", "--config"])
            .arg(&config)
            .arg("--socket")
            .arg(dir.path().join("rpc"))
            .args(["--frontend", kind, "--frontend-path", path])
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(!dir.path().join("rpc").exists());
    }
}

#[test]
fn service_refuses_to_start_without_an_explicit_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_maccer-service"))
        .arg("--socket")
        .arg("/not/a/runtime/socket")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("choose --fixture or --apps"));
}
