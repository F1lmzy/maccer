use launcher_agent::agent::connection::ServiceConnection;
use launcher_agent::service::{Server, Service};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

struct FixtureServer {
    stop: std::sync::Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn fixture_server() -> (tempfile::TempDir, std::path::PathBuf, FixtureServer) {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("service.sock");
    let server = Server::bind(socket.clone(), Service::fixture().unwrap()).unwrap();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || server.serve_until(&worker_stop).unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(socket.exists());
    (
        dir,
        socket,
        FixtureServer {
            stop,
            worker: Some(worker),
        },
    )
}

fn wait_for(connection: &ServiceConnection, kind: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Some(event) = connection.try_recv() {
            if event["type"] == "error" {
                panic!("service actor error: {event}");
            }
            if event["type"] == kind {
                return event;
            }
            seen.push(event);
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for {kind} event; received {seen:?}")
}

#[test]
fn validates_socket_paths_before_starting_actor() {
    for path in [
        std::path::Path::new("relative.sock"),
        std::path::Path::new(""),
    ] {
        assert!(ServiceConnection::connect(path).is_err());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let bytes = vec![b'a'; 4097];
        let path = std::path::Path::new(std::ffi::OsStr::from_bytes(&bytes));
        assert!(ServiceConnection::connect(path).is_err());
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
        assert!(ServiceConnection::connect(&path).is_err());
    }
}

#[test]
fn queries_fixture_and_exposes_events_without_c_abi() {
    let (_dir, socket, _server) = fixture_server();
    let connection = ServiceConnection::connect(&socket).unwrap();
    assert!(wait_for(&connection, "ready")["type"] == "ready");
    connection.search(1, "thmes".into()).unwrap();
    let results = wait_for(&connection, "results");
    assert_eq!(results["request"], 1);
    assert!(
        results["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "themes")
    );
    connection.home(2).unwrap();
    let home = wait_for(&connection, "outcome");
    assert_eq!(home["request"], 2);
    assert_eq!(home["route"]["type"], "home");
    connection.cancel().unwrap();
    connection.search(4, "apps".into()).unwrap();
    assert_eq!(wait_for(&connection, "results")["request"], 4);
}

#[test]
fn facade_supports_independent_details_and_snapshot_scoped_refresh() {
    let (_dir, socket, _server) = fixture_server();
    let connection = ServiceConnection::connect(&socket).unwrap();
    wait_for(&connection, "ready");
    connection.search(1, "themes".into()).unwrap();
    let home = wait_for(&connection, "results");
    assert!(
        home["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "themes")
    );
    connection
        .details(1, 1, "menu".into(), "themes".into())
        .unwrap();
    let unavailable = wait_for(&connection, "details");
    assert_eq!(unavailable["detailRequest"], 1);
    assert_eq!(unavailable["content"][0]["text"], "No preview available");
    connection
        .activate(2, 1, "menu".into(), "themes".into(), "open".into())
        .unwrap();
    wait_for(&connection, "outcome");
    connection.search(3, String::new()).unwrap();
    let scoped = wait_for(&connection, "results");
    let theme = scoped["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "themes")
        .unwrap();
    assert_eq!(theme["id"], "themes:fixture");
    connection
        .details(2, 3, "themes".into(), "themes:fixture".into())
        .unwrap();
    let details = wait_for(&connection, "details");
    assert_eq!(details["request"], 3);
    assert_eq!(details["detailRequest"], 2);
    connection.refresh(4, 3).unwrap();
    let refreshed = wait_for(&connection, "results");
    assert_eq!(refreshed["request"], 4);
    assert_eq!(refreshed["recovery"], true);
}

#[test]
fn facade_exposes_theme_state_without_search_and_across_cancel() {
    let (_dir, socket, _server) = fixture_server();
    let connection = ServiceConnection::connect(&socket).unwrap();
    assert_eq!(wait_for(&connection, "ready")["themeState"], true);
    // No search is issued; the actor still publishes active state.
    let state = wait_for(&connection, "themeState");
    assert_eq!(state["state"]["available"], true);
    assert_eq!(state["state"]["palette"]["schema"], 1);
    let revision = state["state"]["revision"].clone();
    connection.cancel().unwrap();
    // Cancellation moves the request watermark but not the shared palette.
    let mut client = launcher_agent::client::Client::connect(&socket).unwrap();
    assert_eq!(client.theme_state().unwrap().revision, revision);
}

#[test]
fn drop_does_not_stop_shared_service_and_later_client_connects() {
    let (_dir, socket, _server) = fixture_server();
    let first = ServiceConnection::connect(&socket).unwrap();
    wait_for(&first, "ready");
    drop(first);
    thread::sleep(Duration::from_millis(30));
    let second = ServiceConnection::connect(&socket).unwrap();
    assert_eq!(wait_for(&second, "ready")["type"], "ready");
    second.search(1, "apps".into()).unwrap();
    assert_eq!(wait_for(&second, "results")["route"]["type"], "home");
}

#[test]
fn icon_accessor_returns_owned_pixels_for_cached_token() {
    let (_dir, socket, _server) = fixture_server();
    let connection = ServiceConnection::connect(&socket).unwrap();
    wait_for(&connection, "ready");
    assert!(connection.icon("missing").is_none());
}

// A provider that requires explicit confirmation for its single action.
struct Confirming {
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl launcher_agent::core::Provider for Confirming {
    fn id(&self) -> launcher_agent::core::ProviderId {
        launcher_agent::core::ProviderId("confirming".into())
    }
    fn name(&self) -> &str {
        "Confirming"
    }
    fn search(
        &self,
        _: &launcher_agent::core::SearchQuery,
        _: &launcher_agent::core::SearchContext,
    ) -> anyhow::Result<Vec<launcher_agent::core::Item>> {
        Ok(vec![launcher_agent::core::Item {
            id: launcher_agent::core::ItemId("one".into()),
            provider: self.id(),
            title: "Confirm target".into(),
            subtitle: None,
            keywords: vec!["confirm".into()],
            icon: None,
            score: 0.0,
            payload: serde_json::Value::Null,
        }])
    }
    fn actions(&self, _: &launcher_agent::core::Item) -> Vec<launcher_agent::core::Action> {
        vec![launcher_agent::core::Action {
            id: launcher_agent::core::ActionId("apply".into()),
            title: "Apply".into(),
        }]
    }
    fn activate(
        &self,
        _: &launcher_agent::core::Item,
        _: &launcher_agent::core::Action,
    ) -> anyhow::Result<launcher_agent::core::ActionOutcome> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(launcher_agent::core::ActionOutcome::KeepOpen(
            "applied".into(),
        ))
    }
}
impl launcher_agent::service::ServiceProvider for Confirming {
    fn description(&self) -> &str {
        "confirmation transport test"
    }
    fn kind(&self) -> &str {
        "test"
    }
    fn presentation(
        &self,
        _: &launcher_agent::core::Item,
    ) -> launcher_agent::service::ItemPresentation {
        launcher_agent::service::ItemPresentation {
            kind: "test".into(),
            icon: None,
            content: vec![],
            data: None,
        }
    }
    fn action_confirmation(
        &self,
        _: &launcher_agent::core::Item,
        _: &launcher_agent::core::Action,
    ) -> Option<String> {
        Some("Apply \"Confirm target\"?".into())
    }
}

fn wait_any(connection: &ServiceConnection) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Some(event) = connection.try_recv() {
            return event;
        }
        thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out waiting for any event");
}

#[test]
fn facade_surfaces_confirmation_and_requires_activate_confirmed() {
    let dir = tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("service.sock");
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let service = Service::new(
        vec![(
            std::sync::Arc::new(Confirming {
                calls: calls.clone(),
            }) as std::sync::Arc<dyn launcher_agent::service::ServiceProvider>,
            launcher_agent::core::ProviderConfig::default(),
        )],
        std::sync::Arc::new(launcher_agent::core::HistoryStore::in_memory().unwrap()),
    )
    .unwrap();
    let server = Server::bind(socket.clone(), service).unwrap();
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker = thread::spawn(move || server.serve_until(&worker_stop).unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    let _server = FixtureServer {
        stop,
        worker: Some(worker),
    };

    let connection = ServiceConnection::connect(&socket).unwrap();
    wait_for(&connection, "ready");
    connection.search(1, "confirm".into()).unwrap();
    let results = wait_for(&connection, "results");
    let item = results["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["provider"] == "confirming")
        .unwrap();
    assert_eq!(
        item["actions"][0]["confirmation"],
        "Apply \"Confirm target\"?"
    );

    // Unconfirmed invocation is rejected by the service before any side effect.
    connection
        .activate(2, 1, "confirming".into(), "one".into(), "apply".into())
        .unwrap();
    let rejection = wait_any(&connection);
    assert_eq!(rejection["type"], "error");
    assert!(
        rejection["message"]
            .as_str()
            .unwrap()
            .contains("CONFIRMATION_REQUIRED"),
        "{rejection}"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    // Confirmed invocation executes exactly once.
    connection
        .activate_confirmed(
            3,
            1,
            "confirming".into(),
            "one".into(),
            "apply".into(),
            Default::default(),
        )
        .unwrap();
    let outcome = wait_for(&connection, "outcome");
    assert_eq!(outcome["request"], 3);
    assert_eq!(outcome["kind"], "keepOpen");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
