use anyhow::Result;
use launcher_agent::client::Client;
use launcher_agent::core::{
    Action, ActionOutcome, HistoryStore, IconDescriptor, Item, ItemId, Provider, ProviderConfig,
    ProviderId, SearchContext, SearchQuery,
};
use launcher_agent::macos::ApplicationIconPixels;
use launcher_agent::protocol::{ContentBlock, DomainData, Icon, Route};
use launcher_agent::service::{
    AssetStore, IconResolver, ItemPresentation, NativeIconResolver, Server, Service,
    ServiceProvider,
};
use serde_json::json;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

struct Resolver {
    calls: AtomicUsize,
    bad: bool,
    missing: bool,
}
impl IconResolver for Resolver {
    fn resolve(&self, _: &Path) -> Result<Option<ApplicationIconPixels>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.missing {
            return Ok(None);
        }
        Ok(Some(if self.bad {
            ApplicationIconPixels {
                bgra: vec![0; 16_385],
                width: 65,
                height: 1,
            }
        } else {
            ApplicationIconPixels {
                bgra: vec![1, 2, 3, 4],
                width: 1,
                height: 1,
            }
        }))
    }
}
struct AssetProvider {
    store: Arc<AssetStore>,
}
impl Provider for AssetProvider {
    fn id(&self) -> ProviderId {
        ProviderId("icons".into())
    }
    fn name(&self) -> &str {
        "Icons"
    }
    fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        Ok(vec![Item {
            id: ItemId("item".into()),
            provider: self.id(),
            title: "Icon result".into(),
            subtitle: None,
            keywords: vec![],
            icon: Some(IconDescriptor::ApplicationBundle(PathBuf::from(
                "/private/test.app",
            ))),
            score: 0.0,
            payload: json!({"secret":"not public"}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        unreachable!()
    }
}
impl ServiceProvider for AssetProvider {
    fn description(&self) -> &str {
        "icon test"
    }
    fn kind(&self) -> &str {
        "application"
    }
    fn presentation(&self, item: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "application".into(),
            icon: Some(Icon::Asset {
                asset: self.store.reference(Path::new("/private/test.app"), 0),
                fallback_text: item.title.clone(),
            }),
            content: vec![ContentBlock::Text {
                text: "safe".into(),
                truncated: false,
                fallback_text: "safe".into(),
            }],
            data: Some(DomainData::Application {
                schema_version: 1,
                application_id: "opaque-app".into(),
            }),
        }
    }
    fn public_id(&self, _: &Item) -> String {
        "opaque-app".into()
    }
}
fn service(resolver: Arc<Resolver>) -> Arc<Service> {
    let assets = AssetStore::new(resolver);
    let provider: Arc<dyn ServiceProvider> = Arc::new(AssetProvider {
        store: assets.clone(),
    });
    Service::with_assets(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
        assets,
    )
    .unwrap()
}
struct Running {
    dir: tempfile::TempDir,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<thread::JoinHandle<Result<()>>>,
}
impl Running {
    fn new(service: Arc<Service>) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::Builder::new()
            .prefix("mc-assets-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let server = Server::bind(dir.path().join("rpc"), service).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let s = stop.clone();
        let worker = thread::spawn(move || server.serve_until(&s));
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

#[test]
fn asset_fetch_is_lazy_cached_bounded_and_authorized_per_session() {
    let resolver = Arc::new(Resolver {
        calls: AtomicUsize::new(0),
        bad: false,
        missing: false,
    });
    let running = Running::new(service(resolver.clone()));
    let mut first = running.client();
    assert!(
        first
            .hello()
            .capabilities
            .iter()
            .any(|c| c == "iconAssetsBgra8")
    );
    let view = first
        .open_view(
            Route::Provider {
                provider_id: "icons".into(),
            },
            "",
            5,
        )
        .unwrap();
    let row = &view.sections[0].items[0];
    assert_eq!(row.id, "opaque-app");
    let wire = serde_json::to_string(row).unwrap();
    assert!(!wire.contains("/private") && !wire.contains("secret"));
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        0,
        "view projection must not resolve icons"
    );
    let asset = match row.icon.as_ref().unwrap() {
        Icon::Asset { asset, .. } => asset.clone(),
        _ => panic!("expected asset ref"),
    };
    let mut second = running.client();
    // A session that has only seen the menu has no icon grant, even when
    // another session knows the shared cache resource ID.
    second.open_view(Route::Home, "", 5).unwrap();
    assert!(
        second
            .get_asset(&asset)
            .unwrap_err()
            .to_string()
            .contains("ASSET_NOT_AUTHORIZED")
    );
    let data = first.get_asset(&asset).unwrap();
    assert_eq!(data.bytes, [1, 2, 3, 4]);
    assert_eq!(data.metadata.stride, 4);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first.get_asset(&asset).unwrap().bytes, [1, 2, 3, 4]);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    first.open_view(Route::Home, "", 5).unwrap();
    assert!(
        first
            .get_asset(&asset)
            .unwrap_err()
            .to_string()
            .contains("ASSET_NOT_AUTHORIZED")
    );
}

#[test]
fn malformed_resolver_pixels_are_rejected_and_cached_capacity_evicts_old_refs() {
    let bad = AssetStore::new(Arc::new(Resolver {
        calls: AtomicUsize::new(0),
        bad: true,
        missing: false,
    }));
    let reference = bad.reference(Path::new("/private/x.app"), 1);
    assert!(
        bad.get(&reference.id, &reference.revision, "bgra8")
            .is_err()
    );
    let good = AssetStore::new(Arc::new(Resolver {
        calls: AtomicUsize::new(0),
        bad: false,
        missing: false,
    }));
    let old = good.reference(Path::new("/private/old.app"), 1);
    for n in 0..256 {
        good.reference(Path::new("/private/new.app"), n);
    }
    assert!(
        good.get(&old.id, &old.revision, "bgra8")
            .unwrap_err()
            .to_string()
            .contains("ASSET_EXPIRED")
    );
    let missing_resolver = Arc::new(Resolver {
        calls: AtomicUsize::new(0),
        bad: false,
        missing: true,
    });
    let missing = AssetStore::new(missing_resolver.clone());
    let missing_ref = missing.reference(Path::new("/private/missing.app"), 2);
    for _ in 0..2 {
        assert!(
            missing
                .get(&missing_ref.id, &missing_ref.revision, "bgra8")
                .unwrap_err()
                .to_string()
                .contains("ASSET_UNAVAILABLE")
        );
    }
    assert_eq!(missing_resolver.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn generic_rpc_request_rejects_binary_asset_method_without_poisoning_connection() {
    let running = Running::new(Service::fixture().unwrap());
    let mut client = running.client();
    assert!(
        client
            .request::<serde_json::Value>("assets.get", json!({}))
            .is_err()
    );
    assert_eq!(client.providers().unwrap().len(), 8);
}

#[test]
fn native_resolver_is_only_a_thin_adapter_and_is_not_used_by_fixtures() {
    let _resolver = NativeIconResolver;
    let running = Running::new(Service::fixture().unwrap());
    let client = running.client();
    assert!(
        !client
            .hello()
            .capabilities
            .iter()
            .any(|c| c == "iconAssetsBgra8")
    );
}
