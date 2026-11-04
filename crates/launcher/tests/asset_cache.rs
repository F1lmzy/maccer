use anyhow::Result;
use launcher_agent::macos::ApplicationIconPixels;
use launcher_agent::service::{AssetStore, IconResolver};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Resolver(AtomicUsize);
impl IconResolver for Resolver {
    fn resolve(&self, _: &Path) -> Result<Option<ApplicationIconPixels>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Some(ApplicationIconPixels {
            bgra: vec![1, 2, 3, 4],
            width: 1,
            height: 1,
        }))
    }
}
#[test]
fn unchanged_icon_references_are_shared_across_view_refreshes() {
    let resolver = Arc::new(Resolver(AtomicUsize::new(0)));
    let store = AssetStore::new(resolver.clone());
    let first = store.reference(Path::new("/private/app.app"), 1);
    store.get(&first.id, &first.revision).unwrap();
    let refreshed = store.reference(Path::new("/private/app.app"), 1);
    assert_eq!(
        first.id, refreshed.id,
        "unchanged views must reuse resolved icon assets"
    );
    store.get(&refreshed.id, &refreshed.revision).unwrap();
    assert_eq!(resolver.0.load(Ordering::SeqCst), 1);
    let changed = store.reference(Path::new("/private/app.app"), 2);
    assert_ne!(changed.id, first.id);
}
#[test]
fn repeated_references_do_not_exhaust_the_asset_cache() {
    let store = AssetStore::new(Arc::new(Resolver(AtomicUsize::new(0))));
    let first = store.reference(Path::new("/private/app.app"), 1);
    for _ in 0..1000 {
        store.reference(Path::new("/private/app.app"), 1);
    }
    assert!(store.get(&first.id, &first.revision).is_ok());
}
