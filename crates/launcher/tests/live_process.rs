#[cfg(target_os = "macos")]
use launcher_agent::client::Client;
#[cfg(target_os = "macos")]
use launcher_agent::protocol::Route;
use std::process::Command;
#[cfg(target_os = "macos")]
use std::{
    os::unix::fs::PermissionsExt,
    process::{Child, Stdio},
    thread,
    time::{Duration, Instant},
};

#[test]
fn live_and_fixture_modes_cannot_be_combined() {
    let output = Command::new(env!("CARGO_BIN_EXE_maccer-service"))
        .args(["--fixture", "--apps", "--socket", "/not/a/socket"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cannot combine"));
}

#[test]
fn fixture_mode_never_loads_live_configuration_or_history() {
    let output = Command::new(env!("CARGO_BIN_EXE_maccer-service"))
        .args([
            "--fixture",
            "--config",
            "/not/a/config",
            "--socket",
            "/not/a/socket",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("only available with --apps"));
}

#[cfg(target_os = "macos")]
struct Process(Child);
#[cfg(target_os = "macos")]
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// A live native host with the provider disabled does not scan personal applications
// or activate anything. All configuration and database files belong to this test.
#[cfg(target_os = "macos")]
#[test]
fn native_host_starts_with_owned_configuration_and_history_then_stops_cleanly() {
    let dir = tempfile::Builder::new()
        .prefix("mc-live-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[providers.apps]\nenabled = false\n[providers.themes]\nenabled = false\n[providers.wallpapers]\nenabled = false\n[providers.web]\nenabled = false\n[providers.clipboard]\nenabled = false\n",
    )
    .unwrap();
    let history = dir.path().join("history.sqlite3");
    let socket = dir.path().join("rpc");
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_maccer-service"))
            .arg("--apps")
            .arg("--config")
            .arg(&config)
            .arg("--history")
            .arg(&history)
            .arg("--socket")
            .arg(&socket)
            .env("MACTHEMER_STATE", dir.path().join("theme-state"))
            .env("MACTHEMER_HOME", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut client = loop {
        if let Ok(client) = Client::connect(&socket) {
            break client;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "native host exited before accepting client"
        );
        assert!(Instant::now() < deadline, "native host failed to start");
        thread::sleep(Duration::from_millis(10));
    };
    assert!(client.providers().unwrap().is_empty());
    assert!(
        client
            .open_view(Route::Home, "", 10)
            .unwrap()
            .sections
            .is_empty()
    );
    assert!(history.is_file());
    drop(client);
    assert!(process.0.try_wait().unwrap().is_none());
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
        assert!(Instant::now() < deadline, "native host did not stop");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists());
}
