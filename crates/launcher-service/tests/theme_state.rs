//! Actual-socket tests for the read-only active theme state checkpoint.
//!
//! Every test uses synthetic palette documents in a temp directory; none read
//! or write the user's real `current.json` or invoke a mac-themer helper.
use launcher_client::Client;
use launcher_core::HistoryStore;
use launcher_protocol::{
    THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS, ThemeState,
};
use launcher_service::{Server, Service, ThemerState};
use serde_json::Value;
use std::{
    path::Path,
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
    fn new(service: Arc<Service>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("mc-theme-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
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

fn themed_service(theme: Arc<ThemerState>) -> Arc<Service> {
    Service::with_state(
        Vec::new(),
        Arc::new(HistoryStore::in_memory().unwrap()),
        None,
        theme,
    )
    .unwrap()
}

fn color_object(keys: &[&str], color: &str) -> Value {
    Value::Object(
        keys.iter()
            .map(|key| ((*key).to_owned(), serde_json::json!(color)))
            .collect(),
    )
}

fn document(accent: &str) -> Value {
    let mut palette = color_object(&THEME_PALETTE_KEYS, "#101112");
    palette["accent"] = serde_json::json!(accent);
    serde_json::json!({
        "schema": 1,
        "generated_by": "mac-themer test",
        "slug": "test-pack",
        "name": "Test Pack",
        "variant": "dark",
        "alpha_byte": 230,
        "pack": "themes/test-pack",
        "backgrounds": "themes/test-pack/backgrounds",
        "palette": palette,
        "tokens": color_object(&THEME_TOKEN_KEYS, "#202122"),
        "border": color_object(&THEME_BORDER_KEYS, "#303132"),
        "bar": color_object(&THEME_BAR_KEYS, "#404142"),
    })
}

fn write_atomic(path: &Path, value: &Value) {
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, serde_json::to_vec(value).unwrap()).unwrap();
    std::fs::rename(&temporary, path).unwrap();
}

#[track_caller]
fn wait_for(client: &mut Client, predicate: impl Fn(&ThemeState) -> bool) -> ThemeState {
    // Native FSEvents registration can take several seconds on loaded hosts.
    // Keep this an eventual-delivery check, not a filesystem latency benchmark.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state = client.theme_state().unwrap();
        if predicate(&state) {
            return state;
        }
        assert!(Instant::now() < deadline, "theme state timeout: {state:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn two_clients_share_theme_state_without_view_or_action_interference() {
    let dir = tempfile::Builder::new()
        .prefix("mc-theme-live-")
        .tempdir_in("/tmp")
        .unwrap();
    write_atomic(&dir.path().join("current.json"), &document("#7aa2f7"));
    let running = Running::new(themed_service(ThemerState::watch(dir.path().to_path_buf())));
    let mut first = running.client();
    let mut second = running.client();
    assert!(
        first
            .hello()
            .capabilities
            .iter()
            .any(|capability| capability == "themeState")
    );
    let a = wait_for(&mut first, |state| state.available);
    let b = second.theme_state().unwrap();
    assert_eq!(a.revision, b.revision);
    assert_eq!(a.palette, b.palette);
    assert_eq!(
        a.palette.as_ref().unwrap().palette.get("accent").unwrap(),
        "#7aa2f7"
    );
    // A view/search/close on one session must not disturb shared state.
    let view = first
        .open_view(launcher_protocol::Route::Home, "", 5)
        .unwrap();
    first.query_view(&view.view_id, "anything").unwrap();
    first.close_view(&view.view_id).unwrap();
    let after = second.theme_state().unwrap();
    assert_eq!(after.revision, a.revision);
    assert_eq!(after.palette, a.palette);
}

#[test]
fn identical_rewrite_does_not_churn_revision_and_replacements_do() {
    let dir = tempfile::Builder::new()
        .prefix("mc-theme-rewrite-")
        .tempdir_in("/tmp")
        .unwrap();
    let file = dir.path().join("current.json");
    write_atomic(&file, &document("#7aa2f7"));
    let running = Running::new(themed_service(ThemerState::watch(dir.path().to_path_buf())));
    let mut client = running.client();
    let initial = wait_for(&mut client, |state| state.available);
    // Rewrite identical content plus unrelated metadata: revision is unchanged.
    write_atomic(&file, &document("#7aa2f7"));
    thread::sleep(Duration::from_millis(1200));
    assert_eq!(client.theme_state().unwrap().revision, initial.revision);
    // A different color changes the revision.
    write_atomic(&file, &document("#f7768e"));
    let changed = wait_for(&mut client, |state| state.revision != initial.revision);
    assert_eq!(
        changed
            .palette
            .as_ref()
            .unwrap()
            .palette
            .get("accent")
            .unwrap(),
        "#f7768e"
    );
}

#[test]
fn invalid_replacement_keeps_last_good_palette_with_public_error() {
    let dir = tempfile::Builder::new()
        .prefix("mc-theme-invalid-")
        .tempdir_in("/tmp")
        .unwrap();
    let file = dir.path().join("current.json");
    write_atomic(&file, &document("#7aa2f7"));
    let running = Running::new(themed_service(ThemerState::watch(dir.path().to_path_buf())));
    let mut client = running.client();
    let good = wait_for(&mut client, |state| {
        state.available && state.error.is_none()
    });
    std::fs::write(&file, b"{ not json").unwrap();
    let stale = wait_for(&mut client, |state| state.error.is_some());
    assert!(stale.available);
    assert_eq!(stale.palette, good.palette);
    let error = stale.error.clone().unwrap();
    assert_eq!(error.code, "THEME_INVALID");
    assert!(!error.message.contains('/'));
    // A valid rewrite clears the error.
    write_atomic(&file, &document("#9ece6a"));
    let recovered = wait_for(&mut client, |state| state.error.is_none());
    assert!(recovered.available);
}

#[test]
fn missing_state_is_unavailable_then_recovers_when_the_directory_appears() {
    let dir = tempfile::Builder::new()
        .prefix("mc-theme-missing-")
        .tempdir_in("/tmp")
        .unwrap();
    // Watch a child directory that does not exist yet.
    let state_dir = dir.path().join("late");
    let running = Running::new(themed_service(ThemerState::watch(state_dir.clone())));
    let mut client = running.client();
    let missing = wait_for(&mut client, |state| !state.available);
    assert!(missing.palette.is_none());
    assert_eq!(missing.error.clone().unwrap().code, "THEME_UNAVAILABLE");
    std::fs::create_dir_all(&state_dir).unwrap();
    write_atomic(&state_dir.join("current.json"), &document("#7aa2f7"));
    let recovered = wait_for(&mut client, |state| state.available);
    assert_eq!(
        recovered.palette.as_ref().unwrap().slug.as_str(),
        "test-pack"
    );
}

#[test]
fn a_fifo_or_oversized_file_is_rejected_without_blocking() {
    let dir = tempfile::Builder::new()
        .prefix("mc-theme-fifo-")
        .tempdir_in("/tmp")
        .unwrap();
    let file = dir.path().join("current.json");
    let fifo = std::process::Command::new("mkfifo")
        .arg(&file)
        .status()
        .expect("mkfifo");
    assert!(fifo.success());
    let running = Running::new(themed_service(ThemerState::watch(dir.path().to_path_buf())));
    let mut client = running.client();
    let state = wait_for(&mut client, |state| !state.available);
    assert_eq!(state.error.clone().unwrap().code, "THEME_INVALID");
    // Replace the FIFO with an oversized document; still rejected, not read unbounded.
    std::fs::remove_file(&file).unwrap();
    let mut oversized = serde_json::to_vec(&document("#7aa2f7")).unwrap();
    oversized.extend(vec![b' '; 70_000]);
    std::fs::write(&file, oversized).unwrap();
    let state = wait_for(&mut client, |state| {
        state
            .error
            .as_ref()
            .is_some_and(|error| !error.message.is_empty())
    });
    assert!(!state.available);
}

#[test]
fn fixture_service_is_hermetic_and_ignores_the_environment() {
    let dir = tempfile::Builder::new()
        .prefix("mc-theme-hermetic-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::write(dir.path().join("current.json"), b"{ broken").unwrap();
    // Fixture state is in-memory only; even a real-looking override is ignored.
    let previous = std::env::var_os("MACTHEMER_STATE");
    unsafe { std::env::set_var("MACTHEMER_STATE", dir.path()) };
    let service = Service::fixture().unwrap();
    match previous {
        Some(value) => unsafe { std::env::set_var("MACTHEMER_STATE", value) },
        None => unsafe { std::env::remove_var("MACTHEMER_STATE") },
    }
    let running = Running::new(service);
    let mut client = running.client();
    let state = client.theme_state().unwrap();
    assert!(state.available && state.error.is_none());
    assert_eq!(
        state.palette.as_ref().unwrap().slug.as_str(),
        "maccer-fixture"
    );
    // Constructors without explicit state advertise no theme capability.
    let plain = Running::new(
        Service::new(Vec::new(), Arc::new(HistoryStore::in_memory().unwrap())).unwrap(),
    );
    let mut plain_client = plain.client();
    assert!(
        !plain_client
            .hello()
            .capabilities
            .iter()
            .any(|capability| capability == "themeState")
    );
    assert!(plain_client.theme_state().is_err());
}

#[test]
fn dropping_the_last_handle_stops_the_reader() {
    let dir = tempfile::Builder::new()
        .prefix("mc-theme-drop-")
        .tempdir_in("/tmp")
        .unwrap();
    write_atomic(&dir.path().join("current.json"), &document("#7aa2f7"));
    let thematic = ThemerState::watch(dir.path().to_path_buf());
    let weak = Arc::downgrade(&thematic);
    let service = themed_service(thematic);
    drop(service);
    // Drop must have joined the reader; the only strong handle is gone.
    assert!(weak.upgrade().is_none());
    // A second watch still works after the first shut down.
    let again = ThemerState::watch(dir.path().to_path_buf());
    assert!(again.snapshot().available);
}
