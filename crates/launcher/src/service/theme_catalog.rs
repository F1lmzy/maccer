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
use crate::protocol::{ThemePalette, is_theme_slug};
use anyhow::Result;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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
/// Most wallpapers one pack may publish. A frontend list is not a file browser.
const MAX_WALLPAPERS_PER_THEME: usize = 64;
/// Homebrew Python must precede Apple's older system Python. Other environment
/// variables, including mac-themer overrides, are inherited unchanged.
const HELPER_PATH_PREFIX: &str = "/opt/homebrew/bin:/usr/local/bin:";

/// Wallpaper identities per theme slug, in the producer's cycle order.
pub type WallpaperMap = BTreeMap<String, Vec<String>>;

/// One immutable view of the cache. Cheap to clone: the palette list is shared.
#[derive(Clone)]
pub struct CatalogSnapshot {
    pub revision: u64,
    pub palettes: Arc<Vec<ThemePalette>>,
    /// Wallpapers each pack ships. A pack with none is absent from the map.
    pub wallpapers: Arc<WallpaperMap>,
    pub error: Option<String>,
}
impl CatalogSnapshot {
    /// Published wallpapers for one theme, in apply order. Empty when it ships none.
    pub fn wallpapers_for(&self, slug: &str) -> &[String] {
        self.wallpapers
            .get(slug)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

struct Inner {
    current: Mutex<CatalogSnapshot>,
    /// The helper this cache reconciles through. An in-memory catalog has none,
    /// so nothing can be started on its behalf.
    helper: Option<PathBuf>,
    /// How many reconciles have started. A result is only published when no
    /// later reconcile has begun, so a slow periodic read cannot overwrite the
    /// snapshot a mutation just forced.
    sequence: AtomicU64,
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
        Self::from_catalog(palettes, WallpaperMap::new())
    }

    /// In-memory catalog with the wallpapers each pack ships.
    pub fn from_catalog(
        palettes: Vec<ThemePalette>,
        wallpapers: WallpaperMap,
    ) -> Result<Arc<Self>> {
        let (palettes, wallpapers) = accept(palettes, wallpapers).map_err(anyhow::Error::msg)?;
        let palettes = Arc::new(palettes);
        let wallpapers = Arc::new(wallpapers);
        let revision = revision_for(&palettes, &wallpapers, None);
        Ok(Arc::new(Self {
            inner: Arc::new(Inner {
                current: Mutex::new(CatalogSnapshot {
                    revision,
                    palettes,
                    wallpapers,
                    error: None,
                }),
                helper: None,
                sequence: AtomicU64::new(0),
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
            helper: Some(helper.clone()),
            sequence: AtomicU64::new(0),
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

    /// Reload the collection now instead of at the next periodic reconcile.
    ///
    /// A mutation that changes the collection calls this, so the pack it just
    /// installed or removed is visible to the next query rather than up to one
    /// refresh interval later. An in-memory catalog has no helper and does
    /// nothing.
    pub fn refresh(&self) {
        if let Some(helper) = self.inner.helper.as_deref() {
            reconcile(&self.inner, helper);
        }
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
    // Load outside the lock, then publish only if this is still the newest
    // read. A periodic reconcile and a forced one can overlap, and the slower
    // of the two must not rewind the cache past the faster.
    let sequence = inner.sequence.fetch_add(1, Ordering::AcqRel) + 1;
    let result = load(helper, &inner.cancel);
    if inner.sequence.load(Ordering::Acquire) != sequence {
        return;
    }
    apply(inner, result);
}

/// Merge one helper outcome into the cache. Split from [`reconcile`] so the
/// last-good and revision rules can be unit tested without a subprocess.
fn apply(inner: &Inner, result: std::result::Result<(Vec<ThemePalette>, WallpaperMap), String>) {
    let mut current = inner.current.lock().unwrap_or_else(|e| e.into_inner());
    let next = match result {
        Ok((palettes, wallpapers)) => {
            let palettes = Arc::new(palettes);
            let wallpapers = Arc::new(wallpapers);
            let revision = revision_for(&palettes, &wallpapers, None);
            if current.revision == revision {
                return;
            }
            CatalogSnapshot {
                revision,
                palettes,
                wallpapers,
                error: None,
            }
        }
        // A failure keeps the last good palettes but records a public status.
        Err(message) => {
            let revision = revision_for(&current.palettes, &current.wallpapers, Some(&message));
            if current.revision == revision {
                return;
            }
            CatalogSnapshot {
                revision,
                palettes: current.palettes.clone(),
                wallpapers: current.wallpapers.clone(),
                error: Some(message),
            }
        }
    };
    *current = next;
}

fn empty_snapshot() -> CatalogSnapshot {
    let palettes: Arc<Vec<ThemePalette>> = Arc::new(Vec::new());
    let wallpapers: Arc<WallpaperMap> = Arc::new(WallpaperMap::new());
    let revision = revision_for(&palettes, &wallpapers, None);
    CatalogSnapshot {
        revision,
        palettes,
        wallpapers,
        error: None,
    }
}

/// Run the one fixed command and project its stdout into validated palettes and
/// the wallpapers each pack ships. Every error is a fixed public message; stderr
/// and paths never escape.
fn load(
    helper: &Path,
    cancel: &CancellationToken,
) -> std::result::Result<(Vec<ThemePalette>, WallpaperMap), String> {
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
    /// Absent from a producer that does not publish wallpapers yet.
    #[serde(default)]
    wallpapers: WallpaperMap,
}

fn parse(bytes: &[u8]) -> std::result::Result<(Vec<ThemePalette>, WallpaperMap), String> {
    let document: Document = serde_json::from_slice(bytes)
        .map_err(|_| "Theme catalog response is invalid.".to_owned())?;
    if document.schema != 1 {
        return Err("Theme catalog response is invalid.".to_owned());
    }
    accept(document.themes, document.wallpapers)
}

/// Validate every palette and wallpaper identity, reject duplicates, and sort
/// by slug so the revision does not depend on the helper's output order.
fn accept(
    mut palettes: Vec<ThemePalette>,
    wallpapers: WallpaperMap,
) -> std::result::Result<(Vec<ThemePalette>, WallpaperMap), String> {
    if palettes.len() > MAX_THEMES {
        return Err("Theme catalog has too many packs.".to_owned());
    }
    for palette in &palettes {
        palette
            .validate()
            .map_err(|_| "Theme catalog contains an invalid pack.".to_owned())?;
        if !is_theme_slug(&palette.slug) {
            return Err("Theme catalog contains an invalid pack.".to_owned());
        }
    }
    palettes.sort_by(|a, b| a.slug.cmp(&b.slug));
    for pair in palettes.windows(2) {
        if pair[0].slug == pair[1].slug {
            return Err("Theme catalog contains a duplicate pack.".to_owned());
        }
    }
    let mut accepted = WallpaperMap::new();
    for (slug, ids) in wallpapers {
        // A wallpaper that belongs to no published pack is an association the
        // frontend could never resolve, so it fails the catalog rather than
        // being dropped.
        if !palettes.iter().any(|palette| palette.slug == slug) {
            return Err("Theme catalog has a wallpaper for an unknown pack.".to_owned());
        }
        if ids.len() > MAX_WALLPAPERS_PER_THEME {
            return Err("Theme catalog has too many wallpapers for a pack.".to_owned());
        }
        let mut seen = BTreeSet::new();
        for id in &ids {
            if !crate::protocol::is_wallpaper_id(id) {
                return Err("Theme catalog contains an invalid wallpaper.".to_owned());
            }
            if !seen.insert(id.clone()) {
                return Err("Theme catalog contains a duplicate wallpaper.".to_owned());
            }
        }
        accepted.insert(slug, ids);
    }
    Ok((palettes, accepted))
}

/// Content hash over the accepted palettes, wallpapers and public status.
/// Identical rewrites, reordered output, and repeated failures do not churn it.
fn revision_for(palettes: &[ThemePalette], wallpapers: &WallpaperMap, error: Option<&str>) -> u64 {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(palettes.len() as u64).to_le_bytes());
    for palette in palettes {
        bytes.extend_from_slice(&serde_json::to_vec(palette).unwrap_or_default());
    }
    bytes.extend_from_slice(&(wallpapers.len() as u64).to_le_bytes());
    for (slug, ids) in wallpapers {
        bytes.extend_from_slice(slug.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&(ids.len() as u64).to_le_bytes());
        for id in ids {
            bytes.extend_from_slice(id.as_bytes());
            bytes.push(0);
        }
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

    fn wallpapers(pairs: &[(&str, &[&str])]) -> WallpaperMap {
        pairs
            .iter()
            .map(|(slug, ids)| {
                (
                    (*slug).to_owned(),
                    ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>(),
                )
            })
            .collect()
    }

    #[test]
    fn a_pack_publishes_its_wallpapers_in_the_producers_order() {
        let catalog = ThemeCatalog::from_catalog(
            vec![palette("a-pack"), palette("b-pack")],
            wallpapers(&[("a-pack", &["2-b.webp", "1-a.jpg"])]),
        )
        .unwrap();
        let snapshot = catalog.snapshot();
        assert_eq!(snapshot.wallpapers_for("a-pack"), ["2-b.webp", "1-a.jpg"]);
        // A pack with none is absent, and asks the frontend for nothing.
        assert!(snapshot.wallpapers_for("b-pack").is_empty());
        assert!(snapshot.wallpapers_for("missing").is_empty());
    }

    #[test]
    fn wallpapers_take_part_in_the_revision() {
        let palettes = || vec![palette("a-pack")];
        let plain = ThemeCatalog::from_catalog(palettes(), WallpaperMap::new()).unwrap();
        let with = ThemeCatalog::from_catalog(palettes(), wallpapers(&[("a-pack", &["1-a.webp"])]))
            .unwrap();
        let swapped =
            ThemeCatalog::from_catalog(palettes(), wallpapers(&[("a-pack", &["2-b.webp"])]))
                .unwrap();
        assert_ne!(plain.snapshot().revision, with.snapshot().revision);
        assert_ne!(with.snapshot().revision, swapped.snapshot().revision);
    }

    #[test]
    fn wallpaper_associations_are_validated() {
        // An association to a pack that is not in the catalog can never resolve.
        assert!(
            ThemeCatalog::from_catalog(
                vec![palette("a-pack")],
                wallpapers(&[("ghost", &["1-a.webp"])]),
            )
            .is_err()
        );
        for bad in ["", ".", "..", "../1-a.webp", "a/b.webp", "tab\tname.webp"] {
            assert!(
                ThemeCatalog::from_catalog(
                    vec![palette("a-pack")],
                    wallpapers(&[("a-pack", &[bad])]),
                )
                .is_err(),
                "accepted {bad:?}"
            );
        }
        assert!(
            ThemeCatalog::from_catalog(
                vec![palette("a-pack")],
                wallpapers(&[("a-pack", &["1-a.webp", "1-a.webp"])]),
            )
            .is_err()
        );
        let too_many = vec!["1-a.webp"; MAX_WALLPAPERS_PER_THEME + 1]
            .into_iter()
            .enumerate()
            .map(|(index, _)| format!("{index}.webp"))
            .collect::<Vec<_>>();
        let too_many: Vec<&str> = too_many.iter().map(String::as_str).collect();
        assert!(
            ThemeCatalog::from_catalog(
                vec![palette("a-pack")],
                wallpapers(&[("a-pack", &too_many)]),
            )
            .is_err()
        );
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
            helper: None,
            sequence: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            cancel: CancellationToken::default(),
            wake: Mutex::new(None),
            worker: Mutex::new(None),
        };
        apply(&inner, Ok((vec![palette("a-pack")], WallpaperMap::new())));
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
        apply(&inner, Ok((vec![palette("a-pack")], WallpaperMap::new())));
        let current = inner.current.lock().unwrap();
        assert!(current.error.is_none());
        assert_eq!(current.revision, good);
    }

    #[test]
    fn a_failure_retains_the_last_good_wallpapers() {
        let inner = Inner {
            current: Mutex::new(empty_snapshot()),
            helper: None,
            sequence: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            cancel: CancellationToken::default(),
            wake: Mutex::new(None),
            worker: Mutex::new(None),
        };
        apply(
            &inner,
            Ok((
                vec![palette("a-pack")],
                wallpapers(&[("a-pack", &["1-a.webp"])]),
            )),
        );
        apply(&inner, Err("Theme catalog helper failed.".into()));
        let current = inner.current.lock().unwrap();
        assert_eq!(current.wallpapers_for("a-pack"), ["1-a.webp"]);
        assert!(current.error.is_some());
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
        assert_eq!(accepted.0.len(), 1);
        assert!(accepted.1.is_empty(), "no wallpapers were published");
        // A padded palette map is a partial/unknown shape and is rejected.
        let mut padded = wire;
        padded["themes"][0]["palette"]["secret_extra"] = serde_json::json!("#000000");
        assert!(parse(&serde_json::to_vec(&padded).unwrap()).is_err());
    }

    #[test]
    fn wallpapers_for_an_unknown_pack_fail_the_catalog() {
        let wire = serde_json::json!({
            "schema": 1,
            "themes": [{
                "schema": 1,
                "slug": "pack",
                "name": "Pack",
                "variant": "dark",
                "alpha_byte": 230,
                "palette": serde_json::to_value(palette("pack").palette).unwrap(),
                "tokens": serde_json::to_value(palette("pack").tokens).unwrap(),
                "border": serde_json::to_value(palette("pack").border).unwrap(),
                "bar": serde_json::to_value(palette("pack").bar).unwrap(),
            }],
            "wallpapers": {"ghost": ["1-a.webp"]},
        });
        assert!(parse(&serde_json::to_vec(&wire).unwrap()).is_err());
    }

    #[test]
    fn supported_producer_fixture_matches_the_consumer_contract() {
        let (palettes, wallpapers) =
            parse(include_bytes!("../../fixtures/theme-catalog.json")).unwrap();
        assert_eq!(
            palettes.iter().map(|p| p.slug.as_str()).collect::<Vec<_>>(),
            ["catppuccin", "gruvbox", "osaka-jade", "tokyo-night"]
        );
        assert_eq!(palettes[0].palette["accent"], "#cba6f7");
        assert!(palettes.iter().all(|p| p.validate().is_ok()));
        // Every pack in the fixture publishes at least one wallpaper, and the
        // fixture keeps the producer's cycle order rather than sorting.
        assert_eq!(wallpapers.len(), palettes.len());
        assert_eq!(
            wallpapers["catppuccin"],
            ["1-totoro.webp", "2-waves.webp", "3-blue-eye.webp"]
        );
        assert_eq!(
            wallpapers["tokyo-night"].first().unwrap(),
            "0-winding-road.webp"
        );
    }

    #[test]
    fn a_parsed_palette_serializes_back_to_the_published_entry() {
        // `mac-themer apply` compares the expected palette byte for byte, so
        // what the service sends back must be exactly what the producer
        // published. A new field here that is not modelled would silently turn
        // every Apply into "catalog changed".
        let fixture = include_bytes!("../../fixtures/theme-catalog.json");
        let document: serde_json::Value = serde_json::from_slice(fixture).unwrap();
        let (palettes, _) = parse(fixture).unwrap();
        for (palette, published) in palettes.iter().zip(document["themes"].as_array().unwrap()) {
            assert_eq!(
                &serde_json::to_value(palette).unwrap(),
                published,
                "{} must be passed back unchanged",
                palette.slug
            );
        }
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
