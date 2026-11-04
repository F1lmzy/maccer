use anyhow::Result;
use launcher_agent::client::Client;
use launcher_agent::core::{
    Action, ActionId, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig,
    ProviderId, SearchContext, SearchQuery,
};
use launcher_agent::macos::ApplicationIconPixels;
use launcher_agent::protocol::{
    ContentBlock, DomainData, Icon, Route, THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS,
    THEME_TOKEN_KEYS,
};
use launcher_agent::service::{
    AssetStore, IconResolver, ItemPresentation, Server, Service, ServiceProvider, ThemerState,
};
use serde_json::{Value, json};
use std::{
    ffi::{CStr, CString},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

struct FixtureService {
    path: std::path::PathBuf,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}
impl FixtureService {
    fn start() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("maccer-agent-ipc-")
            .tempdir_in("/tmp")
            .unwrap()
            .keep();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("rpc");
        let server = Server::bind(&path, Service::fixture().unwrap()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let thread = thread::spawn(move || server.serve_until(&stop_thread).unwrap());
        Self {
            path,
            stop,
            thread: Some(thread),
        }
    }
    fn with_service(service: Arc<Service>) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("maccer-agent-ipc-")
            .tempdir_in("/tmp")
            .unwrap()
            .keep();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = dir.join("rpc");
        let server = Server::bind(&path, service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let thread = thread::spawn(move || server.serve_until(&stop_thread).unwrap());
        Self {
            path,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for FixtureService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        let _ = std::fs::remove_file(&self.path);
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
}

unsafe fn send(agent: *mut launcher_agent::agent::MaccerAgent, value: Value) -> Result<(), String> {
    let text = CString::new(value.to_string()).unwrap();
    let error = unsafe { launcher_agent::agent::maccer_agent_send(agent, text.as_ptr()) };
    if error.is_null() {
        Ok(())
    } else {
        let message = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        unsafe { launcher_agent::agent::maccer_agent_string_free(error) };
        Err(message)
    }
}
unsafe fn poll(agent: *mut launcher_agent::agent::MaccerAgent) -> Option<Value> {
    let event = unsafe { launcher_agent::agent::maccer_agent_poll(agent) };
    if event.is_null() {
        return None;
    }
    let value = serde_json::from_str(unsafe { CStr::from_ptr(event) }.to_str().unwrap()).unwrap();
    unsafe { launcher_agent::agent::maccer_agent_string_free(event) };
    Some(value)
}
fn assert_no_outcome(agent: *mut launcher_agent::agent::MaccerAgent, request: u64) {
    let deadline = Instant::now() + Duration::from_millis(40);
    while Instant::now() < deadline {
        if let Some(event) = unsafe { poll(agent) } {
            assert!(
                event["type"] != "outcome" || event["request"] != request,
                "superseded action outcome leaked: {event}"
            );
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

struct ReleaseOnDrop(mpsc::SyncSender<()>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

fn next_event(
    agent: *mut launcher_agent::agent::MaccerAgent,
    timeout: Duration,
    mut predicate: impl FnMut(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(event) = unsafe { poll(agent) }
            && predicate(&event)
        {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for agent event"
        );
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn connects_without_embedded_backend_searches_and_navigates_authoritatively() {
    let service = FixtureService::start();
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    assert!(
        !agent.is_null(),
        "connect constructor should return before IPC handshake"
    );
    next_event(agent, Duration::from_secs(2), |e| e["type"] == "ready");
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"thmes"})).unwrap();
    }
    let results = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 1
    });
    let theme = results["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "themes")
        .unwrap();
    assert!(theme["actions"][0]["id"].is_string());
    unsafe {
        send(agent, json!({"op":"activate","request":2,"search_request":1,"provider":theme["provider"],"id":theme["id"],"action":"open"})).unwrap();
    }
    let outcome = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "outcome" && e["request"] == 2
    });
    assert_eq!(outcome["kind"], "setQuery");
    assert_eq!(outcome["text"], "");
    assert_eq!(outcome["route"]["providerId"], "themes");
    unsafe {
        send(agent, json!({"op":"search","request":3,"query":""})).unwrap();
    }
    let scoped = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 3
    });
    assert_eq!(
        scoped["route"],
        json!({"type":"provider","providerId":"themes"})
    );
    assert!(
        scoped["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["title"] == "Catppuccin Mocha")
    );
    unsafe {
        send(agent, json!({"op":"home","request":4})).unwrap();
    }
    let home_outcome = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "outcome" && e["request"] == 4
    });
    assert_eq!(home_outcome["route"]["type"], "home");
    unsafe {
        send(agent, json!({"op":"search","request":5,"query":""})).unwrap();
    }
    let home = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 5
    });
    assert_eq!(home["route"]["type"], "home");
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
    let mut client = Client::connect(&service.path).unwrap();
    assert_eq!(
        client
            .open_view(Route::Home, "", 10)
            .unwrap()
            .sections
            .len(),
        1
    );
}

fn theme_document(accent: &str) -> Value {
    let colors = |keys: &[&str], color: &str| -> Value {
        Value::Object(
            keys.iter()
                .map(|key| ((*key).to_owned(), json!(color)))
                .collect(),
        )
    };
    let mut palette = colors(&THEME_PALETTE_KEYS, "#101112");
    palette["accent"] = json!(accent);
    json!({
        "schema": 1, "slug": "test-pack", "name": "Test Pack", "variant": "dark",
        "alpha_byte": 230,
        "palette": palette,
        "tokens": colors(&THEME_TOKEN_KEYS, "#202122"),
        "border": colors(&THEME_BORDER_KEYS, "#303132"),
        "bar": colors(&THEME_BAR_KEYS, "#404142"),
    })
}

#[test]
fn theme_state_is_delivered_without_a_search_and_shared_between_clients() {
    let service = FixtureService::start();
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let first = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    let ready = next_event(first, Duration::from_secs(3), |e| e["type"] == "ready");
    assert_eq!(ready["themeState"], true);
    // Delivered before any search request exists, so it is independent of views.
    let a = next_event(first, Duration::from_secs(3), |e| e["type"] == "themeState");
    assert_eq!(a["state"]["available"], true);
    assert_eq!(a["state"]["palette"]["schema"], 1);
    assert!(a["state"]["palette"]["tokens"]["primary"].is_string());
    let revision = a["state"]["revision"].as_str().unwrap().to_owned();

    let second = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(second, Duration::from_secs(3), |e| e["type"] == "ready");
    let b = next_event(second, Duration::from_secs(3), |e| {
        e["type"] == "themeState"
    });
    assert_eq!(b["state"]["revision"].as_str(), Some(revision.as_str()));
    assert_eq!(b["state"]["palette"], a["state"]["palette"]);

    // Cancel resets query state but must not surface an error or alter the palette.
    unsafe {
        send(first, json!({"op":"cancel"})).unwrap();
    }
    let deadline = Instant::now() + Duration::from_millis(150);
    while Instant::now() < deadline {
        if let Some(event) = unsafe { poll(first) } {
            assert_ne!(event["type"], "error", "cancel surfaced an error: {event}");
        } else {
            thread::sleep(Duration::from_millis(2));
        }
    }
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(first);
        launcher_agent::agent::maccer_agent_destroy(second);
    }
}

#[test]
fn theme_state_events_track_watcher_revisions_without_queries() {
    let dir = tempfile::Builder::new()
        .prefix("maccer-agent-theme-")
        .tempdir_in("/tmp")
        .unwrap();
    let file = dir.path().join("current.json");
    std::fs::write(
        &file,
        serde_json::to_vec(&theme_document("#7aa2f7")).unwrap(),
    )
    .unwrap();
    let service = Service::with_state(
        Vec::new(),
        Arc::new(HistoryStore::in_memory().unwrap()),
        None,
        ThemerState::watch(dir.path().to_path_buf()),
    )
    .unwrap();
    let server = FixtureService::with_service(service);
    let socket = CString::new(server.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    let ready = next_event(agent, Duration::from_secs(3), |e| e["type"] == "ready");
    assert_eq!(ready["themeState"], true);
    let first = next_event(agent, Duration::from_secs(3), |e| {
        e["type"] == "themeState" && e["state"]["available"] == true
    });
    let first_revision = first["state"]["revision"].as_str().unwrap().to_owned();
    assert_eq!(first["state"]["palette"]["palette"]["accent"], "#7aa2f7");
    // No search is ever sent; a replacement still reaches the frontend.
    std::fs::write(
        &file,
        serde_json::to_vec(&theme_document("#f7768e")).unwrap(),
    )
    .unwrap();
    let second = next_event(agent, Duration::from_secs(8), |e| {
        e["type"] == "themeState"
            && e["state"]["revision"].as_str() != Some(first_revision.as_str())
    });
    assert_eq!(second["state"]["palette"]["palette"]["accent"], "#f7768e");
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn failed_connect_is_reported_without_fallback_and_searches_are_session_local() {
    let missing = CString::new("/tmp/maccer-agent-ipc-definitely-not-present/rpc").unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(missing.as_ptr()) };
    assert!(!agent.is_null());
    let error = next_event(agent, Duration::from_secs(2), |e| e["type"] == "error");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("connecting to Maccer service")
    );
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"safari"})).unwrap();
    }
    let search_error = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "error" && e["request"] == 1
    });
    assert!(
        search_error["message"]
            .as_str()
            .unwrap()
            .contains("connecting to Maccer service")
    );
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn stale_activation_is_rejected_and_cancel_suppresses_pending_results() {
    let service = FixtureService::start();
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(agent, Duration::from_secs(2), |e| e["type"] == "ready");
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"safari"})).unwrap();
    }
    next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 1
    });
    unsafe {
        send(agent, json!({"op":"activate","request":2,"search_request":999,"provider":"apps","id":"safari","action":"open"})).unwrap();
    }
    let error = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "error" && e["request"] == 2
    });
    assert!(error["message"].as_str().unwrap().contains("stale"));
    unsafe {
        send(agent, json!({"op":"search","request":3,"query":"terminal"})).unwrap();
    }
    unsafe {
        send(agent, json!({"op":"cancel"})).unwrap();
    }
    thread::sleep(Duration::from_millis(30));
    while let Some(event) = unsafe { poll(agent) } {
        assert_ne!(event["request"], 3);
    }
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

struct AssetProvider {
    store: Arc<AssetStore>,
    activations: Arc<AtomicUsize>,
}
impl Provider for AssetProvider {
    fn id(&self) -> ProviderId {
        ProviderId("apps".into())
    }
    fn name(&self) -> &str {
        "Apps"
    }
    fn search(&self, query: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        if query.text == "fail" {
            anyhow::bail!("synthetic provider search failure");
        }
        Ok(vec![Item {
            id: ItemId("app-1".into()),
            provider: self.id(),
            title: "Fixture App".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: json!({"private":"/private/path"}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![Action {
            id: ActionId("open".into()),
            title: "Open".into(),
        }]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        self.activations.fetch_add(1, Ordering::SeqCst);
        Ok(ActionOutcome::Close)
    }
}
impl ServiceProvider for AssetProvider {
    fn description(&self) -> &str {
        "Synthetic app"
    }
    fn kind(&self) -> &str {
        "application"
    }
    fn details_available(&self, _: &Item) -> bool {
        true
    }
    fn details(&self, _: &Item) -> Result<Vec<ContentBlock>> {
        Ok(vec![ContentBlock::Image {
            asset: self.store.reference(Path::new("/private/detail.png"), 1),
            alt: "Synthetic preview".into(),
            fallback_text: "Synthetic preview image".into(),
        }])
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "application".into(),
            icon: Some(Icon::Asset {
                asset: self.store.reference(Path::new("/private/app.app"), 1),
                fallback_text: "A".into(),
            }),
            content: vec![ContentBlock::Text {
                text: "Synthetic".into(),
                truncated: false,
                fallback_text: "Synthetic".into(),
            }],
            data: Some(DomainData::Application {
                schema_version: 1,
                application_id: "app-1".into(),
            }),
        }
    }
}
struct BlockingIcons {
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
    blocked: AtomicBool,
}
impl IconResolver for BlockingIcons {
    fn resolve(&self, _: &Path) -> Result<Option<ApplicationIconPixels>> {
        if !self.blocked.swap(true, Ordering::SeqCst) {
            self.entered.send(()).unwrap();
            self.release.lock().unwrap().recv().unwrap();
        }
        Ok(Some(ApplicationIconPixels {
            bgra: vec![0, 0, 0, 255],
            width: 1,
            height: 1,
        }))
    }
}

struct FakeIcons;
impl IconResolver for FakeIcons {
    fn resolve(&self, _: &Path) -> Result<Option<ApplicationIconPixels>> {
        Ok(Some(ApplicationIconPixels {
            bgra: vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255],
            width: 2,
            height: 2,
        }))
    }
}

#[test]
fn provider_search_errors_are_preserved_in_legacy_results() {
    let assets = AssetStore::new(Arc::new(FakeIcons));
    let provider = Arc::new(AssetProvider {
        store: assets.clone(),
        activations: Arc::new(AtomicUsize::new(0)),
    });
    let service = Service::with_assets(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
        assets,
    )
    .unwrap();
    let service = FixtureService::with_service(service);
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "ready"
    });
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"fail"})).unwrap();
    }
    let results = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 1
    });
    assert!(results["errors"].as_array().unwrap().iter().any(|error| {
        error
            .as_str()
            .unwrap()
            .contains("apps: synthetic provider search failure")
    }));
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn queued_navigation_survives_a_new_search_while_icon_fetch_is_blocked() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let assets = AssetStore::new(Arc::new(BlockingIcons {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        blocked: AtomicBool::new(false),
    }));
    let activations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AssetProvider {
        store: assets.clone(),
        activations: activations.clone(),
    });
    let service = Service::with_assets(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
        assets,
    )
    .unwrap();
    let service = FixtureService::with_service(service);
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "ready"
    });
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"apps"})).unwrap();
    }
    let results = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 1
    });
    assert!(
        results["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "menu" && item["id"] == "apps")
    );
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("actor is blocked fetching the app icon");

    // The action is valid for the last published snapshot. A newer query is
    // accepted while the actor is blocked on the unrelated, lazy asset request.
    unsafe {
        send(agent, json!({"op":"activate","request":2,"search_request":1,"provider":"menu","id":"apps","action":"open"})).unwrap();
        send(agent, json!({"op":"search","request":3,"query":""})).unwrap();
    }
    release_tx.send(()).unwrap();
    let scoped = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 3
    });
    assert_eq!(
        scoped["route"],
        json!({"type":"provider","providerId":"apps"})
    );
    assert!(
        scoped["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "app-1")
    );
    assert_eq!(
        activations.load(Ordering::SeqCst),
        0,
        "menu navigation must not activate an app"
    );
    assert_no_outcome(agent, 2);
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn queued_action_executes_once_when_a_new_query_supersedes_its_outcome() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let assets = AssetStore::new(Arc::new(BlockingIcons {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        blocked: AtomicBool::new(false),
    }));
    let activations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AssetProvider {
        store: assets.clone(),
        activations: activations.clone(),
    });
    let service = Service::with_assets(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
        assets,
    )
    .unwrap();
    let service = FixtureService::with_service(service);
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "ready"
    });
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"apps"})).unwrap();
    }
    let results = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 1
    });
    assert!(
        results["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "apps" && item["id"] == "app-1")
    );
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("actor is blocked fetching the app icon");

    unsafe {
        send(agent, json!({"op":"activate","request":2,"search_request":1,"provider":"apps","id":"app-1","action":"open"})).unwrap();
        send(agent, json!({"op":"search","request":3,"query":"apps"})).unwrap();
    }
    release_tx.send(()).unwrap();
    let latest = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 3
    });
    assert_eq!(latest["route"]["type"], "home");
    assert_eq!(
        activations.load(Ordering::SeqCst),
        1,
        "accepted action must execute exactly once"
    );
    assert_no_outcome(agent, 2);
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn service_asset_pixels_populate_the_existing_cache_only_icon_abi() {
    let assets = AssetStore::new(Arc::new(FakeIcons));
    let provider = Arc::new(AssetProvider {
        store: assets.clone(),
        activations: Arc::new(AtomicUsize::new(0)),
    });
    let service = Service::with_assets(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
        assets,
    )
    .unwrap();
    let service = FixtureService::with_service(service);
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(agent, Duration::from_secs(2), |e| e["type"] == "ready");
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":""})).unwrap();
    }
    let home = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 1
    });
    let apps = home["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "apps")
        .unwrap();
    unsafe {
        send(agent, json!({"op":"activate","request":2,"search_request":1,"provider":"menu","id":"apps","action":"open"})).unwrap();
    }
    next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "outcome" && e["request"] == 2
    });
    assert_eq!(apps["id"], "apps");
    unsafe {
        send(agent, json!({"op":"search","request":3,"query":""})).unwrap();
    }
    let results = next_event(agent, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 3
    });
    assert_eq!(results["items"][0]["iconSource"], "");
    let app = results["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "apps")
        .unwrap();
    unsafe {
        send(
            agent,
            json!({"op":"details","request":1,"search_request":3,"provider":"apps","id":app["id"]}),
        )
        .unwrap();
    }
    let details = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "details" && event["detailRequest"] == 1
    });
    let preview_source = details["content"][0]["source"].as_str().unwrap();
    assert!(preview_source.starts_with("image://maccer-icons/"));
    let token = preview_source
        .strip_prefix("image://maccer-icons/")
        .unwrap();
    let token = CString::new(token).unwrap();
    let pixels = unsafe { launcher_agent::agent::maccer_agent_icon(agent, token.as_ptr()) };
    assert!(!pixels.is_null());
    let pixels = unsafe { &*pixels };
    assert_eq!((pixels.width, pixels.height), (2, 2));
    assert_eq!(
        unsafe { std::slice::from_raw_parts(pixels.bgra, pixels.len) },
        &[1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
    );
    unsafe {
        launcher_agent::agent::maccer_agent_icon_free(pixels as *const _ as *mut _);
    }
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn separate_frontend_handles_keep_independent_service_sessions() {
    let service = FixtureService::start();
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let first = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    let second = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(first, Duration::from_secs(2), |e| e["type"] == "ready");
    next_event(second, Duration::from_secs(2), |e| e["type"] == "ready");
    unsafe {
        send(first, json!({"op":"search","request":1,"query":"thmes"})).unwrap();
    }
    unsafe {
        send(second, json!({"op":"search","request":1,"query":"safari"})).unwrap();
    }
    let a = next_event(first, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 1
    });
    let b = next_event(second, Duration::from_secs(2), |e| {
        e["type"] == "results" && e["request"] == 1
    });
    assert!(
        a["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == "themes")
    );
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["title"] == "Safari")
    );
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(first);
    }
    unsafe {
        send(
            second,
            json!({"op":"search","request":2,"query":"terminal"}),
        )
        .unwrap();
    }
    assert!(
        next_event(second, Duration::from_secs(2), |e| e["type"] == "results"
            && e["request"] == 2)["items"]
            .is_array()
    );
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(second);
    }
}

struct BlockingDetailsProvider {
    entered: mpsc::SyncSender<()>,
    release: Mutex<mpsc::Receiver<()>>,
}
impl Provider for BlockingDetailsProvider {
    fn id(&self) -> ProviderId {
        ProviderId("preview".into())
    }
    fn name(&self) -> &str {
        "Preview"
    }
    fn search(&self, _: &SearchQuery, context: &SearchContext) -> Result<Vec<Item>> {
        if context.cancellation.is_cancelled() || context.limit == 0 {
            return Ok(vec![]);
        }
        Ok(vec![Item {
            id: ItemId("entry".into()),
            provider: self.id(),
            title: "Preview entry".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: json!({"private":"never"}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![Action {
            id: ActionId("open".into()),
            title: "Open".into(),
        }]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        Ok(ActionOutcome::Close)
    }
}
impl ServiceProvider for BlockingDetailsProvider {
    fn description(&self) -> &str {
        "A detail race fixture"
    }
    fn kind(&self) -> &str {
        "fixture"
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "fixture".into(),
            icon: None,
            content: vec![],
            data: None,
        }
    }
    fn details_available(&self, _: &Item) -> bool {
        true
    }
    fn details(&self, _: &Item) -> Result<Vec<ContentBlock>> {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv().unwrap();
        Ok(vec![ContentBlock::Text {
            text: "current detail".into(),
            truncated: false,
            fallback_text: "current detail".into(),
        }])
    }
}

#[test]
fn newer_detail_selection_suppresses_blocked_details_without_consuming_search_id() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let provider = Arc::new(BlockingDetailsProvider {
        entered: entered_tx,
        release: Mutex::new(release_rx),
    });
    let service = Service::new(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
    )
    .unwrap();
    let service = FixtureService::with_service(service);
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    let _release_on_drop = ReleaseOnDrop(release_tx.clone());
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "ready"
    });
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"Preview"})).unwrap();
    }
    let home = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 1
    });
    assert!(
        home["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["provider"] == "preview")
    );
    unsafe {
        send(agent, json!({"op":"activate","request":2,"search_request":1,"provider":"menu","id":"preview","action":"open"})).unwrap();
    }
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "outcome" && event["request"] == 2
    });
    unsafe {
        send(agent, json!({"op":"search","request":3,"query":""})).unwrap();
    }
    let scoped = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 3
    });
    let item = scoped["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "preview")
        .unwrap();
    unsafe {
        send(agent, json!({"op":"details","request":1,"search_request":3,"provider":"preview","id":item["id"]})).unwrap();
    }
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first details request is blocked");
    unsafe {
        send(agent, json!({"op":"details","request":2,"search_request":3,"provider":"preview","id":item["id"]})).unwrap();
    }
    release_tx.send(()).unwrap();
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("newest details request reaches provider");
    unsafe {
        send(agent, json!({"op":"search","request":4,"query":"absent"})).unwrap();
    }
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    let updated = loop {
        if let Some(event) = unsafe { poll(agent) } {
            assert_ne!(
                event["type"], "details",
                "query change suppresses details for previous selection"
            );
            if event["type"] == "results" && event["request"] == 4 {
                break event;
            }
        } else {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for refreshed results"
        );
    };
    assert_eq!(updated["request"], 4);
    let deadline = Instant::now() + Duration::from_millis(50);
    while Instant::now() < deadline {
        if let Some(event) = unsafe { poll(agent) } {
            assert_ne!(
                event["type"], "details",
                "query change suppresses details for previous selection"
            );
        } else {
            thread::sleep(Duration::from_millis(1));
        }
    }
    unsafe {
        send(agent, json!({"op":"search","request":5,"query":""})).unwrap();
    }
    let current_view = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 5
    });
    let item = current_view["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "preview")
        .unwrap();
    unsafe {
        send(agent, json!({"op":"details","request":3,"search_request":5,"provider":"preview","id":item["id"]})).unwrap();
    }
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("current selection details reaches provider");
    release_tx.send(()).unwrap();
    let current = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "details"
    });
    assert_eq!(current["detailRequest"], 3);
    assert_eq!(
        current["request"], 5,
        "detail ids remain independent from main search ids"
    );
    assert_eq!(current["content"][0]["text"], "current detail");
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn service_details_use_independent_request_ids_and_preserve_action_authority() {
    let service = FixtureService::start();
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    let ready = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "ready"
    });
    assert_eq!(ready["itemDetails"], true);
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"themes"})).unwrap();
    }
    let home = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 1
    });
    assert!(
        home["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "themes")
    );
    unsafe {
        send(agent, json!({"op":"activate","request":2,"search_request":1,"provider":"menu","id":"themes","action":"open"})).unwrap();
    }
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "outcome" && event["request"] == 2
    });
    unsafe {
        send(agent, json!({"op":"search","request":3,"query":""})).unwrap();
    }
    let scoped = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 3
    });
    let result = scoped["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "themes")
        .unwrap();
    assert_eq!(result["detailsAvailable"], true);
    unsafe {
        send(agent, json!({"op":"details","request":1,"search_request":3,"provider":"themes","id":result["id"]})).unwrap();
    }
    let details = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "details" && event["detailRequest"] == 1
    });
    assert_eq!(
        details["request"], 3,
        "top-level request correlates to displayed result snapshot"
    );
    assert_eq!(details["provider"], "themes");
    assert!(
        details["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|block| block["type"] == "colorPalette")
    );
    unsafe {
        send(agent, json!({"op":"details","request":2,"search_request":3,"provider":"themes","id":result["id"]})).unwrap();
    }
    let second = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "details" && event["detailRequest"] == 2
    });
    assert_eq!(second["request"], 3);
    unsafe {
        send(agent, json!({"op":"activate","request":4,"search_request":3,"provider":result["provider"],"id":result["id"],"action":"select"})).unwrap();
    }
    let outcome = next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "outcome" && event["request"] == 4
    });
    assert_eq!(
        outcome["kind"], "keepOpen",
        "details did not consume the main request sequence or action authority"
    );
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn expired_icon_emits_one_authorized_refresh_and_does_not_retry_mutation() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let assets = AssetStore::new(Arc::new(BlockingIcons {
        entered: entered_tx,
        release: Mutex::new(release_rx),
        blocked: AtomicBool::new(false),
    }));
    let activations = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(AssetProvider {
        store: assets.clone(),
        activations: activations.clone(),
    });
    let service = Service::with_assets(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
        assets.clone(),
    )
    .unwrap();
    let service = FixtureService::with_service(service);
    let socket = CString::new(service.path.to_str().unwrap()).unwrap();
    let agent = unsafe { launcher_agent::agent::maccer_agent_connect(socket.as_ptr()) };
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "ready"
    });
    unsafe {
        send(agent, json!({"op":"search","request":1,"query":"apps"})).unwrap();
    }
    next_event(agent, Duration::from_secs(2), |event| {
        event["type"] == "results" && event["request"] == 1
    });
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("icon resolution blocks on the test resolver");
    for index in 0..256 {
        assets.reference(&Path::new("/private/eviction").join(index.to_string()), 2);
    }
    release_tx.send(()).unwrap();
    let expired = next_event(agent, Duration::from_secs(3), |event| {
        event["type"] == "assetExpired"
    });
    assert_eq!(expired["request"], 1);
    unsafe {
        send(
            agent,
            json!({"op":"refresh","request":2,"search_request":1}),
        )
        .unwrap();
    }
    let refreshed = next_event(agent, Duration::from_secs(3), |event| {
        event["type"] == "results" && event["request"] == 2
    });
    assert_eq!(refreshed["recovery"], true);
    next_event(agent, Duration::from_secs(3), |event| {
        event["type"] == "icon" && event["request"] == 2
    });
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        if let Some(event) = unsafe { poll(agent) } {
            assert_ne!(
                event["type"], "assetExpired",
                "recovery snapshot cannot trigger another automatic refresh"
            );
        } else {
            thread::sleep(Duration::from_millis(2));
        }
    }
    assert_eq!(
        activations.load(Ordering::SeqCst),
        0,
        "asset recovery never invokes provider actions"
    );
    unsafe {
        launcher_agent::agent::maccer_agent_destroy(agent);
    }
}

#[test]
fn invalid_socket_arguments_fail_without_starting_a_worker() {
    assert!(unsafe { launcher_agent::agent::maccer_agent_connect(std::ptr::null()) }.is_null());
    let relative = CString::new("relative.sock").unwrap();
    assert!(unsafe { launcher_agent::agent::maccer_agent_connect(relative.as_ptr()) }.is_null());
}
