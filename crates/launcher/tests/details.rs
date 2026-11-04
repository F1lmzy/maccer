use anyhow::{Result, bail};
use launcher_agent::client::{Client, RemoteError};
use launcher_agent::core::{
    Action, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig, ProviderId,
    SearchContext, SearchQuery,
};
use launcher_agent::protocol::{ContentBlock, GetDetailsParams, Route};
use launcher_agent::service::{ItemPresentation, Server, Service, ServiceProvider};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

struct Running {
    dir: tempfile::TempDir,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}
impl Running {
    fn new(service: Arc<Service>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("mc-details-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server_stop = stop.clone();
        let worker = thread::spawn(move || server.serve_until(&server_stop));
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

struct DetailsProvider {
    revision: AtomicUsize,
    panic: AtomicBool,
    oversized: AtomicBool,
    too_many: AtomicBool,
    activations: AtomicUsize,
}
impl Provider for DetailsProvider {
    fn id(&self) -> ProviderId {
        ProviderId("details".into())
    }
    fn name(&self) -> &str {
        "Details"
    }
    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire) as u64
    }
    fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        Ok(vec![Item {
            id: ItemId("internal-id".into()),
            provider: self.id(),
            title: "Preview item".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: json!({"privatePath":"/private/secret.app"}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        self.activations.fetch_add(1, Ordering::SeqCst);
        bail!("details test item has no actions")
    }
}
impl ServiceProvider for DetailsProvider {
    fn description(&self) -> &str {
        "Details test provider"
    }
    fn kind(&self) -> &str {
        "test"
    }
    fn public_id(&self, _: &Item) -> String {
        "public-item".into()
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "test".into(),
            icon: None,
            content: vec![],
            data: None,
        }
    }
    fn details_available(&self, _: &Item) -> bool {
        true
    }
    fn details(&self, _: &Item) -> Result<Vec<ContentBlock>> {
        if self.panic.load(Ordering::Acquire) {
            panic!("expected provider panic");
        }
        if self.oversized.load(Ordering::Acquire) {
            return Ok(vec![ContentBlock::Text {
                text: "x".repeat(65_537),
                truncated: false,
                fallback_text: "large".into(),
            }]);
        }
        if self.too_many.load(Ordering::Acquire) {
            return Ok((0..33)
                .map(|_| ContentBlock::Text {
                    text: "x".into(),
                    truncated: false,
                    fallback_text: "x".into(),
                })
                .collect());
        }
        Ok(vec![ContentBlock::Metadata {
            entries: vec![launcher_agent::protocol::MetadataEntry {
                label: "Type".into(),
                value: "Test".into(),
            }],
            fallback_text: "Safe test details".into(),
        }])
    }
}
fn service(provider: Arc<DetailsProvider>) -> Arc<Service> {
    let provider: Arc<dyn ServiceProvider> = provider;
    Service::new(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
    )
    .unwrap()
}
fn provider() -> Arc<DetailsProvider> {
    Arc::new(DetailsProvider {
        revision: AtomicUsize::new(0),
        panic: AtomicBool::new(false),
        oversized: AtomicBool::new(false),
        too_many: AtomicBool::new(false),
        activations: AtomicUsize::new(0),
    })
}
fn view_params(view: &launcher_agent::protocol::ViewSnapshot) -> GetDetailsParams {
    let item = &view.sections[0].items[0];
    assert!(item.details_available);
    GetDetailsParams {
        view_id: view.view_id.clone(),
        revision: view.revision.clone(),
        provider_id: item.provider_id.clone(),
        item_id: item.id.clone(),
    }
}

#[test]
fn socket_details_are_public_lazy_and_read_only() {
    let provider = provider();
    let running = Running::new(service(provider.clone()));
    let mut client = running.client();
    assert!(
        client
            .hello()
            .capabilities
            .iter()
            .any(|c| c == "itemDetails")
    );
    let view = client
        .open_view(
            Route::Provider {
                provider_id: "details".into(),
            },
            "",
            5,
        )
        .unwrap();
    let params = view_params(&view);
    let details = client.get_details(&params).unwrap();
    let wire = serde_json::to_string(&details).unwrap();
    assert!(!wire.contains("/private") && !wire.contains("internal-id"));
    assert_eq!(details.content[0].fallback_text(), "Safe test details");
    provider.revision.fetch_add(1, Ordering::AcqRel);
    let stale = client.get_details(&params).unwrap_err();
    assert_eq!(
        stale.downcast_ref::<RemoteError>().unwrap().0.data.code,
        "STALE_DETAILS"
    );
    assert_eq!(provider.activations.load(Ordering::SeqCst), 0);
}

#[test]
fn socket_details_reject_unadvertised_stale_and_cross_session_items() {
    let running = Running::new(Service::fixture().unwrap());
    let mut first = running.client();
    let view = first
        .open_view(
            Route::Provider {
                provider_id: "themes".into(),
            },
            "",
            5,
        )
        .unwrap();
    let params = view_params(&view);
    let mut other = running.client();
    let home = other.open_view(Route::Home, "", 5).unwrap();
    let menu_item = &home.sections[0].items[0];
    let unadvertised = GetDetailsParams {
        view_id: home.view_id.clone(),
        revision: home.revision.clone(),
        provider_id: menu_item.provider_id.clone(),
        item_id: menu_item.id.clone(),
    };
    let error = other.get_details(&unadvertised).unwrap_err();
    assert_eq!(
        error.downcast_ref::<RemoteError>().unwrap().0.data.code,
        "DETAILS_NOT_AUTHORIZED"
    );
    // Separate sessions can issue matching view IDs/revisions, but their grants stay isolated.
    let error = other.get_details(&params).unwrap_err();
    let remote = error.downcast_ref::<RemoteError>().unwrap();
    assert_eq!(remote.0.data.code, "DETAILS_NOT_AUTHORIZED");
    let refreshed = first
        .query_view(&view.view_id, "no matching fixture")
        .unwrap();
    let stale = GetDetailsParams {
        revision: view.revision,
        ..params
    };
    assert_ne!(refreshed.revision, stale.revision);
    assert_eq!(
        first
            .get_details(&stale)
            .unwrap_err()
            .downcast_ref::<RemoteError>()
            .unwrap()
            .0
            .data
            .code,
        "STALE_DETAILS"
    );
}

#[test]
fn socket_fixture_details_cover_palette_value_unicode_and_unknown_fallback() {
    let running = Running::new(Service::fixture().unwrap());
    for (provider_id, query, expected) in [
        ("themes", "", "colorPalette"),
        ("calculator", "", "value"),
        ("files", "", "text"),
        ("web", "", "futurePreview"),
    ] {
        let mut client = running.client();
        let view = client
            .open_view(
                Route::Provider {
                    provider_id: provider_id.into(),
                },
                query,
                5,
            )
            .unwrap();
        let details = client.get_details(&view_params(&view)).unwrap();
        let value = serde_json::to_value(details).unwrap();
        if expected == "futurePreview" {
            assert_eq!(value["content"][0]["type"], expected);
            assert_eq!(
                value["content"][0]["fallbackText"],
                "Search the web for this result"
            );
        } else {
            assert_eq!(value["content"][0]["type"], expected);
        }
        if provider_id == "files" {
            assert!(value.to_string().contains("🦀"));
        }
    }
}

#[test]
fn socket_details_bound_provider_output_and_isolate_panics() {
    for (panic, oversized, too_many, expected) in [
        (false, true, false, "DETAILS_TOO_LARGE"),
        (false, false, true, "DETAILS_TOO_LARGE"),
        (true, false, false, "PROVIDER_PANIC"),
    ] {
        let provider = provider();
        provider.panic.store(panic, Ordering::Release);
        provider.oversized.store(oversized, Ordering::Release);
        provider.too_many.store(too_many, Ordering::Release);
        let running = Running::new(service(provider));
        let mut client = running.client();
        let view = client
            .open_view(
                Route::Provider {
                    provider_id: "details".into(),
                },
                "",
                5,
            )
            .unwrap();
        let error = client.get_details(&view_params(&view)).unwrap_err();
        assert_eq!(
            error.downcast_ref::<RemoteError>().unwrap().0.data.code,
            expected
        );
    }
}
