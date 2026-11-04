//! Cached, read-only theme catalog for the shared service.
//!
//! mac-themer's `catalog --json` publishes every installed pack as a resolved,
//! path-free palette (see `lib/macthemer/palette.py`). This module runs that one
//! fixed command through the bounded subprocess runner, validates and dedupes
//! the palettes, and keeps a last-good snapshot plus a degraded status when the
//! helper fails. It never writes desktop state, changes the active theme, or
//! exposes helper stderr or filesystem paths.

use crate::core::CancellationToken;
use crate::macos::process::run_bounded;
use crate::protocol::ThemePalette;
use anyhow::Result;
use serde::Deserialize;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// How often the worker re-runs the catalog when nothing wakes it.
const REFRESH_INTERVAL: Duration = Duration::from_secs(15);
/// Hard deadline for one helper invocation.
const HELPER_TIMEOUT: Duration = Duration::from_secs(3);
/// Reject a catalog whose stdout exceeds this many bytes. The runner caps the
/// stream at `+1` so an over-limit payload is observable rather than silently
/// truncated.
const MAX_CATALOG_BYTES: usize = 2 * 1024 * 1024;
const MAX_THEMES: usize = 128;
/// Homebrew Python must precede Apple's older system Python. Other environment
/// variables, including mac-themer overrides, are inherited unchanged.
const HELPER_PATH_PREFIX: &str = "/opt/homebrew/bin:/usr/local/bin:";

/// One immutable view of the cache. Cheap to clone: the palette list is shared.
#[derive(Clone)]
pub struct CatalogSnapshot {
    pub revision: u64,
    pub palettes: Arc<Vec<ThemePalette>>,
    pub error: Option<String>,
}

struct Inner {
    current: Mutex<CatalogSnapshot>,
    stop: AtomicBool,
    /// Shared with each `run_bounded` call so dropping the cache aborts an
    /// in-flight helper instead of waiting out the deadline.
    cancel: CancellationToken,
    wake: Mutex<Option<mpsc::Sender<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Shared, read-only theme catalog. Cheap to clone behind an `Arc`.
pub struct ThemeCatalog {
    inner: Arc<Inner>,
}

impl ThemeCatalog {
    /// Build an in-memory catalog. Never starts a process, reads the
    /// environment, or spawns a worker; used by fixture services and tests.
    pub fn from_palettes(palettes: Vec<ThemePalette>) -> Result<Arc<Self>> {
        let palettes = Arc::new(accept(palettes).map_err(anyhow::Error::msg)?);
        let revision = revision_for(&palettes, None);
        Ok(Arc::new(Self {
            inner: Arc::new(Inner {
                current: Mutex::new(CatalogSnapshot {
                    revision,
                    palettes,
                    error: None,
                }),
                stop: AtomicBool::new(false),
                cancel: CancellationToken::default(),
                wake: Mutex::new(None),
                worker: Mutex::new(None),
            }),
        }))
    }

    /// Run the helper once synchronously, then reconcile every
    /// [`REFRESH_INTERVAL`] on a background thread until the cache is dropped.
    pub fn start(helper: PathBuf) -> Arc<Self> {
        let (wake, receiver) = mpsc::channel::<()>();
        let inner = Arc::new(Inner {
            current: Mutex::new(empty_snapshot()),
            stop: AtomicBool::new(false),
            cancel: CancellationToken::default(),
            wake: Mutex::new(Some(wake)),
            worker: Mutex::new(None),
        });
        reconcile(&inner, &helper);
        let worker_inner = inner.clone();
        let handle = thread::Builder::new()
            .name("maccer-theme-catalog".into())
            .spawn(move || run_worker(worker_inner, helper, receiver))
            .expect("spawning mac-themer catalog worker");
        *inner.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Arc::new(Self { inner })
    }

    /// Clone the current snapshot. Never blocks on the helper.
    pub fn snapshot(&self) -> CatalogSnapshot {
        self.inner
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for ThemeCatalog {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Release);
        // Abort an in-flight helper so join cannot wait out its deadline.
        self.inner.cancel.cancel();
        if let Some(wake) = self
            .inner
            .wake
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = wake.send(());
        }
        if let Some(handle) = self
            .inner
            .worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

fn run_worker(inner: Arc<Inner>, helper: PathBuf, receiver: mpsc::Receiver<()>) {
    loop {
        match receiver.recv_timeout(REFRESH_INTERVAL) {
            // A wake means the cache is being dropped.
            Ok(()) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if inner.stop.load(Ordering::Acquire) {
            break;
        }
        reconcile(&inner, &helper);
    }
}

fn reconcile(inner: &Inner, helper: &Path) {
    apply(inner, load(helper, &inner.cancel));
}

/// Merge one helper outcome into the cache. Split from [`reconcile`] so the
/// last-good and revision rules can be unit tested without a subprocess.
fn apply(inner: &Inner, result: std::result::Result<Vec<ThemePalette>, String>) {
    let mut current = inner.current.lock().unwrap_or_else(|e| e.into_inner());
    let next = match result {
        Ok(palettes) => {
            let palettes = Arc::new(palettes);
            let revision = revision_for(&palettes, None);
            if current.revision == revision {
                return;
            }
            CatalogSnapshot {
                revision,
                palettes,
                error: None,
            }
        }
        // A failure keeps the last good palettes but records a public status.
        Err(message) => {
            let revision = revision_for(&current.palettes, Some(&message));
            if current.revision == revision {
                return;
            }
            CatalogSnapshot {
                revision,
                palettes: current.palettes.clone(),
                error: Some(message),
            }
        }
    };
    *current = next;
}

fn empty_snapshot() -> CatalogSnapshot {
    let palettes: Arc<Vec<ThemePalette>> = Arc::new(Vec::new());
    let revision = revision_for(&palettes, None);
    CatalogSnapshot {
        revision,
        palettes,
        error: None,
    }
}

/// Run the one fixed command and project its stdout into validated palettes.
/// Every error is a fixed public message; stderr and paths never escape.
fn load(
    helper: &Path,
    cancel: &CancellationToken,
) -> std::result::Result<Vec<ThemePalette>, String> {
    let mut command = Command::new(helper);
    command.arg("catalog").arg("--json");
    command.env("PATH", helper_path());
    let output = run_bounded(&mut command, cancel, HELPER_TIMEOUT, MAX_CATALOG_BYTES + 1)
        .map_err(|_| "Theme catalog helper could not run.".to_owned())?;
    if !output.status.success() {
        return Err("Theme catalog helper failed.".to_owned());
    }
    if output.stdout.len() > MAX_CATALOG_BYTES {
        return Err("Theme catalog response is too large.".to_owned());
    }
    parse(&output.stdout)
}

fn helper_path() -> OsString {
    let mut path = OsString::from(HELPER_PATH_PREFIX);
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(existing);
    }
    path
}

#[derive(Deserialize)]
struct Document {
    schema: u16,
    themes: Vec<ThemePalette>,
}

fn parse(bytes: &[u8]) -> std::result::Result<Vec<ThemePalette>, String> {
    let document: Document = serde_json::from_slice(bytes)
        .map_err(|_| "Theme catalog response is invalid.".to_owned())?;
    if document.schema != 1 {
        return Err("Theme catalog response is invalid.".to_owned());
    }
    accept(document.themes)
}

/// Validate every palette, reject duplicates, and sort by slug so the revision
/// does not depend on the helper's output order.
fn accept(mut palettes: Vec<ThemePalette>) -> std::result::Result<Vec<ThemePalette>, String> {
    if palettes.len() > MAX_THEMES {
        return Err("Theme catalog has too many packs.".to_owned());
    }
    for palette in &palettes {
        palette
            .validate()
            .map_err(|_| "Theme catalog contains an invalid pack.".to_owned())?;
        if !valid_slug(&palette.slug) {
            return Err("Theme catalog contains an invalid pack.".to_owned());
        }
    }
    palettes.sort_by(|a, b| a.slug.cmp(&b.slug));
    for pair in palettes.windows(2) {
        if pair[0].slug == pair[1].slug {
            return Err("Theme catalog contains a duplicate pack.".to_owned());
        }
    }
    Ok(palettes)
}

/// `[a-z0-9][a-z0-9_-]{0,127}`, matching the producer contract.
fn valid_slug(slug: &str) -> bool {
    let bytes = slug.as_bytes();
    if bytes.is_empty() || bytes.len() > 128 {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
    })
}

/// Content hash over the accepted palettes and public status. Identical
/// rewrites, reordered output, and repeated failures do not churn it.
fn revision_for(palettes: &[ThemePalette], error: Option<&str>) -> u64 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(palettes.len() as u64).to_le_bytes());
    for palette in palettes {
        bytes.extend_from_slice(&serde_json::to_vec(palette).unwrap_or_default());
    }
    match error {
        Some(message) => {
            bytes.push(1);
            bytes.extend_from_slice(message.as_bytes());
        }
        None => bytes.push(0),
    }
    fnv1a(&bytes)
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS,
    };
    use std::collections::BTreeMap;

    fn palette(slug: &str) -> ThemePalette {
        let map = |keys: &[&str]| {
            keys.iter()
                .map(|key| ((*key).to_owned(), "#123456".to_owned()))
                .collect::<BTreeMap<_, _>>()
        };
        ThemePalette {
            schema: 1,
            slug: slug.into(),
            name: "Test Pack".into(),
            variant: "dark".into(),
            alpha_byte: 230,
            palette: map(&THEME_PALETTE_KEYS),
            tokens: map(&THEME_TOKEN_KEYS),
            border: map(&THEME_BORDER_KEYS),
            bar: map(&THEME_BAR_KEYS),
        }
    }

    fn cache(palettes: Vec<ThemePalette>) -> Arc<ThemeCatalog> {
        ThemeCatalog::from_palettes(palettes).unwrap()
    }

    #[test]
    fn accepted_catalog_is_sorted_and_revision_is_order_independent() {
        let one = cache(vec![palette("b-pack"), palette("a-pack")]);
        let two = cache(vec![palette("a-pack"), palette("b-pack")]);
        let snapshot = one.snapshot();
        assert_eq!(
            snapshot
                .palettes
                .iter()
                .map(|p| p.slug.as_str())
                .collect::<Vec<_>>(),
            ["a-pack", "b-pack"]
        );
        assert_eq!(snapshot.revision, two.snapshot().revision);
        assert!(snapshot.error.is_none());
    }

    #[test]
    fn invalid_and_duplicate_palettes_are_rejected() {
        assert!(ThemeCatalog::from_palettes(vec![palette("Bad Slug")]).is_err());
        assert!(ThemeCatalog::from_palettes(vec![palette("dup"), palette("dup")]).is_err());
        let mut broken = palette("pack");
        broken.palette.remove("background");
        assert!(ThemeCatalog::from_palettes(vec![broken]).is_err());
    }

    #[test]
    fn failure_retains_last_good_and_churns_revision_once() {
        // `apply` is the unit under test: no helper process is involved.
        let inner = Inner {
            current: Mutex::new(empty_snapshot()),
            stop: AtomicBool::new(false),
            cancel: CancellationToken::default(),
            wake: Mutex::new(None),
            worker: Mutex::new(None),
        };
        apply(&inner, Ok(vec![palette("a-pack")]));
        let good = {
            let current = inner.current.lock().unwrap();
            assert_eq!(current.palettes.len(), 1);
            assert!(current.error.is_none());
            current.revision
        };
        apply(&inner, Err("Theme catalog helper failed.".into()));
        let degraded = {
            let current = inner.current.lock().unwrap();
            assert_eq!(current.palettes.len(), 1, "last good is retained");
            assert_eq!(
                current.error.as_deref(),
                Some("Theme catalog helper failed.")
            );
            assert_ne!(current.revision, good);
            current.revision
        };
        // A repeated identical failure does not churn.
        apply(&inner, Err("Theme catalog helper failed.".into()));
        assert_eq!(inner.current.lock().unwrap().revision, degraded);
        // Recovery with the same palettes clears the status.
        apply(&inner, Ok(vec![palette("a-pack")]));
        let current = inner.current.lock().unwrap();
        assert!(current.error.is_none());
        assert_eq!(current.revision, good);
    }

    #[test]
    fn parse_ignores_unknown_metadata_and_rejects_padded_palettes() {
        let wire = serde_json::json!({
            "schema": 1,
            "generated_by": "mac-themer test",
            "themes": [{
                "schema": 1,
                "slug": "pack",
                "name": "Pack",
                "variant": "dark",
                "alpha_byte": 230,
                "pack": "/private/secret",
                "backgrounds": "/private/secret/backgrounds",
                "palette": serde_json::to_value(palette("pack").palette).unwrap(),
                "tokens": serde_json::to_value(palette("pack").tokens).unwrap(),
                "border": serde_json::to_value(palette("pack").border).unwrap(),
                "bar": serde_json::to_value(palette("pack").bar).unwrap(),
            }],
        });
        let accepted = parse(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(accepted.len(), 1);
        // A padded palette map is a partial/unknown shape and is rejected.
        let mut padded = wire;
        padded["themes"][0]["palette"]["secret_extra"] = serde_json::json!("#000000");
        assert!(parse(&serde_json::to_vec(&padded).unwrap()).is_err());
    }

    #[test]
    fn supported_producer_fixture_matches_the_consumer_contract() {
        let palettes = parse(include_bytes!("../../fixtures/theme-catalog.json")).unwrap();
        assert_eq!(
            palettes.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>(),
            ["catppuccin", "gruvbox", "osaka-jade", "tokyo-night"]
        );
        assert_eq!(palettes[0].palette["accent"], "#cba6f7");
        assert!(palettes.iter().all(|p| p.validate().is_ok()));
    }

    #[test]
    fn wrong_schema_and_malformed_documents_are_rejected() {
        assert!(parse(b"not json").is_err());
        assert!(
            parse(br#"{"schema":2,"themes":[]}"#).is_err(),
            "unsupported schema"
        );
    }
}
