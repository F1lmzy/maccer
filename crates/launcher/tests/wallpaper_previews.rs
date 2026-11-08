//! Wallpaper thumbnail resolution: one decode per file revision, shared by
//! every caller, and a background pass that leaves it ready.
//!
//! These live in their own binary because the first ImageIO call in a test
//! process pays framework setup. Sharing that cost with the decode tests in
//! the library binary makes those tests queue behind it for a decode permit,
//! so this file keeps its own process.
#![cfg(target_os = "macos")]

use anyhow::Result;
use launcher_agent::macos::ApplicationIconPixels;
use launcher_agent::protocol::REPRESENTATION_PREVIEW;
use launcher_agent::service::{AssetStore, IconResolver};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

struct NoIcons;
impl IconResolver for NoIcons {
    fn resolve(&self, _path: &Path) -> Result<Option<ApplicationIconPixels>> {
        Ok(None)
    }
}

fn store() -> Arc<AssetStore> {
    AssetStore::new(Arc::new(NoIcons))
}

/// One throwaway copy of the 1x1 fixture, canonicalized the way the wallpaper
/// provider resolves a pack path.
fn tiny_png() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wallpaper.png");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pixel.png"),
        &path,
    )
    .unwrap();
    (dir, std::fs::canonicalize(path).unwrap())
}

/// Every caller of one reference shares a single decode: the first claims it,
/// the rest wait for that result instead of decoding the same file again.
#[test]
fn concurrent_callers_share_one_preview_decode() {
    let (_dir, path) = tiny_png();
    let assets = store();
    let reference = assets.reference_preview(&path, 7);
    assert!(!assets.preview_ready(&path, 7), "nothing is warm yet");
    let mut workers = Vec::new();
    for _ in 0..8 {
        let assets = assets.clone();
        let (id, revision) = (reference.id.clone(), reference.revision.clone());
        workers.push(thread::spawn(move || {
            assets
                .get(&id, &revision, REPRESENTATION_PREVIEW)
                .map(|data| data.0)
        }));
    }
    for worker in workers {
        let metadata = worker.join().unwrap().unwrap();
        assert_eq!((metadata.width, metadata.height), (1, 1));
        assert_eq!(metadata.representation, REPRESENTATION_PREVIEW);
    }
    assert!(assets.preview_ready(&path, 7));
}

/// A background pass warms the very reference the projection will publish, so
/// the demand path afterwards only copies pixels.
#[test]
fn prefetching_warms_the_reference_the_projection_will_publish() {
    let (_dir, path) = tiny_png();
    let assets = store();
    assets.prefetch_preview(&path, 3);
    assert!(assets.preview_ready(&path, 3));
    let reference = assets.reference_preview(&path, 3);
    let (metadata, _) = assets
        .get(&reference.id, &reference.revision, REPRESENTATION_PREVIEW)
        .unwrap();
    assert_eq!((metadata.width, metadata.height), (1, 1));
    // Warming again is a no-op, and a new revision is a different asset.
    assets.prefetch_preview(&path, 3);
    assert_eq!(assets.reference_preview(&path, 3).id, reference.id);
    assert!(!assets.preview_ready(&path, 4));
}

/// A failed decode is not remembered as "unavailable": the next request retries
/// it and reports its own error.
#[test]
fn a_failed_preview_is_retried_instead_of_cached() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("not-an-image.png");
    std::fs::write(&path, b"not an image").unwrap();
    let assets = store();
    let reference = assets.reference_preview(&path, 1);
    for _ in 0..2 {
        assert!(
            assets
                .get(&reference.id, &reference.revision, REPRESENTATION_PREVIEW)
                .is_err()
        );
    }
    assert!(!assets.preview_ready(&path, 1));
    // A size class cannot be served from the other one.
    let icon = assets.reference(Path::new("/private/x.app"), 1);
    assert!(
        assets
            .get(&icon.id, &icon.revision, REPRESENTATION_PREVIEW)
            .unwrap_err()
            .to_string()
            .contains("ASSET_UNSUPPORTED_REPRESENTATION")
    );
    // Icon extraction stays serialized and a missing icon is cached, so the
    // concurrency added for previews cannot turn into repeat AppKit calls.
    let calls = Arc::new(AtomicUsize::new(0));
    struct Counting(Arc<AtomicUsize>);
    impl IconResolver for Counting {
        fn resolve(&self, _path: &Path) -> Result<Option<ApplicationIconPixels>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(None)
        }
    }
    let counting = AssetStore::new(Arc::new(Counting(calls.clone())));
    let missing = counting.reference(Path::new("/private/missing.app"), 2);
    for _ in 0..2 {
        assert!(
            counting
                .get(&missing.id, &missing.revision, "bgra8")
                .is_err()
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "a missing icon is cached");
}
