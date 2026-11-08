//! Bounded, read-only reader for the mac-themer resolved palette.
//!
//! mac-themer publishes `state_dir/current.json` atomically (see its
//! `lib/macthemer/{paths,render,apply}.py`). This module projects only the
//! documented schema-1 color fields into protocol data, keeps the last good
//! palette when a newer file is missing or invalid, and watches the directory
//! for replacements. It never writes desktop state, runs a helper, or exposes
//! filesystem paths.

use crate::protocol::{
    MAX_THEME_STATE_BYTES, THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS,
    ThemePalette, ThemeState, ThemeStateError,
};
use anyhow::Result;
use notify::{RecursiveMode, Watcher};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

/// How often the reader reconciles even without a filesystem event. Bounded so
/// a missed or coalesced event still converges quickly.
const RECONCILE_INTERVAL: Duration = Duration::from_millis(1000);

struct Inner {
    current: Mutex<ThemeState>,
    stop: AtomicBool,
    worker: Mutex<Option<JoinHandle<()>>>,
}

/// Shared, read-only active theme state. Cheap to clone behind an `Arc`.
pub struct ThemerState {
    inner: Arc<Inner>,
}

impl ThemerState {
    /// Deterministic in-memory palette for fixture services. Never touches disk.
    pub fn fixture() -> Arc<Self> {
        Self::fixture_named("maccer-fixture")
    }

    /// The fixture palette published under `slug`, for tests that key off the
    /// active theme's identity rather than its colors.
    pub fn fixture_named(slug: &str) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(Inner {
                current: Mutex::new(make_state(Some(fixture_palette(slug)), None)),
                stop: AtomicBool::new(false),
                worker: Mutex::new(None),
            }),
        })
    }

    /// Watch `directory/current.json`, publishing an initial state synchronously
    /// before the worker thread starts reconciling.
    pub fn watch(directory: PathBuf) -> Arc<Self> {
        let inner = Arc::new(Inner {
            current: Mutex::new(make_state(None, None)),
            stop: AtomicBool::new(false),
            worker: Mutex::new(None),
        });
        reconcile(&inner, &directory);
        let worker_inner = inner.clone();
        let handle = thread::Builder::new()
            .name("maccer-themer".into())
            .spawn(move || run_reader(worker_inner, directory))
            .expect("spawning mac-themer reader");
        *inner.worker.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Arc::new(Self { inner })
    }

    /// Clone the current state for a response. Never blocks on disk.
    pub fn snapshot(&self) -> ThemeState {
        self.inner
            .current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for ThemerState {
    fn drop(&mut self) {
        self.inner.stop.store(true, Ordering::Release);
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

fn run_reader(inner: Arc<Inner>, directory: PathBuf) {
    // Capacity one coalesces event bursts: a full channel already means "changed".
    let (signal, receiver) = mpsc::sync_channel::<()>(1);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok() {
            let _ = signal.try_send(());
        }
    })
    .ok();
    let mut watched: Option<PathBuf> = None;
    loop {
        reconcile(&inner, &directory);
        if inner.stop.load(Ordering::Acquire) {
            break;
        }
        if let Some(watcher) = watcher.as_mut() {
            let desired = watch_target(&directory);
            if watched != desired {
                if let Some(previous) = watched.take() {
                    let _ = watcher.unwatch(&previous);
                }
                if let Some(target) = desired
                    && watcher.watch(&target, RecursiveMode::NonRecursive).is_ok()
                {
                    watched = Some(target);
                }
            }
        }
        wait_for_change(&receiver, RECONCILE_INTERVAL);
    }
}

// Failed native watching must retain the polling interval, not spin on reads.
fn wait_for_change(receiver: &mpsc::Receiver<()>, interval: Duration) {
    if matches!(
        receiver.recv_timeout(interval),
        Err(mpsc::RecvTimeoutError::Disconnected)
    ) {
        thread::sleep(interval);
    }
}

/// Watch the state directory itself when it exists, otherwise its parent so a
/// directory that appears later is still observed.
fn watch_target(directory: &Path) -> Option<PathBuf> {
    if directory.is_dir() {
        Some(directory.to_path_buf())
    } else {
        directory
            .parent()
            .filter(|parent| parent.is_dir())
            .map(Path::to_path_buf)
    }
}

fn reconcile(inner: &Inner, directory: &Path) {
    let result = read_palette(&directory.join("current.json"));
    let mut current = inner.current.lock().unwrap_or_else(|e| e.into_inner());
    let next = match result {
        Ok(palette) => make_state(Some(palette), None),
        // Keep last-good on a transient or invalid replacement, but surface why.
        Err(error) => match current.palette.clone() {
            Some(palette) => make_state(Some(palette), Some(error)),
            None => make_state(None, Some(error)),
        },
    };
    // The revision is a content hash, so identical rewrites and unknown extra
    // metadata never churn it.
    if current.revision != next.revision {
        *current = next;
    }
}

fn read_palette(path: &Path) -> Result<ThemePalette, ThemeStateError> {
    // O_NONBLOCK keeps a FIFO from blocking; O_NOFOLLOW refuses a symlink that
    // could point at a device. A regular file ignores both flags.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| ThemeStateError::new("THEME_UNAVAILABLE", "No published theme palette."))?;
    let metadata = file
        .metadata()
        .map_err(|_| invalid("Theme palette could not be inspected."))?;
    if !metadata.is_file() {
        return Err(invalid("Theme palette is not a regular file."));
    }
    if metadata.len() > MAX_THEME_STATE_BYTES as u64 {
        return Err(invalid("Theme palette is too large."));
    }
    let mut bytes = Vec::new();
    file.take(MAX_THEME_STATE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid("Theme palette could not be read."))?;
    if bytes.len() > MAX_THEME_STATE_BYTES {
        return Err(invalid("Theme palette is too large."));
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("Theme palette is not valid JSON."))?;
    project_palette(&value)
}

fn project_palette(value: &Value) -> Result<ThemePalette, ThemeStateError> {
    let root = value
        .as_object()
        .ok_or_else(|| invalid("Theme palette root must be an object."))?;
    let schema = root
        .get("schema")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("Theme palette schema is missing."))?;
    if schema != 1 {
        return Err(ThemeStateError::new(
            "THEME_UNSUPPORTED",
            "Unsupported theme palette schema.",
        ));
    }
    let string = |key: &str| -> Result<String, ThemeStateError> {
        let text = root
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| invalid(format!("Theme palette {key} is missing.")))?;
        if text.is_empty() || text.len() > 256 {
            return Err(invalid(format!("Theme palette {key} is invalid.")));
        }
        Ok(text.to_owned())
    };
    let slug = string("slug")?;
    let name = string("name")?;
    let variant = string("variant")?;
    let alpha = root
        .get("alpha_byte")
        .and_then(Value::as_u64)
        .filter(|value| *value <= 255)
        .ok_or_else(|| invalid("Theme palette alpha_byte is invalid."))?;
    let palette = color_map(root.get("palette"), &THEME_PALETTE_KEYS, "palette")?;
    let tokens = color_map(root.get("tokens"), &THEME_TOKEN_KEYS, "tokens")?;
    let border = color_map(root.get("border"), &THEME_BORDER_KEYS, "border")?;
    let bar = color_map(root.get("bar"), &THEME_BAR_KEYS, "bar")?;
    let projected = ThemePalette {
        schema: 1,
        slug,
        name,
        variant,
        alpha_byte: alpha as u8,
        palette,
        tokens,
        border,
        bar,
    };
    projected
        .validate()
        .map_err(|message| invalid(format!("Theme palette is invalid: {message}")))?;
    Ok(projected)
}

/// Copy only the documented keys, so paths, catalogs and private metadata in
/// the source document never reach the protocol.
fn color_map(
    value: Option<&Value>,
    keys: &[&str],
    group: &str,
) -> Result<BTreeMap<String, String>, ThemeStateError> {
    let object = value
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(format!("Theme palette {group} is missing.")))?;
    let mut map = BTreeMap::new();
    for key in keys {
        let color = object
            .get(*key)
            .and_then(Value::as_str)
            .filter(|color| crate::protocol::is_hex_color(color))
            .ok_or_else(|| invalid(format!("Theme palette {group}.{key} must be #rrggbb.")))?;
        map.insert((*key).to_owned(), color.to_owned());
    }
    Ok(map)
}

fn invalid(message: impl Into<String>) -> ThemeStateError {
    ThemeStateError::new("THEME_INVALID", message)
}

fn make_state(palette: Option<ThemePalette>, error: Option<ThemeStateError>) -> ThemeState {
    let revision = revision_for(palette.as_ref(), error.as_ref());
    ThemeState {
        revision,
        available: palette.is_some(),
        palette,
        error,
    }
}

/// Content hash over the projected palette and error. Unknown metadata is
/// already discarded, so this changes only when rendered state changes.
fn revision_for(palette: Option<&ThemePalette>, error: Option<&ThemeStateError>) -> String {
    let mut bytes = Vec::new();
    match palette {
        Some(palette) => {
            bytes.push(1);
            bytes.extend_from_slice(&serde_json::to_vec(palette).unwrap_or_default());
        }
        None => bytes.push(0),
    }
    match error {
        Some(error) => {
            bytes.push(1);
            bytes.extend_from_slice(&serde_json::to_vec(error).unwrap_or_default());
        }
        None => bytes.push(0),
    }
    format!("{:016x}", fnv1a(&bytes))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

const FIXTURE_ANSI: [&str; 16] = [
    "#1a1b26", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#a9b1d6",
    "#414868", "#f7768e", "#9ece6a", "#e0af68", "#7aa2f7", "#bb9af7", "#7dcfff", "#c0caf5",
];
const FIXTURE_TOKENS: [&str; 19] = [
    "#1a1b26", "#1e2030", "#222436", "#2a2d3d", "#343a52", "#c0caf5", "#a9b1d6", "#565f89",
    "#3b4261", "#7aa2f7", "#1a1b26", "#7aa2f7", "#1a1b26", "#7dcfff", "#7aa2f7", "#9ece6a",
    "#e0af68", "#f7768e", "#565f89",
];
const FIXTURE_BORDER: [&str; 2] = ["#3b4261", "#292e42"];
const FIXTURE_BAR: [&str; 10] = [
    "#1a1b26", "#1a1b26", "#222436", "#7aa2f7", "#c0caf5", "#c0caf5", "#565f89", "#f7768e",
    "#9ece6a", "#e0af68",
];

fn fixture_palette(slug: &str) -> ThemePalette {
    let mut palette = BTreeMap::new();
    palette.insert("background".into(), FIXTURE_ANSI[0].to_owned());
    palette.insert("foreground".into(), "#c0caf5".to_owned());
    palette.insert("cursor".into(), "#7aa2f7".to_owned());
    palette.insert("accent".into(), "#7aa2f7".to_owned());
    palette.insert("selection_background".into(), "#2a2d3d".to_owned());
    palette.insert("selection_foreground".into(), "#c0caf5".to_owned());
    for (index, color) in FIXTURE_ANSI.iter().enumerate() {
        palette.insert(format!("color{index}"), (*color).to_owned());
    }
    let mut tokens = BTreeMap::new();
    for (key, color) in THEME_TOKEN_KEYS.iter().zip(FIXTURE_TOKENS) {
        tokens.insert((*key).to_owned(), color.to_owned());
    }
    let mut border = BTreeMap::new();
    for (key, color) in THEME_BORDER_KEYS.iter().zip(FIXTURE_BORDER) {
        border.insert((*key).to_owned(), color.to_owned());
    }
    let mut bar = BTreeMap::new();
    for (key, color) in THEME_BAR_KEYS.iter().zip(FIXTURE_BAR) {
        bar.insert((*key).to_owned(), color.to_owned());
    }
    ThemePalette {
        schema: 1,
        slug: slug.into(),
        name: "Maccer Fixture".into(),
        variant: "dark".into(),
        alpha_byte: 230,
        palette,
        tokens,
        border,
        bar,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_watcher_signal_keeps_reconciliation_throttled() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);
        let started = std::time::Instant::now();
        wait_for_change(&receiver, Duration::from_millis(10));
        assert!(started.elapsed() >= Duration::from_millis(10));
    }

    #[test]
    fn fixture_palette_is_valid_and_stable() {
        let first = ThemerState::fixture();
        let second = ThemerState::fixture();
        let a = first.snapshot();
        let b = second.snapshot();
        assert!(a.available && a.error.is_none());
        assert_eq!(a.revision, b.revision);
        assert_eq!(a.palette, b.palette);
        a.validate().unwrap();
    }

    #[test]
    fn projecting_a_document_keeps_only_documented_keys() {
        let mut document = serde_json::to_value(fixture_palette("maccer-fixture")).unwrap();
        document["generated_by"] = serde_json::json!("mac-themer fixture");
        document["pack"] = serde_json::json!("themes/secret/../../escape");
        document["backgrounds"] = serde_json::json!("themes/secret/backgrounds");
        document["palette"]["secret_extra"] = serde_json::json!("#000000");
        let projected = project_palette(&document).unwrap();
        assert_eq!(projected.palette.len(), THEME_PALETTE_KEYS.len());
        let wire = serde_json::to_string(&projected).unwrap();
        assert!(
            !wire.contains("escape") && !wire.contains("pack") && !wire.contains("secret_extra")
        );
    }

    #[test]
    fn invalid_documents_are_rejected() {
        let base = serde_json::to_value(fixture_palette("maccer-fixture")).unwrap();
        for mutate in [
            |v: &mut Value| v["schema"] = serde_json::json!(2),
            |v: &mut Value| v["variant"] = serde_json::json!("sepia"),
            |v: &mut Value| v["alpha_byte"] = serde_json::json!(256),
            |v: &mut Value| v["palette"]["background"] = serde_json::json!("#fff"),
            |v: &mut Value| {
                v["palette"].as_object_mut().unwrap().remove("color9");
            },
        ] {
            let mut value = base.clone();
            mutate(&mut value);
            assert!(project_palette(&value).is_err());
        }
    }
}
