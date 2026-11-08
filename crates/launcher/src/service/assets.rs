use crate::macos::ApplicationIconPixels;
use crate::protocol::{
    AssetLimits, AssetMetadata, AssetRef, REPRESENTATION_ICON, REPRESENTATION_PREVIEW,
};
use anyhow::{Result, anyhow};
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

struct Entry {
    path: PathBuf,
    revision: String,
    kind: AssetKind,
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
        self.register(path, revision, AssetKind::Icon)
    }

    /// Registers a lazily decoded wallpaper thumbnail for `path`.
    pub fn reference_preview(&self, path: &Path, revision: u64) -> AssetRef {
        self.register(path, revision, AssetKind::Preview)
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
                value: None,
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
        let _resolve = self.resolve_lock.lock().unwrap_or_else(|e| e.into_inner());
        let (path, kind, cached) = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let entry = state
                .entries
                .get(id)
                .ok_or_else(|| anyhow!("ASSET_EXPIRED"))?;
            if entry.revision != revision {
                return Err(anyhow!("ASSET_EXPIRED"));
            }
            if entry.kind.representation() != representation {
                return Err(anyhow!("ASSET_UNSUPPORTED_REPRESENTATION"));
            }
            let cached = entry.value.clone();
            (entry.path.clone(), entry.kind, cached)
        };
        let resolved = match cached {
            Some(resolved) => resolved,
            None => match kind {
                AssetKind::Icon => self
                    .resolver
                    .resolve(&path)?
                    .map(|pixels| validate_icon_pixels(id, revision, pixels))
                    .transpose()?,
                AssetKind::Preview => {
                    // ImageIO decodes on this thread and the result is cached, so
                    // each file pays this once per revision.
                    let (bgra, width, height) =
                        crate::macos::preview_pixels(&path, limits.max_edge)?;
                    Some(validate_pixels(
                        id,
                        revision,
                        representation,
                        width,
                        height,
                        bgra,
                    )?)
                }
            },
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entry) = state.entries.get_mut(id).filter(|e| e.revision == revision) else {
            return Err(anyhow!("ASSET_EXPIRED"));
        };
        entry.value = Some(resolved.clone());
        resolved.ok_or_else(|| anyhow!("ASSET_UNAVAILABLE"))
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
