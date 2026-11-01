//! Actual theme provider over sockets. All helpers and published files are test-owned.
use launcher_client::{Client, RemoteError};
use launcher_core::{HistoryStore, ProviderConfig};
use launcher_protocol::{InvokeActionParams, Route, ViewSnapshot};
use launcher_service::{
    Server, Service, ThemeApply, ThemeCatalog, ThemeServiceProvider, ThemerState,
};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

struct Running {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
}
impl Running {
    fn start(body: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("mc-apply-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let mut active = ThemerState::fixture().snapshot().palette.unwrap();
        active.slug = "original".into();
        std::fs::write(
            state.join("current.json"),
            serde_json::to_vec(&active).unwrap(),
        )
        .unwrap();
        let mut palette = active;
        palette.slug = "owned-target".into();
        palette.name = "Owned Target".into();
        palette.palette.insert("accent".into(), "#123456".into());
        let helper = dir.path().join("helper");
        let script = format!(
            "#!/bin/sh\n[ \"$#\" = 5 ] && [ \"$1\" = apply ] && [ \"$2\" = owned-target ] && [ \"$3\" = --expected-palette ] && [ \"$5\" = --json ] || exit 9\nprintf 'apply\\n' >> \"${{0%/*}}/calls\"\n{body}\n"
        );
        std::fs::write(&helper, script).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let provider = Arc::new(ThemeServiceProvider::with_apply(
            ThemeCatalog::from_palettes(vec![palette]).unwrap(),
            ThemeApply::new(helper),
        ));
        let service = Service::with_state(
            vec![(provider, ProviderConfig::default())],
            Arc::new(HistoryStore::in_memory().unwrap()),
            None,
            ThemerState::watch(state),
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
    fn count(&self) -> usize {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap().unwrap();
    }
}
const SUCCESS: &str =
    "printf '{\"schema\":1,\"status\":\"applied\",\"theme\":\"owned-target\",\"warnings\":[]}'";
fn open(client: &mut Client) -> ViewSnapshot {
    client
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "owned",
            10,
        )
        .unwrap()
}
fn params(view: &ViewSnapshot, op: &str, confirmed: bool) -> InvokeActionParams {
    InvokeActionParams {
        view_id: view.view_id.clone(),
        revision: view.revision.clone(),
        action_ref: view.sections[0].items[0].actions[0].action_ref.clone(),
        operation_id: op.into(),
        confirmed,
    }
}
fn code(error: &anyhow::Error) -> &str {
    &error.downcast_ref::<RemoteError>().unwrap().0.data.code
}
#[track_caller]
fn wait(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(Instant::now() < deadline, "helper/state failed to progress");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn actual_theme_apply_requires_confirmation_executes_once_and_updates_independent_state() {
    let running = Running::start(&format!(
        "printf '%s' \"$4\" > \"${{0%/*}}/state/current.json\"\n{SUCCESS}"
    ));
    let mut client = running.client();
    let view = open(&mut client);
    let item = &view.sections[0].items[0];
    assert_eq!(item.actions.len(), 1);
    let prompt = item.actions[0].confirmation.as_ref().unwrap();
    assert!(prompt.contains("Owned Target") && prompt.contains("wallpaper"));
    assert_eq!(
        code(&client.invoke(&params(&view, "consent", false)).unwrap_err()),
        "CONFIRMATION_REQUIRED"
    );
    assert_eq!(running.count(), 0);
    let request = params(&view, "consent", true);
    let result = client.invoke(&request).unwrap();
    assert!(
        serde_json::to_string(&result)
            .unwrap()
            .contains("Wallpaper was not changed")
    );
    assert_eq!(
        serde_json::to_value(client.invoke(&request).unwrap()).unwrap(),
        serde_json::to_value(result).unwrap()
    );
    assert_eq!(running.count(), 1);
    // Updating active colors comes from the file reader, never from the preview.
    wait(|| {
        client
            .theme_state()
            .unwrap()
            .palette
            .is_some_and(|p| p.slug == "owned-target" && p.palette["accent"] == "#123456")
    });
    assert!(!running.dir.path().join("background").exists());
}

#[test]
fn accepted_apply_finishes_after_the_frontend_disconnects() {
    use launcher_protocol::{RpcRequest, RpcResponse, framing};
    use serde_json::json;
    use std::os::unix::net::UnixStream;
    let running = Running::start(&format!(
        "touch \"${{0%/*}}/started\"\nwhile [ ! -f \"${{0%/*}}/release\" ]; do /bin/sleep 0.02; done\ntouch \"${{0%/*}}/finished\"\n{SUCCESS}"
    ));
    let mut stream = UnixStream::connect(running.dir.path().join("rpc")).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let rpc = |stream: &mut UnixStream, request: RpcRequest| {
        framing::write_frame(stream, &serde_json::to_vec(&request).unwrap()).unwrap();
        let response: RpcResponse =
            serde_json::from_slice(&framing::read_frame(stream).unwrap()).unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.unwrap()
    };
    rpc(
        &mut stream,
        RpcRequest::new(
            "h",
            "hello",
            json!({"major":1,"minor":4,"clientName":"disconnect-test"}),
        ),
    );
    let view: ViewSnapshot = serde_json::from_value(rpc(
        &mut stream,
        RpcRequest::new(
            "v",
            "views.open",
            json!({"route":{"type":"provider","providerId":"themes"},"query":"owned"}),
        ),
    ))
    .unwrap();
    let request = RpcRequest::new(
        "a",
        "actions.invoke",
        serde_json::to_value(params(&view, "disconnected", true)).unwrap(),
    );
    framing::write_frame(&mut stream, &serde_json::to_vec(&request).unwrap()).unwrap();
    wait(|| running.dir.path().join("started").exists());
    drop(stream);
    let mut other = running.client();
    assert!(!other.providers().unwrap().is_empty());
    std::fs::write(running.dir.path().join("release"), b"").unwrap();
    wait(|| running.dir.path().join("finished").exists());
    assert_eq!(running.count(), 1);
    open(&mut other);
}

#[test]
fn provider_pins_the_search_catalog_revision_before_starting_a_helper() {
    use launcher_core::{
        Action, ActionId, CancellationToken, Provider, SearchContext, SearchQuery,
    };
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("helper");
    std::fs::write(
        &helper,
        format!("#!/bin/sh\ntouch \"$0.called\"\n{SUCCESS}\n"),
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut palette = ThemerState::fixture().snapshot().palette.unwrap();
    palette.slug = "owned-target".into();
    let catalog = ThemeCatalog::from_palettes(vec![palette]).unwrap();
    let provider =
        ThemeServiceProvider::with_apply(catalog.clone(), ThemeApply::new(helper.clone()));
    let mut item = provider
        .search(
            &SearchQuery {
                raw: String::new(),
                text: String::new(),
            },
            &SearchContext {
                generation: 1,
                cancellation: CancellationToken::default(),
                limit: 10,
            },
        )
        .unwrap()
        .remove(0);
    assert_eq!(
        item.payload["catalogRevision"].as_u64(),
        Some(catalog.snapshot().revision)
    );
    // Simulate the revision becoming obsolete after session-level validation.
    // The provider must check again before choosing the expected helper palette.
    item.payload["catalogRevision"] =
        serde_json::json!(catalog.snapshot().revision.wrapping_add(1));
    assert!(provider.actions(&item).is_empty());
    assert!(
        provider
            .activate(
                &item,
                &Action {
                    id: ActionId("apply".into()),
                    title: String::new()
                }
            )
            .is_err()
    );
    assert!(!helper.with_extension("called").exists());
}

#[test]
fn forged_and_obsolete_confirmed_actions_cannot_run_the_helper() {
    let running = Running::start(SUCCESS);
    let mut client = running.client();
    let view = open(&mut client);
    let mut forged = params(&view, "forged", true);
    forged.action_ref = "apply-owned-target".into();
    assert_eq!(code(&client.invoke(&forged).unwrap_err()), "STALE_ACTION");
    client.query_view(&view.view_id, "new query").unwrap();
    assert_eq!(
        code(&client.invoke(&params(&view, "old", true)).unwrap_err()),
        "STALE_ACTION"
    );
    assert_eq!(running.count(), 0);
}

#[test]
fn partial_failure_is_public_safe_and_retained_after_a_view_change() {
    let running = Running::start(
        "printf partial > \"${0%/*}/partial\"\nprintf '/private/secret/path' >&2\nexit 7",
    );
    let mut client = running.client();
    let view = open(&mut client);
    let request = params(&view, "partial", true);
    let first = client.invoke(&request).unwrap_err();
    assert_eq!(code(&first), "ACTION_FAILED");
    assert!(first.to_string().contains("partly applied"));
    assert!(!first.to_string().contains("/private/secret"));
    assert!(running.dir.path().join("partial").exists());
    client.query_view(&view.view_id, "something else").unwrap();
    assert_eq!(
        first.to_string(),
        client.invoke(&request).unwrap_err().to_string()
    );
    assert_eq!(running.count(), 1);
}

#[test]
fn apply_is_single_flight_across_clients_and_other_views_remain_responsive() {
    let running = Running::start(&format!(
        "touch \"${{0%/*}}/started\"\nwhile [ ! -f \"${{0%/*}}/release\" ]; do /bin/sleep 0.02; done\n{SUCCESS}"
    ));
    let mut first = running.client();
    let first_view = open(&mut first);
    let mut second = running.client();
    let second_view = open(&mut second);
    let work = thread::spawn(move || first.invoke(&params(&first_view, "first", true)));
    wait(|| running.dir.path().join("started").exists());
    let busy = second
        .invoke(&params(&second_view, "busy", true))
        .unwrap_err();
    assert!(busy.to_string().contains("already in progress"));
    let deadline = Instant::now();
    second.query_view(&second_view.view_id, "owned").unwrap();
    second.theme_state().unwrap();
    assert!(deadline.elapsed() < Duration::from_secs(2));
    assert_eq!(running.count(), 1);
    std::fs::write(running.dir.path().join("release"), b"").unwrap();
    work.join().unwrap().unwrap();
    // Busy is itself retained, not silently queued or retried after completion.
    assert_eq!(
        busy.to_string(),
        second
            .invoke(&params(&second_view, "busy", true))
            .unwrap_err()
            .to_string()
    );
    assert_eq!(running.count(), 1);
}
