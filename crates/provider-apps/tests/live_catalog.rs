use anyhow::Result;
use launcher_core::{Action, ActionId, CancellationToken, Provider, SearchContext, SearchQuery};
use launcher_macos::Platform;
use provider_apps::ApplicationProvider;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct TestRoot(PathBuf);
impl TestRoot {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "maccer-live-apps-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(root.canonicalize().unwrap())
    }
    fn bundle(&self, name: &str, id: &str) -> PathBuf {
        let bundle = self.0.join(format!("{name}.app"));
        fs::create_dir_all(bundle.join("Contents")).unwrap();
        fs::write(bundle.join("Contents/Info.plist"), format!(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{id}</string><key>CFBundleName</key><string>{name}</string></dict></plist>"#)).unwrap();
        bundle
    }
}
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct FakePlatform(AtomicUsize);
impl Platform for FakePlatform {
    fn launch_application(&self, _: &Path) -> Result<()> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    fn reveal_in_finder(&self, _: &Path) -> Result<()> {
        Ok(())
    }
    fn open_url(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn copy_text(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

fn results(provider: &ApplicationProvider) -> Vec<launcher_core::Item> {
    provider
        .search(
            &SearchQuery::default(),
            &SearchContext {
                generation: 1,
                cancellation: CancellationToken::default(),
                limit: 50,
            },
        )
        .unwrap()
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "catalog did not refresh");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn a_live_catalog_adds_removes_and_renames_apps_without_restarting() {
    let root = TestRoot::new();
    root.bundle("Alpha", "dev.test.alpha");
    let platform = Arc::new(FakePlatform::default());
    let provider = ApplicationProvider::watched(vec![root.0.clone()], platform.clone()).unwrap();
    assert_eq!(results(&provider).len(), 1, "initial index is synchronous");
    let initial_revision = provider.revision();

    let beta_path = root.bundle("Beta", "dev.test.beta");
    wait_until(|| results(&provider).iter().any(|item| item.title == "Beta"));
    assert!(provider.revision() > initial_revision);
    let old_beta = results(&provider)
        .into_iter()
        .find(|item| item.title == "Beta")
        .unwrap();

    fs::rename(&beta_path, root.0.join("Renamed.app")).unwrap();
    wait_until(|| {
        results(&provider).iter().any(|item| {
            item.payload["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("Renamed.app"))
        })
    });
    assert!(
        provider.actions(&old_beta).is_empty(),
        "stale paths cannot be activated"
    );
    fs::remove_dir_all(root.0.join("Renamed.app")).unwrap();
    wait_until(|| results(&provider).len() == 1);
    assert!(
        provider
            .activate(
                &old_beta,
                &Action {
                    id: ActionId("open".into()),
                    title: "Open".into()
                }
            )
            .is_err()
    );
    assert_eq!(platform.0.load(Ordering::Relaxed), 0);

    // An unrelated write can cause a rescan but cannot republish the same
    // catalog or restart the UI's revision-driven search.
    let revision = provider.revision();
    fs::write(root.0.join("unrelated.txt"), "irrelevant").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(provider.revision(), revision);
    drop(provider);
}

#[test]
fn a_fixed_catalog_can_search_after_its_bundle_is_deleted() {
    let root = TestRoot::new();
    root.bundle("Alpha", "dev.test.alpha");
    let provider = ApplicationProvider::new(
        launcher_macos::discover_in(std::slice::from_ref(&root.0)).unwrap(),
        Arc::new(FakePlatform::default()),
    );
    fs::remove_dir_all(root.0.join("Alpha.app")).unwrap();
    assert_eq!(
        results(&provider)[0].title,
        "Alpha",
        "search uses only prebuilt records"
    );
    assert_eq!(provider.revision(), 0);
}
