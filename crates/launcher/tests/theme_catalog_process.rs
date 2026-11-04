#![cfg(target_os = "macos")]
//! Exercise the real live daemon, but only with an owned read-only helper.
use launcher_agent::client::Client;
use launcher_agent::protocol::{ContentBlock, GetDetailsParams, Route};
use launcher_agent::service::ThemerState;
use std::{
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Process(std::process::Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn live_catalog_uses_configured_helper_and_leaves_active_palette_unchanged() {
    let dir = tempfile::Builder::new()
        .prefix("mc-cproc-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let bin = dir.path().join("helper bin");
    std::fs::create_dir(&bin).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir(&state).unwrap();
    let mut active = ThemerState::fixture().snapshot().palette.unwrap();
    active.slug = "active-original".into();
    std::fs::write(
        state.join("current.json"),
        serde_json::to_vec(&active).unwrap(),
    )
    .unwrap();
    let mut preview = active.clone();
    preview.slug = "preview-only".into();
    preview.name = "Owned Catalog Preview".into();
    preview.palette.insert("accent".into(), "#123456".into());
    std::fs::write(
        dir.path().join("catalog.json"),
        serde_json::to_vec(&serde_json::json!({"schema":1,"themes":[preview]})).unwrap(),
    )
    .unwrap();
    let helper = bin.join("mac-themer");
    std::fs::write(&helper, "#!/bin/sh\n[ \"$#\" = 2 ] && [ \"$1\" = catalog ] && [ \"$2\" = --json ] || exit 9\nprintf '%s' \"$PATH\" > \"$MACTHEMER_RUN/path\"\nprintf 'catalog\\n' >> \"$MACTHEMER_RUN/calls\"\n/bin/cat \"$MACTHEMER_RUN/catalog.json\"\n").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[providers.apps]\nenabled = false\n[providers.themes]\nmax_results = 7\n[providers.web]\nenabled = false\n[providers.clipboard]\nenabled = false\n",
    )
    .unwrap();
    let socket = dir.path().join("rpc");
    let mut process = Process(
        Command::new(env!("CARGO_BIN_EXE_maccer-service"))
            .arg("--apps")
            .arg("--config")
            .arg(config)
            .arg("--history")
            .arg(dir.path().join("history.sqlite3"))
            .arg("--socket")
            .arg(&socket)
            .env("MACTHEMER_BIN", &bin)
            .env("MACTHEMER_RUN", dir.path())
            .env("MACTHEMER_STATE", &state)
            .env("MACTHEMER_HOME", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut client = loop {
        if let Ok(client) = Client::connect(&socket) {
            break client;
        }
        assert!(process.0.try_wait().unwrap().is_none());
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    };
    assert!(client.providers().unwrap().iter().any(|p| p.id == "themes"));
    let view = client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "owned",
            10,
        )
        .unwrap();
    let item = &view.sections[0].items[0];
    assert_eq!(item.title, "Owned Catalog Preview");
    assert_eq!(item.actions.len(), 1);
    assert!(
        item.actions[0]
            .confirmation
            .as_deref()
            .unwrap()
            .contains("wallpaper")
    );
    let unconfirmed = launcher_agent::protocol::InvokeActionParams {
        view_id: view.view_id.clone(),
        revision: view.revision.clone(),
        action_ref: item.actions[0].action_ref.clone(),
        operation_id: "unconfirmed".into(),
        confirmed: false,
    };
    let error = client.invoke(&unconfirmed).unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<launcher_agent::client::RemoteError>()
            .unwrap()
            .0
            .data
            .code,
        "CONFIRMATION_REQUIRED"
    );
    let details = client
        .get_details(&GetDetailsParams {
            view_id: view.view_id,
            revision: view.revision,
            provider_id: "themes".into(),
            item_id: item.id.clone(),
        })
        .unwrap();
    assert!(details.content.iter().any(|b| matches!(b, ContentBlock::ColorPalette{colors,..} if colors.iter().any(|c| c.value == "#123456"))));
    assert_eq!(client.theme_state().unwrap().palette, Some(active));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calls")).unwrap(),
        "catalog\n"
    );
    assert!(
        std::fs::read_to_string(dir.path().join("path"))
            .unwrap()
            .starts_with("/opt/homebrew/bin:/usr/local/bin:")
    );
    drop(client);
    assert!(process.0.try_wait().unwrap().is_none());
    assert_eq!(
        unsafe { libc::kill(process.0.id() as i32, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert!(!socket.exists());
}
