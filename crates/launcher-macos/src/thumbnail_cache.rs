//! Bounded in-memory PNG cache. No user file contents are persisted to disk.
use std::{
    collections::VecDeque,
    fs::Metadata,
    path::{Path, PathBuf},
    time::SystemTime,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Key {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}
impl Key {
    pub(crate) fn new(path: &Path, metadata: &Metadata) -> Self {
        Self {
            path: path.into(),
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        }
    }
}

const MAX_ENTRIES: usize = 16;
const MAX_BYTES: usize = 16 * 1024 * 1024;
#[derive(Default)]
pub(crate) struct ThumbnailCache {
    entries: VecDeque<(Key, Vec<u8>)>,
    bytes: usize,
}
impl ThumbnailCache {
    pub(crate) fn get(&mut self, key: &Key) -> Option<Vec<u8>> {
        let index = self.entries.iter().position(|(cached, _)| cached == key)?;
        let entry = self.entries.remove(index)?;
        let png = entry.1.clone();
        self.entries.push_back(entry);
        Some(png)
    }
    pub(crate) fn insert(&mut self, key: Key, png: Vec<u8>) {
        if png.len() > MAX_BYTES {
            return;
        }
        // Replace previous revisions of this path rather than hoarding stale bytes.
        self.entries.retain(|(cached, bytes)| {
            if cached.path == key.path {
                self.bytes -= bytes.len();
                false
            } else {
                true
            }
        });
        while self.entries.len() >= MAX_ENTRIES || self.bytes + png.len() > MAX_BYTES {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.len();
            }
        }
        self.bytes += png.len();
        self.entries.push_back((key, png));
    }
}

pub(crate) fn get_or_generate(
    path: &Path,
    metadata: &Metadata,
    token: &launcher_core::CancellationToken,
    generate: impl FnOnce() -> anyhow::Result<Vec<u8>>,
) -> anyhow::Result<Vec<u8>> {
    use std::sync::{LazyLock, Mutex};
    static CACHE: LazyLock<Mutex<ThumbnailCache>> =
        LazyLock::new(|| Mutex::new(ThumbnailCache::default()));
    anyhow::ensure!(!token.is_cancelled(), "preview cancelled");
    let key = Key::new(path, metadata);
    let cached = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&key);
    if let Some(png) = cached {
        anyhow::ensure!(!token.is_cancelled(), "preview cancelled");
        return Ok(png);
    }
    // Never hold the cache lock during decoding, encoding or disk I/O.
    let png = generate()?;
    anyhow::ensure!(!token.is_cancelled(), "preview cancelled");
    if std::fs::metadata(path).is_ok_and(|metadata| Key::new(path, &metadata) == key) {
        CACHE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, png.clone());
    }
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(path: &Path) -> Key {
        Key::new(path, &std::fs::metadata(path).unwrap())
    }
    #[test]
    fn revisiting_a_thumbnail_hits_and_changed_files_miss() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.png");
        std::fs::write(&path, b"first").unwrap();
        let before = key(&path);
        let mut cache = ThumbnailCache::default();
        cache.insert(before.clone(), vec![1, 2, 3]);
        assert_eq!(cache.get(&before), Some(vec![1, 2, 3]));
        std::fs::write(&path, b"different").unwrap();
        let after = key(&path);
        assert_ne!(before, after);
        assert!(cache.get(&after).is_none());
        cache.insert(after.clone(), vec![4]);
        assert!(cache.get(&before).is_none());
        assert_eq!(cache.get(&after), Some(vec![4]));
        assert_eq!(cache.bytes, 1);
    }
    #[test]
    fn cache_hits_skip_generation_but_cancellation_and_file_changes_do_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cached.png");
        std::fs::write(&path, b"first").unwrap();
        let token = launcher_core::CancellationToken::default();
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(
            get_or_generate(&path, &metadata, &token, || Ok(vec![1])).unwrap(),
            vec![1]
        );
        assert_eq!(
            get_or_generate(&path, &metadata, &token, || panic!("cache hit regenerated")).unwrap(),
            vec![1]
        );
        token.cancel();
        assert!(
            get_or_generate(&path, &metadata, &token, || panic!("cancelled preview ran")).is_err()
        );
        std::fs::write(&path, b"changed").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(
            get_or_generate(&path, &metadata, &Default::default(), || Ok(vec![2])).unwrap(),
            vec![2]
        );
    }

    #[test]
    fn memory_and_entry_count_are_bounded_and_hits_refresh_recency() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ThumbnailCache::default();
        let keys: Vec<_> = (0..=MAX_ENTRIES)
            .map(|i| {
                let path = dir.path().join(i.to_string());
                std::fs::write(&path, b"x").unwrap();
                key(&path)
            })
            .collect();
        for key in &keys[..MAX_ENTRIES] {
            cache.insert(key.clone(), vec![0]);
        }
        assert!(cache.get(&keys[0]).is_some());
        cache.insert(keys[MAX_ENTRIES].clone(), vec![1]);
        assert!(cache.get(&keys[1]).is_none());
        assert!(cache.get(&keys[0]).is_some());
        for key in &keys {
            cache.insert(key.clone(), vec![0; MAX_BYTES / 2]);
        }
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(cache.bytes, MAX_BYTES);
    }
}
