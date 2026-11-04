//! Live application-provider conformance: opaque public IDs, lazy icon assets,
//! and native activation only through authorized actions.
//!
//! Everything here uses a synthetic catalog, a fake platform and a fake icon
//! resolver. No real application is discovered, launched or revealed.

use launcher_agent::core::{
    CancellationToken, HistoryStore, Provider, ProviderConfig, SearchContext, SearchQuery,
};
use launcher_agent::macos::{Application, ApplicationIconPixels, Platform};
use launcher_agent::protocol::RpcRequest;
use launcher_agent::providers::apps::ApplicationProvider;
use launcher_agent::service::{
    ApplicationServiceProvider, AssetStore, IconResolver, Service, ServiceProvider, Session,
    application_service,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

fn app(name: &str, bundle: &str) -> Application {
    Application {
        path: format!("/Applications/{name}.app").into(),
        bundle_id: Some(bundle.into()),
        display_name: name.into(),
        unlocalized_name: None,
        executable_name: Some(name.to_lowercase()),
        icon_path: None,
    }
}

/// An application without a bundle identifier, so its core item ID embeds the
/// absolute bundle path. This is the case that must not leak.
fn unbundled_app(name: &str) -> Application {
    Application {
        bundle_id: None,
        ..app(name, "unused")
    }
}

#[derive(Default)]
struct FakePlatform {
    calls: Mutex<Vec<String>>,
}
impl FakePlatform {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}
impl Platform for FakePlatform {
    fn launch_application(&self, path: &Path) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("open:{}", path.display()));
        Ok(())
    }
    fn reveal_in_finder(&self, path: &Path) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("reveal:{}", path.display()));
        Ok(())
    }
    fn open_url(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
    fn copy_text(&self, _: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FakeIconResolver {
    calls: AtomicUsize,
}
impl IconResolver for FakeIconResolver {
    fn resolve(&self, _path: &Path) -> anyhow::Result<Option<ApplicationIconPixels>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(ApplicationIconPixels {
            bgra: vec![10, 20, 30, 255],
            width: 1,
            height: 1,
        }))
    }
}

fn service(
    platform: Arc<FakePlatform>,
    resolver: Arc<FakeIconResolver>,
    apps: Vec<Application>,
) -> Arc<Service> {
    let provider = ApplicationProvider::new(apps, platform);
    let history = Arc::new(HistoryStore::in_memory().unwrap());
    application_service(provider, ProviderConfig::default(), history, resolver).unwrap()
}

fn ctx(limit: usize) -> SearchContext {
    SearchContext {
        generation: 1,
        cancellation: CancellationToken::default(),
        limit,
    }
}

fn call(session: &mut Session, id: &str, method: &str, params: Value) -> Value {
    let response = session.handle(RpcRequest::new(id, method, params));
    assert!(
        response.error.is_none(),
        "{}",
        response.error.map(|e| e.message).unwrap_or_default()
    );
    response.result.unwrap()
}

fn initialized(service: &Arc<Service>) -> Session {
    let mut session = service.session();
    call(
        &mut session,
        "hello",
        "hello",
        json!({"major":1,"minor":0,"clientName":"apps-test"}),
    );
    session
}

fn scoped(session: &mut Session, query: &str) -> Value {
    call(
        session,
        "open",
        "views.open",
        json!({"route":{"type":"provider","providerId":"apps"},"query":query,"limit":20}),
    )
}

#[test]
fn application_views_are_opaque_and_icon_assets_are_lazy() {
    let platform = Arc::new(FakePlatform::default());
    let resolver = Arc::new(FakeIconResolver::default());
    let service = service(
        platform,
        resolver.clone(),
        vec![
            unbundled_app("Secret Tool"),
            app("Safari", "com.apple.safari"),
        ],
    );
    let mut session = initialized(&service);
    let view = scoped(&mut session, "Secret");
    let item = &view["sections"][0]["items"][0];

    assert_eq!(item["kind"], "application");
    assert_eq!(item["data"]["type"], "application");
    let public = item["data"]["applicationId"].as_str().unwrap();
    assert!(public.starts_with("application-"), "got {public}");
    // The published ID is the same opaque value, never the core path-bearing ID.
    assert_eq!(item["id"], public);
    assert_eq!(item["icon"]["type"], "asset");
    assert_eq!(item["icon"]["fallbackText"], "Secret Tool");
    assert_eq!(item["detailsAvailable"], true);
    assert!(
        item["icon"]["asset"]["id"]
            .as_str()
            .unwrap()
            .starts_with("icon-")
    );

    let serialized = view.to_string();
    assert!(!serialized.contains("/Applications"), "{serialized}");
    assert!(!serialized.contains(".app"), "{serialized}");
    assert!(!serialized.contains("payload"), "{serialized}");
    // Registering a reference is lazy: projecting a view resolves no icon bytes.
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        0,
        "view projection must not resolve icons"
    );
    let details = call(
        &mut session,
        "details",
        "views.details",
        json!({"viewId":view["viewId"],"revision":view["revision"],"providerId":item["providerId"],"itemId":item["id"]}),
    );
    assert_eq!(details["content"][0]["type"], "metadata");
    assert!(
        details["content"][0]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["label"] == "Application ID" && entry["value"] == public })
    );
    assert_eq!(details["content"][1]["type"], "image");
    let details_wire = details.to_string();
    assert!(!details_wire.contains("/Applications") && !details_wire.contains("Secret Tool.app"));
    assert_eq!(
        resolver.calls.load(Ordering::SeqCst),
        0,
        "details only register lazy icon references"
    );
    let asset = call(
        &mut session,
        "asset",
        "assets.get",
        json!({
            "assetId":details["content"][1]["asset"]["id"],
            "revision":details["content"][1]["asset"]["revision"],
            "representation":"bgra8"
        }),
    );
    assert_eq!(asset["assetId"], details["content"][1]["asset"]["id"]);
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn public_ids_are_deterministic_and_path_free() {
    let assets = AssetStore::new(Arc::new(FakeIconResolver::default()));
    let secret = unbundled_app("Secret Tool");
    let public_id = |apps: Vec<Application>, title: &str| {
        let provider = ApplicationProvider::new(apps, Arc::new(FakePlatform::default()));
        let item = provider
            .search(&SearchQuery::default(), &ctx(5))
            .unwrap()
            .into_iter()
            .find(|i| i.title == title)
            .unwrap();
        ApplicationServiceProvider::new(provider, assets.clone()).public_id(&item)
    };

    let solo = public_id(vec![secret.clone()], "Secret Tool");
    // The same application in a different catalog keeps the same public ID.
    let with_neighbour = public_id(
        vec![secret, app("Extra", "dev.maccer.extra")],
        "Secret Tool",
    );
    assert_eq!(solo, with_neighbour);
    assert!(!solo.contains('/'), "{solo}");
    assert!(!solo.contains(".app"), "{solo}");

    let other = public_id(vec![app("Safari", "com.apple.safari")], "Safari");
    assert_ne!(solo, other);
}

#[test]
fn home_menu_and_direct_scope_reach_applications() {
    let service = service(
        Arc::new(FakePlatform::default()),
        Arc::new(FakeIconResolver::default()),
        vec![app("Safari", "com.apple.safari")],
    );
    let mut session = initialized(&service);

    let home = call(
        &mut session,
        "h",
        "views.open",
        json!({"route":{"type":"home"},"query":"","limit":20}),
    );
    assert!(
        home["sections"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == "apps"),
        "home menu must include the applications provider"
    );

    // A plain home query searches applications instead of requiring a prefix.
    let mixed = call(
        &mut session,
        "q",
        "views.query",
        json!({"viewId":home["viewId"],"query":"Safari"}),
    );
    assert!(
        mixed["sections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == "results" && s.to_string().contains("Safari"))
    );

    // Direct scope browses with an empty query.
    let browse = scoped(&mut session, "");
    assert!(
        browse["sections"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["title"] == "Safari" && i["kind"] == "application")
    );
}

#[test]
fn native_activation_runs_once_per_operation_id() {
    let platform = Arc::new(FakePlatform::default());
    let service = service(
        platform.clone(),
        Arc::new(FakeIconResolver::default()),
        vec![app("Safari", "com.apple.safari")],
    );
    let mut session = initialized(&service);
    let view = scoped(&mut session, "Safari");
    let item = &view["sections"][0]["items"][0];
    let actions = item["actions"].as_array().unwrap();
    let find = |id: &str| {
        actions
            .iter()
            .find(|a| a["id"] == id)
            .unwrap_or_else(|| panic!("missing {id} action"))
            .clone()
    };

    assert!(
        platform.calls().is_empty(),
        "searching must not launch or reveal anything"
    );

    let open = json!({
        "viewId": view["viewId"],
        "revision": view["revision"],
        "actionRef": find("open")["actionRef"],
        "operationId": "op-open",
    });
    let first = call(&mut session, "i1", "actions.invoke", open.clone());
    assert_eq!(first["status"], "succeeded");
    assert_eq!(first["presentation"]["type"], "close");
    assert_eq!(platform.calls().len(), 1);
    assert!(platform.calls()[0].starts_with("open:"));

    // Reusing the operation ID returns the retained result without re-running it.
    let repeat = call(&mut session, "i2", "actions.invoke", open);
    assert_eq!(repeat, first);
    assert_eq!(platform.calls().len(), 1);

    let reveal = json!({
        "viewId": view["viewId"],
        "revision": view["revision"],
        "actionRef": find("reveal")["actionRef"],
        "operationId": "op-reveal",
    });
    call(&mut session, "i3", "actions.invoke", reveal);
    assert_eq!(platform.calls().len(), 2);
    assert!(platform.calls()[1].starts_with("reveal:"));
}
