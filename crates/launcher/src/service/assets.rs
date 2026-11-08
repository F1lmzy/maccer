use crate::macos::ApplicationIconPixels;
use crate::protocol::{
    AssetLimits, AssetMetadata, AssetRef, REPRESENTATION_ICON, REPRESENTATION_PREVIEW,
};
use anyhow::{Result, anyhow};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
};

const MAX_CACHED_ASSETS: usize = 256;

/// Resolves a private application path to bounded display pixels.
pub trait IconResolver: Send + Sync {
    fn resolve(&self, path: &Path) -> Result<Option<ApplicationIconPixels>>;
}

#[derive(Default)]
pub struct NativeIconResolver;
impl IconResolver for NativeIconResolver {
    fn resolve(&self, path: &Path) -> Result<Option<ApplicationIconPixels>> {
        Ok(crate::macos::application_icon_pixels(path))
    }
}

/// Which size class one registered path belongs to. The representation is
/// fixed when the reference is created, so a caller can never ask a 64px icon
/// slot for 512px pixels or the reverse.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AssetKind {
    Icon,
    Preview,
}
impl AssetKind {
    fn representation(self) -> &'static str {
        match self {
            Self::Icon => REPRESENTATION_ICON,
            Self::Preview => REPRESENTATION_PREVIEW,
        }
    }
}

/// What one registered asset currently holds.
///
/// `Resolving` is a claim: one thread decodes the file while every other
/// request for the same reference waits on [`AssetStore::settled`] instead of
/// paying for the same decode again. Without it a strip could decode the same
/// wallpaper twice when a prefetch and a demand request overlap.
enum EntryValue {
    Empty,
    Resolving,
    Settled(Option<(AssetMetadata, Vec<u8>)>),
}

struct Entry {
    path: PathBuf,
    revision: String,
    kind: AssetKind,
    value: EntryValue,
}

/// Releases a decode claim even if the resolver unwinds, so a panicking decode
/// cannot leave every later request for that reference waiting forever.
struct ClaimGuard<'a> {
    store: &'a AssetStore,
    id: &'a str,
    revision: &'a str,
}
impl Drop for ClaimGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.store.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = state.entries.get_mut(self.id).filter(|entry| {
            entry.revision == self.revision && matches!(entry.value, EntryValue::Resolving)
        }) {
            entry.value = EntryValue::Empty;
        }
        self.store.settled.notify_all();
    }
}

/// A bounded, lazy cache. Paths are only retained by the service and never enter the protocol.
pub struct AssetStore {
    resolver: Arc<dyn IconResolver>,
    /// Serializes native icon extraction only. ImageIO thumbnail decodes are
    /// thread-safe and self-limiting, so previews resolve concurrently.
    resolve_lock: Mutex<()>,
    state: Mutex<State>,
    /// Signalled whenever an entry leaves `EntryValue::Resolving`.
    settled: Condvar,
}
struct State {
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
    next_id: u64,
}
impl AssetStore {
    pub fn new(resolver: Arc<dyn IconResolver>) -> Arc<Self> {
        Arc::new(Self {
            resolver,
            resolve_lock: Mutex::new(()),
            state: Mutex::new(State {
                entries: HashMap::new(),
                order: VecDeque::new(),
                next_id: 1,
            }),
            settled: Condvar::new(),
        })
    }

    /// Registers a stable opaque reference without touching the filesystem or resolving the icon.
    pub fn reference(&self, path: &Path, revision: u64) -> AssetRef {
        self.register(path, revision, AssetKind::Icon)
    }

    /// Registers a lazily decoded wallpaper thumbnail for `path`.
    pub fn reference_preview(&self, path: &Path, revision: u64) -> AssetRef {
        self.register(path, revision, AssetKind::Preview)
    }

    /// Decodes one wallpaper thumbnail ahead of any request.
    ///
    /// Best effort by design: a failure leaves the entry empty so the demand
    /// path retries it and reports the error to the client. Decoding here means
    /// a later `get` for the same reference is a memcpy instead of ~70 ms of
    /// ImageIO work on the connection thread.
    pub fn prefetch_preview(&self, path: &Path, revision: u64) {
        if self.preview_ready(path, revision) {
            return;
        }
        let reference = self.reference_preview(path, revision);
        let _ = self.get(&reference.id, &reference.revision, REPRESENTATION_PREVIEW);
    }

    /// Whether this exact file revision already holds decoded preview pixels.
    /// A decode in flight is not ready: its owner is still paying for it.
    pub fn preview_ready(&self, path: &Path, revision: u64) -> bool {
        let revision = revision.to_string();
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.entries.values().any(|entry| {
            entry.path == path
                && entry.revision == revision
                && entry.kind == AssetKind::Preview
                && matches!(entry.value, EntryValue::Settled(Some(_)))
        })
    }

    fn register(&self, path: &Path, revision: u64, kind: AssetKind) -> AssetRef {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let revision = revision.to_string();
        if let Some(id) = state.entries.iter().find_map(|(id, entry)| {
            (entry.path == path && entry.revision == revision && entry.kind == kind)
                .then(|| id.clone())
        }) {
            // Reuse immutable assets across views/clients instead of re-resolving
            // the same Finder icon on every keystroke. Touch the bounded LRU.
            state.order.retain(|existing| existing != &id);
            state.order.push_back(id.clone());
            return AssetRef {
                id,
                revision,
                representations: vec![kind.representation().into()],
            };
        }
        let id = match kind {
            AssetKind::Icon => format!("icon-{}", state.next_id),
            AssetKind::Preview => format!("preview-{}", state.next_id),
        };
        state.next_id = state
            .next_id
            .checked_add(1)
            .expect("asset id space exhausted");
        if state.entries.len() == MAX_CACHED_ASSETS
            && let Some(expired) = state.order.pop_front()
        {
            state.entries.remove(&expired);
        }
        state.order.push_back(id.clone());
        state.entries.insert(
            id.clone(),
            Entry {
                path: path.to_path_buf(),
                revision: revision.clone(),
                kind,
                value: EntryValue::Empty,
            },
        );
        AssetRef {
            id,
            revision,
            representations: vec![kind.representation().into()],
        }
    }

    /// Resolves an authorized opaque reference on demand. Missing icons are cached too.
    pub fn get(
        &self,
        id: &str,
        revision: &str,
        representation: &str,
    ) -> Result<(AssetMetadata, Vec<u8>)> {
        let limits = AssetLimits::for_representation(representation)
            .ok_or_else(|| anyhow!("ASSET_UNSUPPORTED_REPRESENTATION"))?;
        // Claim the decode, reuse a settled value, or wait for the thread that
        // already claimed it.
        let (path, kind) = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            loop {
                let claimed = {
                    let entry = state
                        .entries
                        .get_mut(id)
                        .ok_or_else(|| anyhow!("ASSET_EXPIRED"))?;
                    if entry.revision != revision {
                        return Err(anyhow!("ASSET_EXPIRED"));
                    }
                    if entry.kind.representation() != representation {
                        return Err(anyhow!("ASSET_UNSUPPORTED_REPRESENTATION"));
                    }
                    match &entry.value {
                        EntryValue::Settled(value) => {
                            return value.clone().ok_or_else(|| anyhow!("ASSET_UNAVAILABLE"));
                        }
                        EntryValue::Resolving => None,
                        EntryValue::Empty => {
                            entry.value = EntryValue::Resolving;
                            Some((entry.path.clone(), entry.kind))
                        }
                    }
                };
                match claimed {
                    Some(claim) => break claim,
                    // Re-check after every wake: the entry may have been
                    // settled by another thread, evicted, or re-registered.
                    None => state = self.settled.wait(state).unwrap_or_else(|e| e.into_inner()),
                }
            }
        };
        let claim = ClaimGuard {
            store: self,
            id,
            revision,
        };
        let resolved = match kind {
            AssetKind::Icon => {
                let _serial = self.resolve_lock.lock().unwrap_or_else(|e| e.into_inner());
                self.resolver.resolve(&path).and_then(|pixels| {
                    pixels
                        .map(|pixels| validate_icon_pixels(id, revision, pixels))
                        .transpose()
                })
            }
            AssetKind::Preview => {
                // ImageIO decodes on this thread and the result is cached, so
                // each file pays this once per revision.
                crate::macos::preview_pixels(&path, limits.max_edge)
                    .and_then(|(bgra, width, height)| {
                        validate_pixels(id, revision, representation, width, height, bgra)
                    })
                    .map(Some)
            }
        };
        let stored = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // The entry may have been evicted while this thread decoded it. That
            // is reported as expiry, never as a successful fetch for an asset the
            // store no longer knows about — the client has to refresh its view.
            let stored = state
                .entries
                .get_mut(id)
                .filter(|entry| entry.revision == revision)
                .map(|entry| {
                    // A failed decode is not cached: the next request retries it
                    // and reports its own error.
                    entry.value = match &resolved {
                        Ok(value) => EntryValue::Settled(value.clone()),
                        Err(_) => EntryValue::Empty,
                    };
                });
            self.settled.notify_all();
            stored
        };
        // The claim has been settled; release it after the state lock so its
        // unwind guard can never re-enter the mutex.
        drop(claim);
        if stored.is_none() {
            return Err(anyhow!("ASSET_EXPIRED"));
        }
        resolved?.ok_or_else(|| anyhow!("ASSET_UNAVAILABLE"))
    }
}

fn validate_icon_pixels(
    id: &str,
    revision: &str,
    pixels: ApplicationIconPixels,
) -> Result<(AssetMetadata, Vec<u8>)> {
    validate_pixels(
        id,
        revision,
        REPRESENTATION_ICON,
        pixels.width,
        pixels.height,
        pixels.bgra,
    )
}

/// Check one straight-alpha BGRA buffer against its representation budget.
fn validate_pixels(
    id: &str,
    revision: &str,
    representation: &str,
    width: u32,
    height: u32,
    bgra: Vec<u8>,
) -> Result<(AssetMetadata, Vec<u8>)> {
    let Some(limits) = AssetLimits::for_representation(representation) else {
        return Err(anyhow!("ASSET_UNSUPPORTED_REPRESENTATION"));
    };
    let stride = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("invalid asset stride"))?;
    let expected = (stride as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow!("invalid asset size"))?;
    if width == 0
        || height == 0
        || width > limits.max_edge
        || height > limits.max_edge
        || expected > limits.max_bytes
        || bgra.len() != expected
    {
        return Err(anyhow!("invalid asset pixel buffer"));
    }
    Ok((
        AssetMetadata {
            asset_id: id.into(),
            revision: revision.into(),
            representation: representation.into(),
            width,
            height,
            stride,
            alpha_mode: "straight".into(),
            row_order: "topToBottom".into(),
            byte_length: bgra.len(),
        },
        bgra,
    ))
}
