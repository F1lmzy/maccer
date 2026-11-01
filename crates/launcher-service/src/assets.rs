use anyhow::{Result, anyhow};
use launcher_macos::ApplicationIconPixels;
use launcher_protocol::{AssetMetadata, AssetRef, MAX_ASSET_BYTES, MAX_ICON_EDGE};
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
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
        Ok(launcher_macos::application_icon_pixels(path))
    }
}

struct Entry {
    path: PathBuf,
    revision: String,
    value: Option<Option<(AssetMetadata, Vec<u8>)>>,
}

/// A bounded, lazy cache. Paths are only retained by the service and never enter the protocol.
pub struct AssetStore {
    resolver: Arc<dyn IconResolver>,
    resolve_lock: Mutex<()>,
    state: Mutex<State>,
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
        })
    }

    /// Registers a stable opaque reference without touching the filesystem or resolving the icon.
    pub fn reference(&self, path: &Path, revision: u64) -> AssetRef {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let revision = revision.to_string();
        if let Some(id) = state.entries.iter().find_map(|(id, entry)| {
            (entry.path == path && entry.revision == revision).then(|| id.clone())
        }) {
            // Reuse immutable assets across views/clients instead of re-resolving
            // the same Finder icon on every keystroke. Touch the bounded LRU.
            state.order.retain(|existing| existing != &id);
            state.order.push_back(id.clone());
            return AssetRef {
                id,
                revision,
                representations: vec!["bgra8".into()],
            };
        }
        let id = format!("icon-{}", state.next_id);
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
                value: None,
            },
        );
        AssetRef {
            id,
            revision,
            representations: vec!["bgra8".into()],
        }
    }

    /// Resolves an authorized opaque reference on demand. Missing icons are cached too.
    pub fn get(&self, id: &str, revision: &str) -> Result<(AssetMetadata, Vec<u8>)> {
        let _resolve = self.resolve_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (path, cached) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let entry = state
                .entries
                .get(id)
                .ok_or_else(|| anyhow!("ASSET_EXPIRED"))?;
            if entry.revision != revision {
                return Err(anyhow!("ASSET_EXPIRED"));
            }
            let cached = entry.value.clone();
            (entry.path.clone(), cached)
        };
        let resolved = match cached {
            Some(resolved) => resolved,
            None => self
                .resolver
                .resolve(&path)?
                .map(|pixels| validate_pixels(id, revision, pixels))
                .transpose()?,
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = state.entries.get_mut(id).filter(|e| e.revision == revision) else {
            return Err(anyhow!("ASSET_EXPIRED"));
        };
        entry.value = Some(resolved.clone());
        resolved.ok_or_else(|| anyhow!("ASSET_UNAVAILABLE"))
    }
}

fn validate_pixels(
    id: &str,
    revision: &str,
    pixels: ApplicationIconPixels,
) -> Result<(AssetMetadata, Vec<u8>)> {
    let stride = pixels
        .width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("invalid icon stride"))?;
    let expected = (stride as usize)
        .checked_mul(pixels.height as usize)
        .ok_or_else(|| anyhow!("invalid icon size"))?;
    if pixels.width == 0
        || pixels.height == 0
        || pixels.width > MAX_ICON_EDGE
        || pixels.height > MAX_ICON_EDGE
        || expected > MAX_ASSET_BYTES
        || pixels.bgra.len() != expected
    {
        return Err(anyhow!("invalid icon pixel buffer"));
    }
    Ok((
        AssetMetadata {
            asset_id: id.into(),
            revision: revision.into(),
            representation: "bgra8".into(),
            width: pixels.width,
            height: pixels.height,
            stride,
            alpha_mode: "straight".into(),
            row_order: "topToBottom".into(),
            byte_length: pixels.bgra.len(),
        },
        pixels.bgra,
    ))
}
