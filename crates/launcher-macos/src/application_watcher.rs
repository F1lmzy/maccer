//! Debounced, cancellable application catalog watcher.
//!
//! The application provider owns a prebuilt, immutable search index built from
//! a [`Application`] snapshot. This module keeps that snapshot fresh: it watches
//! the configured application roots with `notify`, rescans after filesystem
//! activity settles, and hands each complete snapshot to a callback. Discovery,
//! matching and icon loading stay outside; a keystroke never touches the
//! filesystem here.
//!
//! Watches are installed before the first scan so a change that lands during
//! startup is observed. A dedicated worker thread owns the `notify` watcher and
//! the callback; dropping the returned [`ApplicationWatcher`] stops and joins
//! that worker, so no callback runs after `drop` returns.

use crate::applications::{Application, discover_in};
use anyhow::{Result, bail};
use notify::{RecursiveMode, Watcher};
use std::{
    collections::{BTreeMap, btree_map::Entry},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// Debounce and liveness bounds for the refresh worker.
#[derive(Clone, Copy, Debug)]
struct Timing {
    /// Quiet period that must pass before a refresh runs.
    quiet: Duration,
    /// Upper bound on one debounce window, so a constant stream of events
    /// cannot postpone the refresh forever.
    max_burst: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(500),
            max_burst: Duration::from_secs(2),
        }
    }
}

/// A running application catalog watcher.
///
/// Dropping the watcher stops the refresh worker and joins it, so no callback
/// runs after `drop` returns.
#[derive(Debug)]
pub struct ApplicationWatcher {
    wake: mpsc::SyncSender<()>,
    stopped: Arc<AtomicBool>,
    status: Arc<Mutex<Option<String>>>,
    handle: Option<JoinHandle<()>>,
}

impl ApplicationWatcher {
    /// Starts watching `roots`, delivering a complete sorted snapshot whenever
    /// the catalog settles after a change.
    ///
    /// The first snapshot is delivered synchronously before this function
    /// returns. Roots must be absolute and may be directories or explicit
    /// bundles; a missing root is watched through its nearest existing ancestor
    /// so a later creation is detected.
    pub fn start<F>(roots: Vec<PathBuf>, mut on_refresh: F) -> Result<Self>
    where
        F: FnMut(Vec<Application>) + Send + 'static,
    {
        if roots.iter().any(|root| !root.is_absolute()) {
            bail!("application watch roots must be absolute paths");
        }
        if roots.iter().any(|root| root.to_str().is_none()) {
            bail!("application watch roots must be valid UTF-8 paths");
        }

        let status = Arc::new(Mutex::new(None));
        let stopped = Arc::new(AtomicBool::new(false));
        let (signal, receiver) = mpsc::sync_channel::<()>(1);
        let wake = signal.clone();

        let desired = desired_targets(&roots);
        let prefixes = Arc::new(Mutex::new(
            desired.keys().cloned().collect::<Vec<PathBuf>>(),
        ));
        let event_prefixes = prefixes.clone();
        let event_status = status.clone();
        let watch_retry = Arc::new(AtomicBool::new(false));
        let event_retry = watch_retry.clone();

        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) => {
                    // Access events (open/close/execute) never change the
                    // catalog and are the noisiest kind.
                    if event.kind.is_access()
                        || !event_is_relevant(
                            &event_prefixes.lock().unwrap_or_else(|e| e.into_inner()),
                            &event.paths,
                        )
                    {
                        return;
                    }
                    // Capacity one coalesces a burst: a full channel already
                    // means "something changed".
                    let _ = signal.try_send(());
                }
                Err(error) => {
                    let message = format!("application watcher: {error}");
                    tracing::warn!("{message}");
                    record_status(&event_status, Some(message));
                    event_retry.store(true, Ordering::Release);
                    let _ = signal.try_send(());
                }
            })?;

        // Install watches before the first scan so a change that lands during
        // startup is not missed.
        let mut current = WatchSet::default();
        watch_retry.store(
            !reconcile(&mut watcher, &mut current, desired, &status),
            Ordering::Release,
        );

        // Deliver the first snapshot synchronously so the caller has a usable
        // catalog before the window opens.
        on_refresh(discover_in(&roots)?);

        let worker_status = status.clone();
        let worker_stopped = stopped.clone();
        let handle = std::thread::Builder::new()
            .name("maccer-app-watcher".into())
            .spawn(move || {
                worker(
                    roots,
                    on_refresh,
                    watcher,
                    current,
                    worker_status,
                    worker_stopped,
                    receiver,
                    prefixes,
                    watch_retry,
                    Timing::default(),
                );
            })?;

        Ok(Self {
            wake,
            stopped,
            status,
            handle: Some(handle),
        })
    }

    /// Last watch or scan error. `None` means the last reconcile and scan were
    /// clean.
    pub fn status(&self) -> Option<String> {
        self.status.lock().ok().and_then(|slot| slot.clone())
    }
}

fn desired_targets(roots: &[PathBuf]) -> BTreeMap<PathBuf, RecursiveMode> {
    let mut targets: BTreeMap<PathBuf, RecursiveMode> = BTreeMap::new();
    for root in roots {
        let (candidate, mode) = if root.exists() {
            (root.clone(), RecursiveMode::Recursive)
        } else {
            match nearest_existing_ancestor(root) {
                // A missing root is watched through its nearest existing
                // ancestor. NonRecursive keeps a missing `~/Applications` from
                // recursively watching all of `$HOME`.
                Some(ancestor) => (ancestor, RecursiveMode::NonRecursive),
                None => continue,
            }
        };
        let Some(path) = watchable(candidate) else {
            continue;
        };
        // The filesystem root is too broad to watch even non-recursively.
        if path == Path::new("/") {
            continue;
        }
        match targets.entry(path) {
            Entry::Occupied(mut entry) => {
                if mode == RecursiveMode::Recursive {
                    *entry.get_mut() = RecursiveMode::Recursive;
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(mode);
            }
        }
    }
    targets
}

/// Canonical watchable path, or `None` for a path `notify` cannot format as
/// UTF-8 (`notify` panics on those). Such a root is still scanned, just not
/// watched.
fn watchable(path: PathBuf) -> Option<PathBuf> {
    path.to_str()?;
    path.canonicalize().ok()
}

fn nearest_existing_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors()
        .find(|ancestor| ancestor.exists())
        .map(Path::to_path_buf)
}

/// Match notify's canonical watch paths, including ancestor events. Ancestor
/// watches can also trigger a scan for unrelated direct-child changes.
fn event_is_relevant(prefixes: &[PathBuf], paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return true;
    }
    paths.iter().any(|path| {
        prefixes
            .iter()
            .any(|prefix| path.starts_with(prefix) || prefix.starts_with(path))
    })
}

fn watch_additions(
    current: &BTreeMap<PathBuf, RecursiveMode>,
    desired: &BTreeMap<PathBuf, RecursiveMode>,
) -> Vec<(PathBuf, RecursiveMode)> {
    desired
        .iter()
        .filter(|(path, mode)| current.get(*path).copied() != Some(**mode))
        .map(|(path, mode)| (path.clone(), *mode))
        .collect()
}

#[derive(Default)]
struct WatchSet {
    confirmed: BTreeMap<PathBuf, RecursiveMode>,
    // A failed commit may have partially applied. Retain every possibly
    // installed path and force reconciliation even if desired paths revert.
    uncertain: Option<BTreeMap<PathBuf, RecursiveMode>>,
}

/// Brings the installed watches in line with `desired`. Per-path failures are
/// recorded and logged instead of aborting the worker.
fn reconcile<W: Watcher>(
    watcher: &mut W,
    current: &mut WatchSet,
    desired: BTreeMap<PathBuf, RecursiveMode>,
    status: &Mutex<Option<String>>,
) -> bool {
    let stale: Vec<PathBuf> = match &current.uncertain {
        Some(paths) => paths.keys().cloned().collect(),
        None => current
            .confirmed
            .iter()
            .filter(|(path, mode)| desired.get(*path).copied() != Some(**mode))
            .map(|(path, _)| path.clone())
            .collect(),
    };
    let additions = if current.uncertain.is_some() {
        desired
            .iter()
            .map(|(path, mode)| (path.clone(), *mode))
            .collect()
    } else {
        watch_additions(&current.confirmed, &desired)
    };

    // Restarting a notify backend is disruptive: FSEvents stops and recreates
    // its stream, which can drop events that land in the gap. Leave a watch set
    // that already matches `desired` untouched.
    if stale.is_empty() && additions.is_empty() && current.uncertain.is_none() {
        record_status(status, None);
        return true;
    }

    let mut errors: Vec<String> = Vec::new();
    let mut next = current.confirmed.clone();
    let mut possible = current
        .uncertain
        .clone()
        .unwrap_or_else(|| current.confirmed.clone());
    possible.extend(desired.iter().map(|(path, mode)| (path.clone(), *mode)));
    {
        let mut paths = watcher.paths_mut();
        for path in &stale {
            match paths.remove(path) {
                Ok(()) => {
                    next.remove(path);
                }
                Err(error)
                    if current.uncertain.is_some()
                        && matches!(
                            error.kind,
                            notify::ErrorKind::WatchNotFound | notify::ErrorKind::PathNotFound
                        ) =>
                {
                    next.remove(path);
                }
                Err(error) => errors.push(format!("unwatching {}: {error}", path.display())),
            }
        }
        for (path, mode) in additions {
            match paths.add(&path, mode) {
                Ok(()) => {
                    next.insert(path, mode);
                }
                Err(error) => errors.push(format!("watching {}: {error}", path.display())),
            }
        }
        match paths.commit() {
            Ok(()) => {
                current.confirmed = next;
                current.uncertain = None;
            }
            Err(error) => {
                current.uncertain = Some(possible);
                errors.push(format!("committing watch changes: {error}"));
            }
        }
    }
    for error in &errors {
        tracing::warn!("application watcher: {error}");
    }
    let healthy = errors.is_empty();
    record_status(
        status,
        if healthy {
            None
        } else {
            Some(errors.join("; "))
        },
    );
    healthy
}

fn record_status(status: &Mutex<Option<String>>, message: Option<String>) {
    if let Ok(mut slot) = status.lock() {
        *slot = message;
    }
}

/// Waits until activity has been quiet for `timing.quiet`, or until
/// `timing.max_burst` elapses. Returns `false` when a stop was requested.
fn wait_for_quiet(signal: &mpsc::Receiver<()>, stopped: &AtomicBool, timing: Timing) -> bool {
    let start = Instant::now();
    loop {
        if stopped.load(Ordering::Acquire) {
            return false;
        }
        let remaining = timing.max_burst.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return true;
        }
        match signal.recv_timeout(timing.quiet.min(remaining)) {
            Ok(()) => continue,
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                return true;
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn worker<F, W: Watcher>(
    roots: Vec<PathBuf>,
    mut on_refresh: F,
    mut watcher: W,
    mut current: WatchSet,
    status: Arc<Mutex<Option<String>>>,
    stopped: Arc<AtomicBool>,
    signal: mpsc::Receiver<()>,
    prefixes: Arc<Mutex<Vec<PathBuf>>>,
    watch_retry: Arc<AtomicBool>,
    timing: Timing,
) where
    F: FnMut(Vec<Application>),
{
    loop {
        let event = match signal.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => true,
            Err(mpsc::RecvTimeoutError::Timeout) => false,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if !event && !watch_retry.load(Ordering::Acquire) {
            continue;
        }
        if !wait_for_quiet(&signal, &stopped, timing) {
            break;
        }
        if stopped.load(Ordering::Acquire) {
            break;
        }
        // Retarget missing/new roots before scanning and publishing, so a
        // consumer receiving the snapshot can immediately edit the new root.
        let desired = desired_targets(&roots);
        *prefixes.lock().unwrap_or_else(|e| e.into_inner()) = desired.keys().cloned().collect();
        let healthy = reconcile(&mut watcher, &mut current, desired, &status);
        watch_retry.store(!healthy, Ordering::Release);
        match discover_in(&roots) {
            Ok(snapshot) => {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                on_refresh(snapshot);
            }
            Err(error) => {
                let message = format!("application scan: {error:#}");
                tracing::warn!("{message}");
                record_status(&status, Some(message));
            }
        }
        if stopped.load(Ordering::Acquire) {
            break;
        }
    }
}

impl Drop for ApplicationWatcher {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // Wake a worker blocked on the signal channel. If the channel already
        // holds a coalesced event the worker is already runnable.
        let _ = self.wake.try_send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Value;
    use std::fs;

    /// Creates a `Name.app` bundle with the given metadata.
    fn bundle(dir: &Path, name: &str, bundle_id: &str, executable: &str) {
        let contents = dir.join(format!("{name}.app/Contents"));
        fs::create_dir_all(&contents).unwrap();
        write_plist(&contents.join("Info.plist"), name, bundle_id, executable);
    }

    fn write_plist(path: &Path, name: &str, bundle_id: &str, executable: &str) {
        plist::to_file_xml(
            path,
            &Value::Dictionary(plist::Dictionary::from_iter([
                ("CFBundleIdentifier", Value::String(bundle_id.into())),
                ("CFBundleName", Value::String(name.into())),
                ("CFBundleExecutable", Value::String(executable.into())),
            ])),
        )
        .unwrap();
    }

    /// Live temp directory plus its canonical path. The canonical path matters
    /// because FSEvents reports canonical paths and reads resolve symlinks.
    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap();
        (dir, path)
    }

    fn start(roots: Vec<PathBuf>) -> (ApplicationWatcher, Arc<Mutex<Vec<Vec<Application>>>>) {
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let sink = snapshots.clone();
        let watcher = ApplicationWatcher::start(roots, move |apps| {
            sink.lock().unwrap().push(apps);
        })
        .unwrap();
        (watcher, snapshots)
    }

    fn latest_has(snapshots: &Arc<Mutex<Vec<Vec<Application>>>>, name: &str) -> bool {
        snapshots
            .lock()
            .unwrap()
            .last()
            .is_some_and(|apps| apps.iter().any(|app| app.display_name == name))
    }

    fn wait_until(condition: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "wait condition timed out");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    #[derive(Default)]
    struct FakeState {
        installed: BTreeMap<PathBuf, RecursiveMode>,
        fail_add_once: bool,
        fail_commit_once: bool,
        partial_commit: bool,
        attempts: usize,
    }

    #[derive(Clone, Default)]
    struct FakeWatcher(Arc<Mutex<FakeState>>);

    struct FakePaths(Arc<Mutex<FakeState>>, BTreeMap<PathBuf, RecursiveMode>);

    impl notify::PathsMut for FakePaths {
        fn add(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            let mut state = self.0.lock().unwrap();
            state.attempts += 1;
            if state.fail_add_once {
                state.fail_add_once = false;
                return Err(notify::Error::generic("injected add failure"));
            }
            self.1.insert(path.to_path_buf(), mode);
            Ok(())
        }
        fn remove(&mut self, path: &Path) -> notify::Result<()> {
            self.1.remove(path);
            Ok(())
        }
        fn commit(self: Box<Self>) -> notify::Result<()> {
            let mut state = self.0.lock().unwrap();
            if state.fail_commit_once {
                state.fail_commit_once = false;
                if state.partial_commit {
                    state.installed = self.1;
                }
                return Err(notify::Error::generic("injected commit failure"));
            }
            state.installed = self.1;
            Ok(())
        }
    }

    impl Watcher for FakeWatcher {
        fn new<F: notify::EventHandler>(_: F, _: notify::Config) -> notify::Result<Self> {
            Ok(Self::default())
        }
        fn watch(&mut self, path: &Path, mode: RecursiveMode) -> notify::Result<()> {
            self.0
                .lock()
                .unwrap()
                .installed
                .insert(path.to_path_buf(), mode);
            Ok(())
        }
        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.0.lock().unwrap().installed.remove(path);
            Ok(())
        }
        fn paths_mut<'a>(&'a mut self) -> Box<dyn notify::PathsMut + 'a> {
            let staged = self.0.lock().unwrap().installed.clone();
            Box::new(FakePaths(self.0.clone(), staged))
        }
        fn kind() -> notify::WatcherKind {
            notify::WatcherKind::NullWatcher
        }
    }

    #[test]
    fn initial_add_failure_is_retried_and_status_clears() {
        let path = PathBuf::from("/Applications");
        let desired = BTreeMap::from([(path.clone(), RecursiveMode::Recursive)]);
        let mut watcher = FakeWatcher::default();
        watcher.0.lock().unwrap().fail_add_once = true;
        let mut current = WatchSet::default();
        let status = Mutex::new(None);
        assert!(!reconcile(
            &mut watcher,
            &mut current,
            desired.clone(),
            &status
        ));
        assert!(current.confirmed.is_empty());
        assert!(status.lock().unwrap().is_some());
        assert!(reconcile(
            &mut watcher,
            &mut current,
            desired.clone(),
            &status
        ));
        assert_eq!(current.confirmed, desired);
        assert_eq!(watcher.0.lock().unwrap().installed, desired);
        assert!(status.lock().unwrap().is_none());
    }

    #[test]
    fn failed_commit_keeps_state_unconfirmed_and_retries() {
        let path = PathBuf::from("/Applications");
        let desired = BTreeMap::from([(path.clone(), RecursiveMode::Recursive)]);
        let mut watcher = FakeWatcher::default();
        watcher.0.lock().unwrap().fail_commit_once = true;
        let mut current = WatchSet::default();
        let status = Mutex::new(None);
        assert!(!reconcile(
            &mut watcher,
            &mut current,
            desired.clone(),
            &status
        ));
        assert!(
            current.confirmed.is_empty(),
            "failed commit cannot update confirmed state"
        );
        assert!(watcher.0.lock().unwrap().installed.is_empty());
        assert!(reconcile(
            &mut watcher,
            &mut current,
            desired.clone(),
            &status
        ));
        assert_eq!(current.confirmed, desired);
        assert_eq!(watcher.0.lock().unwrap().installed, desired);
    }

    #[test]
    fn failed_partial_commit_is_repaired_even_if_desired_paths_revert() {
        let a = BTreeMap::from([(PathBuf::from("/A"), RecursiveMode::Recursive)]);
        let b = BTreeMap::from([(PathBuf::from("/B"), RecursiveMode::Recursive)]);
        let mut watcher = FakeWatcher::default();
        let mut current = WatchSet::default();
        let status = Mutex::new(None);
        assert!(reconcile(&mut watcher, &mut current, a.clone(), &status));
        {
            let mut state = watcher.0.lock().unwrap();
            state.fail_commit_once = true;
            state.partial_commit = true;
        }
        assert!(!reconcile(&mut watcher, &mut current, b, &status));
        assert!(reconcile(&mut watcher, &mut current, a.clone(), &status));
        assert_eq!(watcher.0.lock().unwrap().installed, a);
    }

    #[test]
    fn worker_retries_failed_watches_and_rescans_without_an_event() {
        let (_guard, root) = fixture();
        let mut watcher = FakeWatcher::default();
        watcher.0.lock().unwrap().fail_add_once = true;
        let mut current = WatchSet::default();
        let status = Arc::new(Mutex::new(None));
        assert!(!reconcile(
            &mut watcher,
            &mut current,
            desired_targets(std::slice::from_ref(&root)),
            &status
        ));
        let retry = Arc::new(AtomicBool::new(true));
        let stopped = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let (snapshots_tx, snapshots_rx) = mpsc::channel();
        bundle(&root, "Recovered", "dev.test.recovered", "recovered");
        let flag = stopped.clone();
        let handle = std::thread::spawn(move || {
            worker(
                vec![root],
                move |apps| {
                    snapshots_tx.send(apps).unwrap();
                },
                watcher,
                current,
                status,
                flag,
                rx,
                Arc::new(Mutex::new(Vec::new())),
                retry,
                Timing::default(),
            );
        });
        let snapshot = snapshots_rx.recv_timeout(Duration::from_secs(10));
        stopped.store(true, Ordering::Release);
        let _ = tx.try_send(());
        handle.join().unwrap();
        assert!(
            snapshot
                .unwrap()
                .iter()
                .any(|app| app.bundle_id.as_deref() == Some("dev.test.recovered"))
        );
    }

    #[test]
    fn rejects_relative_roots() {
        let error =
            ApplicationWatcher::start(vec![PathBuf::from("Applications")], |_| {}).unwrap_err();
        assert!(error.to_string().contains("absolute"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_watch_roots_before_notify_receives_them() {
        use std::os::unix::ffi::OsStrExt;
        let root = PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/maccer-\xff-apps"));
        let error = ApplicationWatcher::start(vec![root], |_| {}).unwrap_err();
        assert!(error.to_string().contains("valid UTF-8"), "{error}");
    }

    #[test]
    fn delivers_the_initial_snapshot_before_returning() {
        let (_guard, root) = fixture();
        bundle(&root, "Alpha", "dev.test.alpha", "alpha");
        let (_watcher, snapshots) = start(vec![root.clone()]);
        {
            let snapshots = snapshots.lock().unwrap();
            assert_eq!(snapshots.len(), 1);
            assert!(snapshots[0].iter().any(|app| app.display_name == "Alpha"));
        }
    }

    #[test]
    fn explicit_bundle_root_is_scanned() {
        let (_guard, root) = fixture();
        bundle(&root, "Alpha", "dev.test.alpha", "alpha");
        let (_watcher, snapshots) = start(vec![root.join("Alpha.app")]);
        assert!(latest_has(&snapshots, "Alpha"));
    }

    #[test]
    fn reports_added_removed_and_changed_bundles() {
        let (_guard, root) = fixture();
        bundle(&root, "Alpha", "dev.test.alpha", "alpha");
        let (_watcher, snapshots) = start(vec![root.clone()]);
        assert!(latest_has(&snapshots, "Alpha"));

        bundle(&root, "Beta", "dev.test.beta", "beta");
        wait_until(|| latest_has(&snapshots, "Beta"));

        let before_metadata = snapshots.lock().unwrap().len();
        write_plist(
            &root.join("Beta.app/Contents/Info.plist"),
            "Beta",
            "dev.test.beta",
            "beta2",
        );
        wait_until(|| {
            snapshots.lock().unwrap().len() > before_metadata
                && snapshots.lock().unwrap().last().is_some_and(|apps| {
                    apps.iter()
                        .any(|app| app.executable_name.as_deref() == Some("beta2"))
                })
        });

        let before_removal = snapshots.lock().unwrap().len();
        fs::remove_dir_all(root.join("Beta.app")).unwrap();
        wait_until(|| {
            let snapshots = snapshots.lock().unwrap();
            snapshots.len() > before_removal
                && snapshots
                    .last()
                    .is_some_and(|apps| !apps.iter().any(|app| app.display_name == "Beta"))
        });
    }

    #[test]
    fn detects_creation_of_a_missing_root() {
        let (_guard, base) = fixture();
        let root = base.join("Applications");
        let (_watcher, snapshots) = start(vec![root.clone()]);
        assert!(snapshots.lock().unwrap().last().unwrap().is_empty());

        bundle(&root, "Gamma", "dev.test.gamma", "gamma");
        wait_until(|| latest_has(&snapshots, "Gamma"));
    }

    #[test]
    fn dropping_the_watcher_stops_refreshes() {
        let (_guard, root) = fixture();
        bundle(&root, "Alpha", "dev.test.alpha", "alpha");
        let (watcher, snapshots) = start(vec![root.clone()]);
        let before = snapshots.lock().unwrap().len();

        drop(watcher);
        bundle(&root, "Beta", "dev.test.beta", "beta");
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(snapshots.lock().unwrap().len(), before);
    }

    #[test]
    fn status_is_clear_when_watches_install() {
        let (_guard, root) = fixture();
        bundle(&root, "Alpha", "dev.test.alpha", "alpha");
        let watcher = ApplicationWatcher::start(vec![root], |_| {}).unwrap();
        assert!(watcher.status().is_none());
    }

    #[test]
    fn existing_roots_watch_recursively_and_missing_roots_watch_the_ancestor() {
        let (_guard, base) = fixture();
        let existing = base.join("Existing");
        fs::create_dir_all(&existing).unwrap();
        let missing = base.join("Missing");

        let targets = desired_targets(&[existing.clone(), missing.clone()]);
        assert_eq!(
            targets.get(&existing.canonicalize().unwrap()),
            Some(&RecursiveMode::Recursive)
        );
        assert_eq!(targets.get(&base), Some(&RecursiveMode::NonRecursive));
        assert!(!targets.contains_key(&missing));
    }

    #[test]
    fn a_watch_mode_change_reinstalls_the_same_path() {
        let path = PathBuf::from("/Applications");
        let current = BTreeMap::from([(path.clone(), RecursiveMode::NonRecursive)]);
        let desired = BTreeMap::from([(path.clone(), RecursiveMode::Recursive)]);
        assert_eq!(
            watch_additions(&current, &desired),
            vec![(path, RecursiveMode::Recursive)]
        );
    }

    #[test]
    fn a_recursive_request_wins_for_a_shared_ancestor() {
        let (_guard, base) = fixture();
        let nested = base.join("Nested");
        fs::create_dir_all(&nested).unwrap();

        // `nested` exists (Recursive); its missing child resolves to `nested`
        // (NonRecursive). The stronger mode must win.
        let targets = desired_targets(&[nested.clone(), nested.join("Missing")]);
        assert_eq!(
            targets.get(&nested.canonicalize().unwrap()),
            Some(&RecursiveMode::Recursive)
        );
    }

    #[test]
    fn only_events_near_a_watched_root_are_relevant() {
        let prefixes = vec![PathBuf::from("/Applications"), PathBuf::from("/Users/me")];
        assert!(event_is_relevant(
            &prefixes,
            &[PathBuf::from("/Applications/Foo.app")]
        ));
        assert!(event_is_relevant(&prefixes, &[PathBuf::from("/Users/me")]));
        assert!(event_is_relevant(&prefixes, &[]));
        assert!(!event_is_relevant(
            &prefixes,
            &[PathBuf::from("/tmp/unrelated")]
        ));
    }

    #[test]
    fn a_prefilled_burst_is_drained_by_one_quiet_window() {
        // An unbounded channel lets the test pre-queue a burst a capacity-one
        // channel could not hold; the drain logic is identical.
        let (sender, receiver) = mpsc::channel::<()>();
        for _ in 0..50 {
            sender.send(()).unwrap();
        }
        drop(sender);
        let stopped = AtomicBool::new(false);
        let timing = Timing {
            quiet: Duration::from_millis(40),
            max_burst: Duration::from_secs(5),
        };

        let start = Instant::now();
        assert!(wait_for_quiet(&receiver, &stopped, timing));
        assert!(receiver.try_recv().is_err(), "burst was not fully drained");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn max_burst_bounds_continuous_activity() {
        let (sender, receiver) = mpsc::channel::<()>();
        let producer = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                if sender.send(()).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        let stopped = AtomicBool::new(false);
        let timing = Timing {
            quiet: Duration::from_millis(80),
            max_burst: Duration::from_millis(200),
        };

        let start = Instant::now();
        assert!(wait_for_quiet(&receiver, &stopped, timing));
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200),
            "returned before max burst: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "exceeded max burst bound: {elapsed:?}"
        );
        drop(receiver);
        producer.join().unwrap();
    }

    #[test]
    fn a_stop_signal_interrupts_the_quiet_window() {
        let (sender, receiver) = mpsc::channel::<()>();
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let wake = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            flag.store(true, Ordering::Release);
            // Drop mirrors this wake so a blocked worker notices the stop.
            let _ = sender.send(());
        });
        let timing = Timing {
            quiet: Duration::from_secs(30),
            max_burst: Duration::from_secs(30),
        };

        let start = Instant::now();
        assert!(!wait_for_quiet(&receiver, &stopped, timing));
        assert!(start.elapsed() < Duration::from_secs(2));
        wake.join().unwrap();
    }
}
