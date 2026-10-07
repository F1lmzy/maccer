//! Rebuildable, atomically published fd index. No search-time subprocesses.
use crate::{
    process::run_bounded,
    spotlight::{FileMetadata, FileSearch},
};
use anyhow::{Context, Result, bail};
use launcher_core::CancellationToken;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use regex::RegexSet;
use rusqlite::Connection;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
pub struct FdConfig {
    pub roots: Vec<PathBuf>,
    pub excluded: Vec<PathBuf>,
    pub ignored_dirs: Vec<String>,
    pub watch: bool,
}
impl FdConfig {
    pub fn validate(&self) -> Result<()> {
        if self.roots.is_empty() || self.roots.iter().any(|p| !p.is_absolute()) {
            bail!("fd search roots must be absolute directories");
        }
        RegexSet::new(&self.ignored_dirs)
            .context("invalid files.ignored_dirs regular expression")?;
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    normalized: String,
    modified: i64,
    directory: bool,
}
#[derive(Default)]
struct Snapshot {
    entries: Vec<Entry>,
    metadata: HashMap<PathBuf, (FileMetadata, bool, u64)>,
}

pub struct FdIndex {
    executable: PathBuf,
    config: FdConfig,
    ignored: RegexSet,
    cache_directory: PathBuf,
    db: Mutex<Connection>,
    refresh_lock: Mutex<()>,
    snapshot: RwLock<Snapshot>,
    error: Mutex<Option<String>>,
    watch_error: Mutex<Option<String>>,
    revision: AtomicU64,
}
impl FdIndex {
    pub fn executable() -> Result<PathBuf> {
        let mut directories: Vec<PathBuf> = std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).collect())
            .unwrap_or_default();
        directories.extend([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ]);
        if let Some(home) = std::env::var_os("HOME") {
            directories.push(PathBuf::from(home).join(".local/bin"));
        }
        for name in ["fd", "fdfind"] {
            for dir in &directories {
                let path = dir.join(name);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if path
                        .metadata()
                        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    {
                        return Ok(path);
                    }
                }
            }
        }
        bail!(
            "Files provider requires fd. Install it with `brew install fd`, or disable providers.files"
        )
    }
    /// Creates the cache; call `start` to index/watch without delaying startup.
    pub fn open(mut config: FdConfig, cache: &Path) -> Result<Arc<Self>> {
        config.validate()?;
        config.roots = config
            .roots
            .iter()
            .map(|p| {
                p.canonicalize()
                    .with_context(|| format!("opening fd search root {}", p.display()))
            })
            .collect::<Result<_>>()?;
        config.excluded = config
            .excluded
            .iter()
            .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
            .collect();
        if config.roots.iter().any(|p| !p.is_dir()) {
            bail!("fd search roots must be existing directories");
        }
        let executable = Self::executable()?;
        let cache_directory = cache
            .parent()
            .context("fd cache must have a parent directory")?
            .to_path_buf();
        std::fs::create_dir_all(&cache_directory)?;
        let cache_directory = cache_directory.canonicalize()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cache_directory, std::fs::Permissions::from_mode(0o700))?;
        }
        let db = Connection::open(cache)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o600))?;
        }
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
            CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, modified INTEGER NOT NULL, directory INTEGER NOT NULL);")?;
        // Do not expose a previous configuration's paths while rebuilding.
        db.execute("DELETE FROM files", [])?;
        Ok(Arc::new(Self {
            ignored: RegexSet::new(&config.ignored_dirs)?,
            executable,
            config,
            cache_directory,
            db: Mutex::new(db),
            refresh_lock: Mutex::new(()),
            snapshot: RwLock::new(Snapshot::default()),
            error: Mutex::new(None),
            watch_error: Mutex::new(None),
            revision: AtomicU64::new(0),
        }))
    }
    pub fn entry_count(&self) -> usize {
        self.snapshot.read().unwrap().entries.len()
    }
    pub fn start(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let config = self.config.clone();
        let cache_directory = self.cache_directory.clone();
        let ignored = RegexSet::new(&config.ignored_dirs).expect("validated ignored directories");
        let excluded = config.excluded.clone();
        std::thread::spawn(move || {
            let (tx, rx) = mpsc::sync_channel(1);
            // Keep the watcher alive for the worker lifetime. fd's ignore rules
            // are reapplied on refresh, including newly-created .gitignore files.
            let mut watcher: Option<RecommendedWatcher> = if config.watch {
                match notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if let Ok(event) = &event
                        && event.paths.iter().all(|p| {
                            p.starts_with(&cache_directory)
                                || excluded.iter().any(|root| p.starts_with(root))
                                || ignored.is_match(p.to_string_lossy().as_ref())
                        })
                    {
                        return;
                    }
                    // One pending request suffices because refresh rescans the
                    // complete roots. A build burst cannot grow the event queue.
                    let _ = tx.try_send(event);
                }) {
                    Ok(mut watcher) => {
                        for root in &config.roots {
                            if let Err(error) = watcher.watch(root, RecursiveMode::Recursive)
                                && let Some(index) = weak.upgrade()
                            {
                                index.report_watch(format!("watching {}: {error}", root.display()));
                            }
                        }
                        Some(watcher)
                    }
                    Err(error) => {
                        if let Some(index) = weak.upgrade() {
                            index.report_watch(format!("starting file watcher: {error}"));
                        }
                        None
                    }
                }
            } else {
                None
            };
            if let Some(index) = weak.upgrade()
                && let Err(error) = index.refresh(&CancellationToken::default())
            {
                index.report(format!("{error:#}"));
            }
            while weak.strong_count() > 0 && watcher.is_some() {
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(event) => {
                        if let Err(error) = event
                            && let Some(index) = weak.upgrade()
                        {
                            index.report_watch(format!("file watcher: {error}"));
                        }
                        // Bounded debounce: even continuous writes cannot postpone
                        // a refresh forever. The last good snapshot remains usable.
                        let deadline = std::time::Instant::now() + Duration::from_millis(750);
                        while std::time::Instant::now() < deadline {
                            if rx.recv_timeout(Duration::from_millis(150)).is_err() {
                                break;
                            }
                        }
                        if let Some(index) = weak.upgrade()
                            && let Err(error) = index.refresh(&CancellationToken::default())
                        {
                            index.report(format!("{error:#}"));
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            watcher.take();
        });
    }
    fn report_watch(&self, error: String) {
        *self.watch_error.lock().unwrap() = Some(error);
        self.revision.fetch_add(1, Ordering::Release);
    }
    fn report(&self, error: String) {
        *self.error.lock().unwrap() = Some(error);
        self.revision.fetch_add(1, Ordering::Release);
    }
    fn allowed(&self, path: &Path) -> bool {
        path.is_absolute()
            && self.config.roots.iter().any(|root| path.starts_with(root))
            && !path.starts_with(&self.cache_directory)
            && !self
                .config
                .excluded
                .iter()
                .any(|root| path.starts_with(root))
            && !self.ignored.is_match(path.to_string_lossy().as_ref())
    }
    pub fn refresh(&self, token: &CancellationToken) -> Result<()> {
        let _refresh = self.refresh_lock.lock().unwrap();
        const MAX_BYTES: usize = 64 * 1024 * 1024;
        let mut command = Command::new(&self.executable);
        command.args([
            "--absolute-path",
            "--print0",
            "--ignore-vcs",
            "--type",
            "file",
            "--type",
            "directory",
            "--",
            ".",
        ]);
        command.args(&self.config.roots);
        let output = run_bounded(&mut command, token, Duration::from_secs(60), MAX_BYTES)?;
        if !output.status.success() {
            bail!(
                "fd indexing failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if output.stdout.len() == MAX_BYTES {
            bail!("fd index exceeds 64 MiB; narrow files.search_dirs or use fd ignore rules");
        }
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for record in output.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
            if token.is_cancelled() {
                bail!("file indexing cancelled");
            }
            // AppKit cannot represent arbitrary Unix filename bytes. Skip only
            // that entry rather than losing the whole index.
            let Ok(text) = std::str::from_utf8(record) else {
                continue;
            };
            let path = PathBuf::from(text);
            if !self.allowed(&path) || !seen.insert(path.clone()) {
                continue;
            }
            if let Ok(meta) = std::fs::metadata(&path) {
                let modified = changed_nanos(&meta);
                entries.push(Entry {
                    path,
                    normalized: normalized(text),
                    modified,
                    directory: meta.is_dir(),
                });
            }
        }
        entries.sort_by(|a, b| {
            b.modified
                .cmp(&a.modified)
                .then_with(|| a.path.cmp(&b.path))
        });
        let unchanged = self.snapshot.read().unwrap().entries == entries;
        if unchanged
            && self.error.lock().unwrap().is_none()
            && self.revision.load(Ordering::Acquire) > 0
        {
            return Ok(());
        }
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute("DELETE FROM files", [])?;
        {
            let mut put =
                tx.prepare("INSERT INTO files(path, modified, directory) VALUES (?1, ?2, ?3)")?;
            for entry in &entries {
                if token.is_cancelled() {
                    bail!("file indexing cancelled");
                }
                put.execute(rusqlite::params![
                    entry.path.to_string_lossy(),
                    entry.modified,
                    entry.directory
                ])?;
            }
        }
        tx.commit()?;
        let metadata = entries
            .iter()
            .map(|e| {
                (
                    e.path.clone(),
                    (
                        FileMetadata {
                            modified: Some(e.modified / 1_000_000_000),
                            ..Default::default()
                        },
                        e.directory,
                        e.modified as u64,
                    ),
                )
            })
            .collect();
        let previous = {
            let mut snapshot = self.snapshot.write().unwrap();
            std::mem::replace(&mut *snapshot, Snapshot { entries, metadata })
        };
        // Drop the potentially large old index outside the read/write lock.
        drop(previous);
        *self.error.lock().unwrap() = None;
        self.revision.fetch_add(1, Ordering::Release);
        Ok(())
    }
}
impl FileSearch for FdIndex {
    fn is_indexed(&self) -> bool {
        true
    }
    fn refresh_index(&self, token: &CancellationToken) -> Result<()> {
        self.refresh(token)
    }
    fn revision(&self) -> u64 {
        self.revision.load(Ordering::Acquire)
    }
    fn status(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap()
            .clone()
            .or_else(|| self.watch_error.lock().unwrap().clone())
            .or_else(|| {
                (self.revision.load(Ordering::Acquire) == 0)
                    .then(|| "Indexing files with fd…".into())
            })
    }
    fn metadata(&self, path: &Path) -> FileMetadata {
        self.snapshot
            .read()
            .unwrap()
            .metadata
            .get(path)
            .map(|entry| entry.0)
            .unwrap_or_default()
    }
    fn preview_revision(&self, path: &Path) -> u64 {
        self.snapshot
            .read()
            .unwrap()
            .metadata
            .get(path)
            .map_or(0, |entry| entry.2)
    }
    fn is_directory(&self, path: &Path) -> bool {
        self.snapshot
            .read()
            .unwrap()
            .metadata
            .get(path)
            .is_some_and(|entry| entry.1)
    }
    fn search(
        &self,
        text: &str,
        root: Option<&Path>,
        token: &CancellationToken,
        limit: usize,
    ) -> Result<Vec<PathBuf>> {
        if let Some(error) = self.error.lock().unwrap().as_ref()
            && self.snapshot.read().unwrap().entries.is_empty()
        {
            bail!("{error}");
        }
        if text.len() > 1024 || text.chars().any(char::is_control) {
            bail!("file query must be at most 1024 bytes without control characters");
        }
        let root = root.map(|p| p.canonicalize().unwrap_or_else(|_| p.to_path_buf()));
        let words: Vec<_> = text.split_whitespace().map(normalized).collect();
        let snapshot = self.snapshot.read().unwrap();
        // Elephant browses the most recently changed files, not every file.
        let limit = limit.min(if words.is_empty() { 100 } else { 1000 });
        if limit == 0 {
            return Ok(vec![]);
        }
        let mut result = Vec::new();
        let mut fuzzy = Vec::new();
        for entry in &snapshot.entries {
            if token.is_cancelled() {
                break;
            }
            if root
                .as_ref()
                .is_some_and(|root| !entry.path.starts_with(root))
                || (words.is_empty() && entry.directory)
            {
                continue;
            }
            // Prefer literal path terms over subsequence-only candidates before
            // applying Elephant's 1000-candidate search budget. Cached normalized
            // paths avoid allocating/lowercasing the whole index per keystroke.
            if words.iter().all(|word| entry.normalized.contains(word)) {
                result.push(entry.path.clone());
                if result.len() >= limit {
                    break;
                }
            } else if fuzzy.len() < limit
                && words
                    .iter()
                    .all(|word| subsequence(word, &entry.normalized))
            {
                fuzzy.push(entry.path.clone());
            }
        }
        result.extend(fuzzy.into_iter().take(limit - result.len()));
        Ok(result)
    }
}
fn changed_nanos(meta: &std::fs::Metadata) -> i64 {
    let modified = meta
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Like Elephant's ChangeTime, include rename/chmod/metadata changes.
        modified.max(
            meta.ctime()
                .saturating_mul(1_000_000_000)
                .saturating_add(meta.ctime_nsec()),
        )
    }
    #[cfg(not(unix))]
    modified
}
fn normalized(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
    text.nfd()
        .filter(|ch| !is_combining_mark(*ch))
        .flat_map(char::to_lowercase)
        .collect()
}
fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|ch| chars.by_ref().any(|candidate| candidate == ch))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fd_indexes_files_folders_full_paths_and_refreshes_rename_delete() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let docs = root_path.join("My Documents");
        std::fs::create_dir(&docs).unwrap();
        let file = docs.join("例\nreport.txt");
        std::fs::write(&file, "hello").unwrap();
        let ignored = root_path.join("private");
        std::fs::create_dir(&ignored).unwrap();
        std::fs::write(ignored.join("secret.txt"), "secret").unwrap();
        let index = FdIndex::open(
            FdConfig {
                roots: vec![root_path],
                excluded: vec![],
                ignored_dirs: vec![format!(
                    "^{}(?:/|$)",
                    regex::escape(ignored.to_str().unwrap())
                )],
                watch: false,
            },
            &cache.path().join("files.db"),
        )
        .unwrap();
        let token = CancellationToken::default();
        index.refresh(&token).unwrap();
        assert_eq!(
            index.search("", None, &token, 100).unwrap(),
            vec![file.clone()]
        );
        let results = index.search("My Documents", None, &token, 100).unwrap();
        assert!(results.contains(&docs) && results.contains(&file));
        // A failed rescan must not make the previous snapshot unusable.
        index.report("temporary indexing failure".into());
        assert!(index.status().unwrap().contains("temporary"));
        assert!(
            index
                .search("report", None, &token, 100)
                .unwrap()
                .contains(&file)
        );
        let renamed = docs.join("new.txt");
        std::fs::rename(&file, &renamed).unwrap();
        index.refresh(&token).unwrap();
        assert_eq!(
            index.search("new", None, &token, 100).unwrap(),
            vec![renamed.clone()]
        );
        std::fs::remove_file(renamed).unwrap();
        index.refresh(&token).unwrap();
        assert!(index.search("", None, &token, 100).unwrap().is_empty());
    }
    #[test]
    fn watcher_refreshes_new_files_and_same_second_edits() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let index = FdIndex::open(
            FdConfig {
                roots: vec![root_path.clone()],
                excluded: vec![],
                ignored_dirs: vec![],
                watch: true,
            },
            &cache.path().join("files.db"),
        )
        .unwrap();
        index.start();
        let wait = |condition: &dyn Fn() -> bool| {
            let deadline = std::time::Instant::now() + Duration::from_secs(8);
            while !condition() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "watcher did not refresh"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        };
        wait(&|| index.revision() > 0);
        let file = root_path.join("new.txt");
        std::fs::write(&file, "first").unwrap();
        wait(&|| {
            index
                .search("new", None, &CancellationToken::default(), 10)
                .unwrap()
                .contains(&file)
        });
        let previous = index.revision();
        let preview_version = index.preview_revision(&file);
        std::fs::write(&file, "edited").unwrap();
        wait(&|| index.revision() > previous);
        assert_ne!(index.preview_revision(&file), preview_version);
        std::fs::remove_file(&file).unwrap();
        wait(&|| {
            index
                .search("new", None, &CancellationToken::default(), 10)
                .unwrap()
                .is_empty()
        });
    }
    #[test]
    fn broad_queries_are_bounded_without_hiding_older_literal_matches() {
        let root = tempfile::tempdir().unwrap();
        let cache = tempfile::tempdir().unwrap();
        let root_path = root.path().canonicalize().unwrap();
        let index = FdIndex::open(
            FdConfig {
                roots: vec![root_path.clone()],
                excluded: vec![],
                ignored_dirs: vec![],
                watch: false,
            },
            &cache.path().join("files.db"),
        )
        .unwrap();
        let mut entries: Vec<_> = (0..2000)
            .map(|n| {
                let path = root_path.join(format!("report-{n:04}.txt"));
                Entry {
                    normalized: normalized(path.to_str().unwrap()),
                    path,
                    modified: 0,
                    directory: false,
                }
            })
            .collect();
        let target = root_path.join("Résumé-rpt.pdf");
        entries.push(Entry {
            normalized: normalized(target.to_str().unwrap()),
            path: target.clone(),
            modified: 0,
            directory: false,
        });
        index.snapshot.write().unwrap().entries = entries;
        let found = index
            .search("rpt", None, &CancellationToken::default(), usize::MAX)
            .unwrap();
        assert_eq!(found.len(), 1000);
        assert_eq!(found[0], target);
        assert_eq!(
            index
                .search("resume", None, &CancellationToken::default(), 10)
                .unwrap(),
            vec![target]
        );
    }
    #[test]
    fn invalid_ignored_regex_is_rejected() {
        assert!(
            FdConfig {
                roots: vec!["/tmp".into()],
                excluded: vec![],
                ignored_dirs: vec!["[".into()],
                watch: false
            }
            .validate()
            .is_err()
        );
    }
}
