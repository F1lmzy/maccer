//! Shared web/clipboard acceptance. Only owned text and fake desktop effects.
use launcher_client::{Client, RemoteError};
use launcher_core::{HistoryStore, Provider, ProviderConfig};
use launcher_macos::Platform;
use launcher_protocol::{
    ContentBlock, GetDetailsParams, InvokeActionParams, Presentation, Route, ViewSnapshot,
};
use launcher_service::{
    ClipboardServiceProvider, Server, Service, ServiceProvider, WebServiceProvider,
};
use provider_clipboard::ClipboardProvider;
use provider_web::WebProvider;
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

#[derive(Default)]
struct Desktop(Mutex<Vec<String>>);
impl Platform for Desktop {
    fn launch_application(&self, _: &Path) -> anyhow::Result<()> {
        panic!("unexpected app launch")
    }
    fn reveal_in_finder(&self, _: &Path) -> anyhow::Result<()> {
        panic!("unexpected reveal")
    }
    fn open_url(&self, url: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(format!("open:{url}"));
        Ok(())
    }
    fn copy_text(&self, text: &str) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(format!("copy:{text}"));
        Ok(())
    }
}
struct Running {
    dir: tempfile::TempDir,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<anyhow::Result<()>>>,
    clipboard: Arc<ClipboardProvider>,
    desktop: Arc<Desktop>,
    history: Arc<HistoryStore>,
}
impl Running {
    fn new(enabled: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let desktop = Arc::new(Desktop::default());
        let clipboard = Arc::new(ClipboardProvider::new(desktop.clone(), 20));
        clipboard.capture(1, Some("pre-launch content"));
        clipboard.capture(2, Some("private entry\nsecond line"));
        let web: Arc<dyn ServiceProvider> = Arc::new(WebServiceProvider::new(
            WebProvider::new(
                desktop.clone(),
                WebProvider::default_engines(),
                "google".into(),
            )
            .unwrap(),
        ));
        let clips: Arc<dyn ServiceProvider> =
            Arc::new(ClipboardServiceProvider::new(clipboard.clone()));
        let config = |prefix: &str| ProviderConfig {
            enabled,
            prefix: Some(prefix.into()),
            default_search: true,
            max_results: 20,
            ..Default::default()
        };
        let history = Arc::new(HistoryStore::in_memory().unwrap());
        let service = Service::new(
            vec![(web, config("?")), (clips, config(":"))],
            history.clone(),
        )
        .unwrap();
        let dir = tempfile::Builder::new()
            .prefix("mc-shared-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = Some(thread::spawn(move || server.serve_until(&worker_stop)));
        Self {
            dir,
            stop,
            worker,
            clipboard,
            desktop,
            history,
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
fn params(
    snapshot: &ViewSnapshot,
    provider: &str,
    action: &str,
    operation: &str,
) -> InvokeActionParams {
    let descriptor = snapshot
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .find(|i| i.provider_id == provider)
        .unwrap()
        .actions
        .iter()
        .find(|a| a.id == action)
        .unwrap();
    InvokeActionParams {
        view_id: snapshot.view_id.clone(),
        revision: snapshot.revision.clone(),
        action_ref: descriptor.action_ref.clone(),
        operation_id: operation.into(),
        confirmed: false,
    }
}
#[test]
fn web_is_shared_encoded_and_deduplicated_without_payloads() {
    let running = Running::new(true);
    let mut client = running.client();
    let providers = client.providers().unwrap();
    assert!(providers.iter().any(|p| p.id == "web"));
    assert!(
        providers
            .iter()
            .any(|p| p.id == "clipboard" && p.explicit_scope)
    );
    let home = client.open_view(Route::Home, "", 20).unwrap();
    assert_eq!(
        home.sections
            .iter()
            .flat_map(|s| &s.items)
            .filter(|i| i.kind == "provider")
            .count(),
        2
    );
    let view = client.query_view(&home.view_id, "café & tea").unwrap();
    let web: Vec<_> = view
        .sections
        .iter()
        .flat_map(|s| &s.items)
        .filter(|i| i.provider_id == "web")
        .collect();
    assert_eq!(web.len(), 2);
    assert_eq!(web[0].title, "Search with Google");
    let encoded = serde_json::to_string(&view).unwrap();
    assert!(!encoded.contains("payload"));
    assert!(!encoded.contains("private entry"));
    let request = params(&view, "web", "search", "search-once");
    client.invoke(&request).unwrap();
    client.invoke(&request).unwrap();
    assert_eq!(
        *running.desktop.0.lock().unwrap(),
        ["open:https://www.google.com/search?q=caf%C3%A9+%26+tea"]
    );
    let mut other = running.client();
    assert!(other.invoke(&request).is_err());
    let next = client.query_view(&home.view_id, "new query").unwrap();
    let mut stale = request;
    stale.operation_id = "stale-web".into();
    assert!(client.invoke(&stale).is_err());
    client
        .invoke(&params(&next, "web", "copy-url", "copy-url"))
        .unwrap();
    assert!(running.desktop.0.lock().unwrap()[1].starts_with("copy:https://www.google.com/"));
}
#[test]
fn clipboard_stays_scoped_and_mutations_refresh_without_paste_or_history() {
    let running = Running::new(true);
    let mut client = running.client();
    let global = client.open_view(Route::Home, "private entry", 20).unwrap();
    assert!(
        !global
            .sections
            .iter()
            .flat_map(|s| &s.items)
            .any(|i| i.provider_id == "clipboard")
    );
    let scoped = client
        .open_view(
            Route::Provider {
                provider_id: "clipboard".into(),
            },
            "private",
            20,
        )
        .unwrap();
    let item = &scoped.sections[0].items[0];
    assert_eq!(item.title, "private entry");
    assert!(!serde_json::to_string(item).unwrap().contains("second line"));
    assert!(!item.actions.iter().any(|a| a.id == "paste"));
    let details = client
        .get_details(&GetDetailsParams {
            view_id: scoped.view_id.clone(),
            revision: scoped.revision.clone(),
            provider_id: "clipboard".into(),
            item_id: item.id.clone(),
        })
        .unwrap();
    assert!(
        matches!(&details.content[0], ContentBlock::Text { text, .. } if text == "private entry\nsecond line")
    );
    let pin = params(&scoped, "clipboard", "pin", "pin-once");
    let outcome = client.invoke(&pin).unwrap();
    client.invoke(&pin).unwrap();
    assert!(
        matches!(outcome.presentation, Some(Presentation::SetQuery { query }) if query == "private")
    );
    let fresh = client.query_view(&scoped.view_id, "private").unwrap();
    assert_eq!(
        fresh.sections[0].items[0].subtitle.as_deref(),
        Some("Pinned")
    );
    client
        .invoke(&params(&fresh, "clipboard", "copy", "copy-entry"))
        .unwrap();
    assert_eq!(
        *running.desktop.0.lock().unwrap(),
        ["copy:private entry\nsecond line"]
    );
    let count = running
        .history
        .stats(
            &launcher_core::ProviderId("clipboard".into()),
            &launcher_core::ItemId(item.id.clone()),
        )
        .unwrap()
        .count;
    assert_eq!(count, 0);
    client
        .invoke(&params(&fresh, "clipboard", "delete", "delete-entry"))
        .unwrap();
    assert!(
        client
            .query_view(&scoped.view_id, "")
            .unwrap()
            .sections
            .is_empty()
    );
    // A second frontend sees the same RAM store, including management changes.
    running.clipboard.capture(3, Some("owned first"));
    running.clipboard.capture(4, Some("owned second"));
    let mut other = running.client();
    let shared = other
        .open_view(
            Route::Provider {
                provider_id: "clipboard".into(),
            },
            "",
            20,
        )
        .unwrap();
    assert_eq!(shared.sections[0].items.len(), 2);
    other
        .invoke(&params(&shared, "clipboard", "pin", "pin-shared"))
        .unwrap();
    let pinned = client.query_view(&scoped.view_id, "").unwrap();
    assert_eq!(
        pinned.sections[0].items[0].subtitle.as_deref(),
        Some("Pinned")
    );
    client
        .invoke(&params(&pinned, "clipboard", "unpin", "unpin-shared"))
        .unwrap();
    let unpinned = other.query_view(&shared.view_id, "").unwrap();
    assert!(
        unpinned.sections[0]
            .items
            .iter()
            .all(|item| item.subtitle.as_deref() != Some("Pinned"))
    );
    let clear = params(&unpinned, "clipboard", "clear", "clear-shared");
    other.invoke(&clear).unwrap();
    // Replaying an accepted Clear cannot erase later captures.
    running.clipboard.capture(5, Some("after clear"));
    other.invoke(&clear).unwrap();
    let after = client.query_view(&scoped.view_id, "").unwrap();
    assert_eq!(after.sections[0].items.len(), 1);
    assert_eq!(after.sections[0].items[0].title, "after clear");
    let paste = launcher_core::Action {
        id: launcher_core::ActionId("paste".into()),
        title: "Paste".into(),
    };
    let core_item = launcher_core::Item {
        id: launcher_core::ItemId("clipboard:0".into()),
        provider: launcher_core::ProviderId("clipboard".into()),
        title: "".into(),
        subtitle: None,
        keywords: vec![],
        icon: None,
        score: 0.,
        payload: serde_json::json!({"entry":0}),
    };
    assert!(
        ClipboardServiceProvider::new(running.clipboard.clone())
            .activate(&core_item, &paste)
            .is_err()
    );
}
#[test]
fn disabled_shared_providers_are_unavailable() {
    let running = Running::new(false);
    let mut client = running.client();
    assert!(client.providers().unwrap().is_empty());
    assert!(
        client
            .open_view(Route::Home, "private", 20)
            .unwrap()
            .sections
            .is_empty()
    );
    assert!(
        client
            .open_view(
                Route::Provider {
                    provider_id: "clipboard".into()
                },
                "",
                20
            )
            .is_err()
    );
    assert!(running.desktop.0.lock().unwrap().is_empty());
}
#[test]
fn clipboard_escaped_details_remain_within_protocol_bound_and_expire_on_capture() {
    let running = Running::new(true);
    let mut client = running.client();
    let text = "\u{0001}".repeat(provider_clipboard::MAX_ENTRY_BYTES);
    assert!(running.clipboard.capture(3, Some(&text)));
    let scoped = client
        .open_view(
            Route::Provider {
                provider_id: "clipboard".into(),
            },
            "",
            20,
        )
        .unwrap();
    let item = &scoped.sections[0].items[0];
    let request = GetDetailsParams {
        view_id: scoped.view_id.clone(),
        revision: scoped.revision.clone(),
        provider_id: "clipboard".into(),
        item_id: item.id.clone(),
    };
    let details = client.get_details(&request).unwrap();
    assert!(serde_json::to_vec(&details.content).unwrap().len() <= 64 * 1024);
    assert!(matches!(
        &details.content[0],
        ContentBlock::Text {
            truncated: true,
            ..
        }
    ));
    running.clipboard.capture(4, Some("new capture"));
    let error = client.get_details(&request).unwrap_err();
    assert_eq!(
        error.downcast_ref::<RemoteError>().unwrap().0.data.code,
        "STALE_DETAILS"
    );
}
