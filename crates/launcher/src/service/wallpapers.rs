//! Wallpaper browsing across the installed mac-themer packs.
//!
//! The catalog publishes a pack's wallpapers as path-free identities. A picker
//! needs pixels, and the producer cannot decode images, so this module resolves
//! `<themes>/<slug>/backgrounds/<id>` exactly the way mac-themer's own
//! `paths.py` does, validates that the resolved file stays inside that pack's
//! directory, and hands the private path to the asset store. Paths never enter
//! the protocol: a frontend only ever sees an opaque `previewBgra8` reference.
use crate::core::{
    Action, ActionId, ActionOutcome, Item, ItemId, Provider, ProviderId, SearchContext, SearchQuery,
};
use crate::macos::process::run_bounded;
use crate::protocol::{DomainData, Icon, ThemeWallpaper, is_theme_slug, is_wallpaper_id};
use crate::service::{
    AssetStore, ItemPresentation, ServiceProvider, ThemeCatalog, ThemerState, menu,
};
use anyhow::{Result, bail};
use serde::Deserialize;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

/// Hard deadline for one wallpaper mutation.
const SET_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const HELPER_PATH_PREFIX: &str = "/opt/homebrew/bin:/usr/local/bin:";
const BUSY_MESSAGE: &str =
    "Another wallpaper change is already in progress. Wait for it to finish, then try again.";
const PARTIAL_MESSAGE: &str = "The wallpaper helper failed or did not finish. The desktop picture \
may be unchanged or partly applied; check it before retrying.";
const INVALID_MESSAGE: &str = "The wallpaper information is invalid.";
const KNOWN_WARNINGS: [&str; 1] = ["wallpaperFailed"];

/// Where mac-themer keeps its packs.
///
/// Mirrors `lib/macthemer/paths.py`: `MACTHEMER_THEMES` wins, then
/// `MACTHEMER_REPO` (with `/themes`), then the checkout the helper was
/// installed from. The last case follows the `bin/mac-themer` symlink, which is
/// how `install.sh` wires it up. `None` means thumbnails are unavailable; the
/// picker still lists wallpapers, it just cannot preview them.
#[derive(Clone, Debug, Default)]
pub struct WallpaperLocation {
    themes: Option<PathBuf>,
}
impl WallpaperLocation {
    pub fn resolve(helper: &Path) -> Self {
        let expanded = |value: Option<OsString>| {
            let value = value.filter(|value| !value.is_empty())?;
            let path = PathBuf::from(value);
            let path = match path.strip_prefix("~") {
                Ok(rest) => dirs::home_dir()?.join(rest),
                Err(_) => path,
            };
            Some(path)
        };
        if let Some(themes) = expanded(std::env::var_os("MACTHEMER_THEMES")) {
            return Self {
                themes: Some(themes),
            };
        }
        if let Some(repo) = expanded(std::env::var_os("MACTHEMER_REPO")) {
            return Self {
                themes: Some(repo.join("themes")),
            };
        }
        // The installed helper is a symlink into the checkout, and mac-themer
        // itself derives its root from the module file the same way.
        let checkout = std::fs::canonicalize(helper)
            .ok()
            .and_then(|path| path.parent()?.parent().map(Path::to_path_buf));
        Self {
            themes: checkout.map(|repo| repo.join("themes")),
        }
    }

    #[cfg(test)]
    fn at(themes: PathBuf) -> Self {
        Self {
            themes: Some(themes),
        }
    }

    /// Resolve one published identity to a private path.
    ///
    /// Every component is revalidated here because the identity crossed a
    /// process boundary: a slug or filename that is not exactly what the
    /// producer publishes can never name a file outside its own pack.
    pub fn path_for(&self, slug: &str, id: &str) -> Option<PathBuf> {
        let themes = self.themes.as_ref()?;
        if !is_theme_slug(slug) || !is_wallpaper_id(id) {
            return None;
        }
        let folder = themes.join(slug).join("backgrounds");
        let candidate = folder.join(id);
        let resolved = std::fs::canonicalize(&candidate).ok()?;
        let folder = std::fs::canonicalize(&folder).ok()?;
        if !resolved.starts_with(&folder) || !resolved.is_file() {
            return None;
        }
        Some(resolved)
    }
}

/// Shared executor for confirmed wallpaper changes. Cheap to clone behind an
/// `Arc`; only one change may run at a time across every client.
pub struct WallpaperApply {
    helper: PathBuf,
    busy: AtomicBool,
}
impl WallpaperApply {
    pub fn new(helper: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            helper,
            busy: AtomicBool::new(false),
        })
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }

    /// Set one of a pack's published wallpapers without touching the theme.
    pub fn set(&self, theme: &str, wallpaper: &ThemeWallpaper) -> Result<String> {
        if !is_theme_slug(theme) || !is_wallpaper_id(&wallpaper.id) {
            bail!(INVALID_MESSAGE);
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            bail!(BUSY_MESSAGE);
        }
        let _busy = BusyGuard(&self.busy);
        let mut command = Command::new(&self.helper);
        command
            .arg("wallpaper")
            .arg(theme)
            .arg("--id")
            .arg(&wallpaper.id)
            .arg("--json");
        command.env("PATH", helper_path());
        let output = run_bounded(
            &mut command,
            &crate::core::CancellationToken::default(),
            SET_TIMEOUT,
            MAX_OUTPUT_BYTES + 1,
        )
        .map_err(|_| anyhow::anyhow!(PARTIAL_MESSAGE))?;
        if !output.status.success() || output.stdout.len() > MAX_OUTPUT_BYTES {
            bail!(PARTIAL_MESSAGE);
        }
        let applied: SetReply =
            serde_json::from_slice(&output.stdout).map_err(|_| anyhow::anyhow!(PARTIAL_MESSAGE))?;
        if applied.schema != 1
            || !matches!(applied.status.as_str(), "set" | "failed")
            || applied.theme != theme
            || applied.wallpaper != wallpaper.id
            || applied
                .warnings
                .iter()
                .any(|warning| !KNOWN_WARNINGS.contains(&warning.as_str()))
        {
            bail!(PARTIAL_MESSAGE);
        }
        // The producer reports a failed picture separately from a rejected
        // identity: the identity was valid, the desktop simply did not change.
        if applied.status == "failed" || !applied.warnings.is_empty() {
            bail!(
                "The wallpaper could not be changed. The desktop may still show the previous one."
            );
        }
        Ok(format!("Wallpaper set to {}.", wallpaper.label))
    }
}
impl std::fmt::Debug for WallpaperApply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallpaperApply")
            .field("helper", &self.helper)
            .field("busy", &self.is_busy())
            .finish()
    }
}

struct BusyGuard<'a>(&'a AtomicBool);
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// The fixed producer reply. Unknown fields are ignored rather than forwarded.
#[derive(Deserialize)]
struct SetReply {
    schema: u16,
    status: String,
    theme: String,
    wallpaper: String,
    #[serde(default)]
    warnings: Vec<String>,
}

fn helper_path() -> OsString {
    let mut path = OsString::from(HELPER_PATH_PREFIX);
    if let Some(existing) = std::env::var_os("PATH") {
        path.push(existing);
    }
    path
}

/// One wallpaper a frontend can pick, resolved against the catalog snapshot
/// that produced the retained item.
struct Entry {
    slug: String,
    theme: String,
    path: PathBuf,
    wallpaper: ThemeWallpaper,
    revision: u64,
}

pub struct WallpaperServiceProvider {
    catalog: Arc<ThemeCatalog>,
    location: Arc<WallpaperLocation>,
    assets: Arc<AssetStore>,
    apply: Option<Arc<WallpaperApply>>,
    /// The active theme. A picker is scoped to the pack the desktop is wearing,
    /// so switching themes changes the strip rather than mixing every pack.
    theme: Option<Arc<ThemerState>>,
}
impl WallpaperServiceProvider {
    /// Browsing and previews only. Never starts a mutation helper.
    pub fn new(
        catalog: Arc<ThemeCatalog>,
        location: WallpaperLocation,
        assets: Arc<AssetStore>,
    ) -> Self {
        Self {
            catalog,
            location: Arc::new(location),
            assets,
            apply: None,
            theme: None,
        }
    }

    /// Scope the picker to the active theme; without this every pack is listed.
    pub fn with_theme(mut self, theme: Arc<ThemerState>) -> Self {
        self.theme = Some(theme);
        self
    }

    /// Explicit mutation support. Share this executor across all service clients.
    pub fn with_apply(
        catalog: Arc<ThemeCatalog>,
        location: WallpaperLocation,
        assets: Arc<AssetStore>,
        apply: Arc<WallpaperApply>,
    ) -> Self {
        Self {
            catalog,
            location: Arc::new(location),
            assets,
            apply: Some(apply),
            theme: None,
        }
    }

    /// The slug of the theme the desktop is currently wearing, if known.
    fn active_slug(&self) -> Option<String> {
        let state = self.theme.as_ref()?.snapshot();
        Some(state.palette?.slug)
    }

    fn entries(&self) -> (u64, Vec<Entry>) {
        let snapshot = self.catalog.snapshot();
        let active = self.active_slug();
        let mut entries = Vec::new();
        for palette in snapshot.palettes.iter() {
            // One active pack means one theme on the desktop; the picker only
            // offers pictures the desktop could be wearing right now.
            if active.as_ref().is_some_and(|slug| *slug != palette.slug) {
                continue;
            }
            for id in snapshot.wallpapers_for(&palette.slug) {
                let Some(wallpaper) = ThemeWallpaper::new(id) else {
                    continue;
                };
                let Some(path) = self.location.path_for(&palette.slug, id) else {
                    continue;
                };
                entries.push(Entry {
                    revision: asset_revision(snapshot.revision, &path),
                    slug: palette.slug.clone(),
                    theme: palette.name.clone(),
                    path,
                    wallpaper,
                });
            }
        }
        (snapshot.revision, entries)
    }

    /// Resolve one retained item back to the exact wallpaper it advertised.
    fn entry_for(&self, item: &Item) -> Option<Entry> {
        if item.provider != self.id() {
            return None;
        }
        let slug = item.payload.get("theme")?.as_str()?;
        let id = item.payload.get("wallpaper")?.as_str()?;
        let snapshot = self.catalog.snapshot();
        if item
            .payload
            .get("catalogRevision")
            .and_then(serde_json::Value::as_u64)
            != Some(snapshot.revision)
        {
            return None;
        }
        let wallpaper = ThemeWallpaper::new(id)?;
        let path = self.location.path_for(slug, id)?;
        Some(Entry {
            revision: asset_revision(snapshot.revision, &path),
            slug: slug.to_owned(),
            theme: snapshot
                .palettes
                .iter()
                .find(|palette| palette.slug == slug)
                .map(|palette| palette.name.clone())
                .unwrap_or_else(|| slug.to_owned()),
            path,
            wallpaper,
        })
    }
}

impl Provider for WallpaperServiceProvider {
    fn id(&self) -> ProviderId {
        ProviderId("wallpapers".into())
    }
    fn name(&self) -> &str {
        "Wallpapers"
    }
    /// The catalog revision plus the active theme, so switching themes
    /// re-searches the picker instead of serving the previous pack's strip.
    fn revision(&self) -> u64 {
        let revision = self.catalog.snapshot().revision;
        match self.active_slug() {
            Some(slug) => revision ^ fnv1a(slug.as_bytes()),
            None => revision,
        }
    }
    fn status(&self) -> Option<String> {
        if self.location.themes.is_none() {
            return Some(
                "Wallpaper previews are unavailable; mac-themer's pack directory was not found."
                    .into(),
            );
        }
        let snapshot = self.catalog.snapshot();
        if let Some(error) = snapshot.error {
            return Some(error);
        }
        let active = self.active_slug()?;
        if snapshot.wallpapers_for(&active).is_empty() {
            return Some(format!(
                "The active theme \"{active}\" ships no wallpapers."
            ));
        }
        None
    }
    fn records_usage(&self) -> bool {
        false
    }
    /// A wallpaper picker is a browse surface, never part of mixed search.
    fn requires_explicit_scope(&self) -> bool {
        true
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() || ctx.limit == 0 {
            return Ok(vec![]);
        }
        let (revision, entries) = self.entries();
        let mut matched = Vec::new();
        for (position, entry) in entries.iter().enumerate() {
            let Some(score) = menu::score(
                &query.text,
                &format!(
                    "{} {} {} {}",
                    entry.theme, entry.wallpaper.label, entry.wallpaper.id, entry.slug
                ),
            ) else {
                continue;
            };
            matched.push((position, entry, score));
        }
        // Catalog order is the strip order: theme by theme, in the order the
        // producer publishes. Ranking only decides which items survive a typed
        // query, never how they sit next to each other.
        let total = matched.len();
        Ok(matched
            .into_iter()
            .take(ctx.limit)
            .map(|(position, entry, score)| Item {
                id: ItemId(format!("wallpapers:{}:{position}", entry.slug)),
                provider: self.id(),
                title: entry.wallpaper.label.clone(),
                subtitle: Some(format!("{} · {}", entry.theme, entry.wallpaper.id)),
                keywords: vec![
                    entry.slug.clone(),
                    entry.theme.clone(),
                    entry.wallpaper.id.clone(),
                ],
                // Preview pixels are registered in `presentation`, which is the
                // only place the service projects an item for the wire.
                icon: None,
                // The coordinator ranks descending by score and breaks ties by
                // title, so a small positional term is the only way a browse
                // surface can keep its published order.
                score: -(score as f64) + (total - position) as f64 * 0.001,
                payload: serde_json::json!({
                    "catalogRevision": revision,
                    "theme": entry.slug,
                    "wallpaper": entry.wallpaper.id,
                }),
            })
            .collect())
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        if self.apply.is_some() && self.entry_for(item).is_some() {
            vec![Action {
                id: ActionId("set".into()),
                title: "Set wallpaper".into(),
            }]
        } else {
            vec![]
        }
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        if action.id.0 != "set" {
            bail!("this wallpaper action is not available")
        }
        let executor = self
            .apply
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("wallpaper changes are not enabled in this service"))?;
        let entry = self
            .entry_for(item)
            .ok_or_else(|| anyhow::anyhow!("that wallpaper is no longer in the catalog"))?;
        executor
            .set(&entry.slug, &entry.wallpaper)
            .map(ActionOutcome::KeepOpen)
    }
}

impl ServiceProvider for WallpaperServiceProvider {
    fn description(&self) -> &str {
        "Browse the active theme's wallpapers and set one without changing the theme."
    }
    fn kind(&self) -> &str {
        "wallpaper"
    }
    fn presentation(&self, item: &Item) -> ItemPresentation {
        let entry = self.entry_for(item);
        let fallback = item.title.chars().next().unwrap_or('◧').to_string();
        // Lazy: registers an opaque reference, so no image I/O happens while a
        // snapshot is being built.
        let icon = match &entry {
            Some(entry) => Icon::Asset {
                asset: self.assets.reference_preview(&entry.path, entry.revision),
                fallback_text: fallback,
            },
            None => Icon::Glyph {
                name: "wallpaper".into(),
                fallback_text: fallback,
            },
        };
        ItemPresentation {
            kind: "wallpaper".into(),
            icon: Some(icon),
            content: Vec::new(),
            data: entry.map(|entry| DomainData::WallpaperSummary {
                schema_version: 1,
                theme_slug: entry.slug,
                wallpaper_id: entry.wallpaper.id,
            }),
        }
    }
}

/// Asset revisions roll when the catalog does or when the file itself changes,
/// so a re-rendered pack is re-decoded instead of serving stale pixels.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn asset_revision(catalog_revision: u64, path: &Path) -> u64 {
    let modified = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |elapsed| elapsed.as_nanos() as u64);
    let mut bytes = Vec::with_capacity(16);
    bytes.extend_from_slice(&catalog_revision.to_le_bytes());
    bytes.extend_from_slice(&modified.to_le_bytes());
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::ProviderConfig;
    use crate::protocol::{
        THEME_BAR_KEYS, THEME_BORDER_KEYS, THEME_PALETTE_KEYS, THEME_TOKEN_KEYS,
    };
    use std::collections::BTreeMap;

    fn palette(slug: &str) -> crate::protocol::ThemePalette {
        let map = |keys: &[&str]| {
            keys.iter()
                .map(|key| ((*key).to_owned(), "#123456".to_owned()))
                .collect::<BTreeMap<_, _>>()
        };
        crate::protocol::ThemePalette {
            schema: 1,
            slug: slug.into(),
            name: format!("{slug} pack"),
            variant: "dark".into(),
            alpha_byte: 230,
            palette: map(&THEME_PALETTE_KEYS),
            tokens: map(&THEME_TOKEN_KEYS),
            border: map(&THEME_BORDER_KEYS),
            bar: map(&THEME_BAR_KEYS),
        }
    }

    /// One throwaway pack tree with the given wallpapers on disk.
    fn pack_tree(packs: &[(&str, &[&str])]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let themes = dir.path().join("themes");
        for (slug, ids) in packs {
            let backgrounds = themes.join(slug).join("backgrounds");
            std::fs::create_dir_all(&backgrounds).unwrap();
            for id in *ids {
                std::fs::write(backgrounds.join(id), b"not decoded in these tests").unwrap();
            }
        }
        (dir, themes)
    }

    fn provider(
        themes: PathBuf,
        packs: &[(&str, &[&str])],
    ) -> (WallpaperServiceProvider, Arc<crate::service::ThemeCatalog>) {
        let palettes = packs
            .iter()
            .map(|(slug, _)| palette(slug))
            .collect::<Vec<_>>();
        let wallpapers = packs
            .iter()
            .map(|(slug, ids)| {
                (
                    (*slug).to_owned(),
                    ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>(),
                )
            })
            .collect();
        let catalog = crate::service::ThemeCatalog::from_catalog(palettes, wallpapers).unwrap();
        let assets = crate::service::AssetStore::new(Arc::new(NoIcons));
        (
            WallpaperServiceProvider::new(catalog.clone(), WallpaperLocation::at(themes), assets),
            catalog,
        )
    }

    struct NoIcons;
    impl crate::service::IconResolver for NoIcons {
        fn resolve(
            &self,
            _path: &Path,
        ) -> anyhow::Result<Option<crate::macos::ApplicationIconPixels>> {
            Ok(None)
        }
    }

    fn search(provider: &WallpaperServiceProvider, query: &str) -> Vec<Item> {
        provider
            .search(
                &SearchQuery {
                    text: query.into(),
                    ..Default::default()
                },
                &SearchContext {
                    limit: 50,
                    cancellation: crate::core::CancellationToken::default(),
                    generation: 0,
                },
            )
            .unwrap()
    }

    #[test]
    fn the_picker_follows_the_active_theme() {
        let (_dir, themes) = pack_tree(&[
            ("pack-a", &["1-a.webp", "2-b.webp"]),
            ("pack-b", &["1-c.jpg"]),
        ]);
        let packs: &[(&str, &[&str])] = &[
            ("pack-a", &["1-a.webp", "2-b.webp"]),
            ("pack-b", &["1-c.jpg"]),
        ];
        let (provider, _catalog) = provider(themes, packs);
        // Without an active theme nothing is hidden: the picker still browses.
        assert_eq!(search(&provider, "").len(), 3);

        let provider = provider.with_theme(ThemerState::fixture_named("pack-a"));
        let items = search(&provider, "");
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.payload["theme"] == "pack-a"));
        assert_eq!(items[0].id.0, "wallpapers:pack-a:0");
        assert_eq!(items[1].id.0, "wallpapers:pack-a:1");

        // Switching the desktop's theme switches the strip, and the revision
        // moves so a client re-searches instead of keeping the old items.
        let before = provider.revision();
        let provider = provider.with_theme(ThemerState::fixture_named("pack-b"));
        let items = search(&provider, "");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subtitle.as_deref(), Some("pack-b pack · 1-c.jpg"));
        assert_ne!(provider.revision(), before);
    }

    #[test]
    fn an_active_pack_without_wallpapers_says_so() {
        let (_dir, themes) = pack_tree(&[("bare", &[]), ("pack", &["1-a.webp"])]);
        let packs: &[(&str, &[&str])] = &[("bare", &[]), ("pack", &["1-a.webp"])];
        let (provider, _catalog) = provider(themes, packs);
        assert!(provider.status().is_none());
        let provider = provider.with_theme(ThemerState::fixture_named("bare"));
        assert!(search(&provider, "").is_empty());
        assert_eq!(
            provider.status().as_deref(),
            Some("The active theme \"bare\" ships no wallpapers.")
        );
    }

    #[test]
    fn identities_that_escape_their_pack_never_resolve() {
        let (_dir, themes) = pack_tree(&[("pack", &["1-a.webp"])]);
        let location = WallpaperLocation::at(themes.clone());
        assert!(location.path_for("pack", "1-a.webp").is_some());
        for id in [
            "",
            ".",
            "..",
            "../1-a.webp",
            "sub/../1-a.webp",
            "/etc/passwd",
        ] {
            assert!(location.path_for("pack", id).is_none(), "accepted {id:?}");
        }
        for slug in ["", "..", "PACK", "pack/.."] {
            assert!(
                location.path_for(slug, "1-a.webp").is_none(),
                "accepted {slug:?}"
            );
        }
        // A directory is not a wallpaper.
        std::fs::create_dir_all(themes.join("pack").join("backgrounds").join("dir.webp")).unwrap();
        assert!(location.path_for("pack", "dir.webp").is_none());
        // No resolvable pack root means no previews, never a guess.
        assert!(
            WallpaperLocation::default()
                .path_for("pack", "1-a.webp")
                .is_none()
        );
    }

    #[test]
    fn one_item_is_published_per_wallpaper_in_catalog_order() {
        let (_dir, themes) = pack_tree(&[
            ("catppuccin", &["1-totoro.webp", "2-waves.webp"]),
            ("gruvbox", &["1-the-backwater.jpg"]),
        ]);
        let (provider, _catalog) = provider(
            themes,
            &[
                ("catppuccin", &["1-totoro.webp", "2-waves.webp"]),
                ("gruvbox", &["1-the-backwater.jpg"]),
            ],
        );
        // Browsing is explicit-scope only: never mixed into ordinary search.
        assert!(provider.requires_explicit_scope());
        assert_eq!(provider.kind(), "wallpaper");
        let items = search(&provider, "");
        assert_eq!(
            items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            ["Totoro", "Waves", "The Backwater"]
        );
        assert_eq!(
            items[0].subtitle.as_deref(),
            Some("catppuccin pack · 1-totoro.webp")
        );
        assert_eq!(items[0].payload["theme"], "catppuccin");
        assert_eq!(items[0].payload["wallpaper"], "1-totoro.webp");
        // Read-only construction authorizes nothing.
        assert!(provider.actions(&items[0]).is_empty());
        // The projection carries a preview asset reference, not a path.
        let presentation = provider.presentation(&items[0]);
        assert_eq!(presentation.kind, "wallpaper");
        match presentation.icon.unwrap() {
            Icon::Asset { asset, .. } => {
                assert_eq!(asset.representations, vec!["previewBgra8".to_owned()]);
            }
            other => panic!("expected a preview asset, got {other:?}"),
        }
        match presentation.data.unwrap() {
            DomainData::WallpaperSummary {
                theme_slug,
                wallpaper_id,
                ..
            } => {
                assert_eq!(theme_slug, "catppuccin");
                assert_eq!(wallpaper_id, "1-totoro.webp");
            }
            other => panic!("expected a wallpaper summary, got {other:?}"),
        }
        let presentation = provider.presentation(&items[0]);
        let wire = serde_json::to_string(&(presentation.icon, presentation.data)).unwrap();
        assert!(
            !wire.contains("/themes/"),
            "no pack path crosses the wire: {wire}"
        );
    }

    #[test]
    fn a_wallpaper_missing_from_disk_only_hides_itself() {
        let (_dir, themes) = pack_tree(&[("pack", &["1-a.webp"])]);
        let (provider, _catalog) = provider(themes, &[("pack", &["1-a.webp", "2-gone.webp"])]);
        let items = search(&provider, "");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "A");
    }

    #[test]
    fn the_strip_order_survives_the_shared_ranker() {
        let (_dir, themes) = pack_tree(&[
            ("pack-b", &["1-b.webp"]),
            ("pack-a", &["1-a.webp", "2-a.webp"]),
        ]);
        let (provider, _catalog) = provider(
            themes,
            &[
                ("pack-b", &["1-b.webp"]),
                ("pack-a", &["1-a.webp", "2-a.webp"]),
            ],
        );
        // The coordinator ranks descending by score and breaks ties by title,
        // so the positional term has to keep catalog order among equal matches.
        let items = search(&provider, "");
        let mut ranked = items.clone();
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.title.cmp(&b.title))
        });
        assert_eq!(
            ranked
                .iter()
                .map(|item| item.id.0.as_str())
                .collect::<Vec<_>>(),
            [
                "wallpapers:pack-a:0",
                "wallpapers:pack-a:1",
                "wallpapers:pack-b:2"
            ]
        );
        assert_eq!(
            items
                .iter()
                .map(|item| item.id.0.as_str())
                .collect::<Vec<_>>(),
            [
                "wallpapers:pack-a:0",
                "wallpapers:pack-a:1",
                "wallpapers:pack-b:2"
            ]
        );
    }

    #[test]
    fn a_typed_query_narrows_without_reordering() {
        let (_dir, themes) = pack_tree(&[("pack", &["1-totoro.webp", "2-waves.webp"])]);
        let (provider, _catalog) =
            provider(themes, &[("pack", &["1-totoro.webp", "2-waves.webp"])]);
        let narrowed = search(&provider, "waves");
        assert_eq!(narrowed.len(), 1);
        assert_eq!(narrowed[0].title, "Waves");
        assert_eq!(search(&provider, "zzz").len(), 0);
    }

    #[test]
    fn asset_revisions_follow_the_catalog_and_the_file() {
        let (_dir, themes) = pack_tree(&[("pack", &["1-a.webp"])]);
        let path = themes.join("pack").join("backgrounds").join("1-a.webp");
        let first = asset_revision(1, &path);
        assert_eq!(first, asset_revision(1, &path));
        assert_ne!(first, asset_revision(2, &path));
        // A rewritten pack must re-decode instead of serving stale pixels.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, b"rewritten").unwrap();
        assert_ne!(first, asset_revision(1, &path));
    }

    fn install_helper(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("mac-themer");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    const RECORDING: &str = "#!/bin/sh\n\
        dir=$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\n\
        printf '%s\\n' \"$@\" > \"$dir/args.txt\"\n\
        cat \"$dir/response.json\"\n";

    fn write_response(dir: &Path, json: &str) {
        std::fs::write(dir.join("response.json"), json).unwrap();
    }

    #[test]
    fn a_successful_set_reports_the_wallpaper_label() {
        let dir = tempfile::tempdir().unwrap();
        let path = install_helper(dir.path(), RECORDING);
        write_response(
            dir.path(),
            r#"{"schema":1,"status":"set","theme":"pack","wallpaper":"1-totoro.webp","warnings":[]}"#,
        );
        let apply = WallpaperApply::new(path);
        let wallpaper = ThemeWallpaper::new("1-totoro.webp").unwrap();
        assert_eq!(
            apply.set("pack", &wallpaper).unwrap(),
            "Wallpaper set to Totoro."
        );
        let recorded = std::fs::read_to_string(dir.path().join("args.txt")).unwrap();
        assert_eq!(
            recorded.lines().collect::<Vec<_>>(),
            ["wallpaper", "pack", "--id", "1-totoro.webp", "--json"]
        );
        assert!(!apply.is_busy());
    }

    #[test]
    fn a_failed_or_mismatched_reply_is_never_reported_as_success() {
        let wallpaper = ThemeWallpaper::new("1-a.webp").unwrap();
        for (json, expected) in [
            (
                r#"{"schema":1,"status":"failed","theme":"pack","wallpaper":"1-a.webp","warnings":["wallpaperFailed"]}"#,
                "The wallpaper could not be changed.",
            ),
            (
                r#"{"schema":1,"status":"set","theme":"other","wallpaper":"1-a.webp","warnings":[]}"#,
                PARTIAL_MESSAGE,
            ),
            (
                r#"{"schema":1,"status":"set","theme":"pack","wallpaper":"2-b.webp","warnings":[]}"#,
                PARTIAL_MESSAGE,
            ),
            (
                r#"{"schema":1,"status":"set","theme":"pack","wallpaper":"1-a.webp","warnings":["mystery"]}"#,
                PARTIAL_MESSAGE,
            ),
            ("not json", PARTIAL_MESSAGE),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = install_helper(dir.path(), RECORDING);
            write_response(dir.path(), json);
            let apply = WallpaperApply::new(path);
            let error = apply.set("pack", &wallpaper).unwrap_err().to_string();
            assert!(
                error.starts_with(expected),
                "{json} produced {error:?}, expected {expected:?}"
            );
            assert!(!apply.is_busy());
        }
        // A non-zero exit and an invalid identity never reach a report either.
        let dir = tempfile::tempdir().unwrap();
        let apply = WallpaperApply::new(install_helper(dir.path(), "#!/bin/sh\nexit 3\n"));
        assert_eq!(
            apply.set("pack", &wallpaper).unwrap_err().to_string(),
            PARTIAL_MESSAGE
        );
        let invalid = ThemeWallpaper {
            id: "../1-a.webp".into(),
            label: "X".into(),
        };
        assert_eq!(
            apply.set("pack", &invalid).unwrap_err().to_string(),
            INVALID_MESSAGE
        );
        assert_eq!(
            apply.set("Bad Slug", &wallpaper).unwrap_err().to_string(),
            INVALID_MESSAGE
        );
    }

    #[test]
    fn a_mutation_enabled_provider_offers_exactly_one_action() {
        let (_dir, themes) = pack_tree(&[("pack", &["1-a.webp"])]);
        let catalog = crate::service::ThemeCatalog::from_catalog(
            vec![palette("pack")],
            [("pack".to_owned(), vec!["1-a.webp".to_owned()])]
                .into_iter()
                .collect(),
        )
        .unwrap();
        let assets = crate::service::AssetStore::new(Arc::new(NoIcons));
        let helper = PathBuf::from("/nonexistent/mac-themer");
        let provider = WallpaperServiceProvider::with_apply(
            catalog,
            WallpaperLocation::at(themes),
            assets,
            WallpaperApply::new(helper),
        );
        let items = search(&provider, "");
        let actions = provider.actions(&items[0]);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0].id.0, "set");
        assert_eq!(actions[0].title, "Set wallpaper");
        // Setting a picture is the action the user picked; no consent prompt.
        assert!(
            provider
                .action_confirmation(&items[0], &actions[0])
                .is_none()
        );
        assert_eq!(provider.status(), None);
    }

    #[test]
    fn a_missing_pack_root_is_reported_but_keeps_browsing() {
        let catalog = crate::service::ThemeCatalog::from_catalog(
            vec![palette("pack")],
            [("pack".to_owned(), vec!["1-a.webp".to_owned()])]
                .into_iter()
                .collect(),
        )
        .unwrap();
        let assets = crate::service::AssetStore::new(Arc::new(NoIcons));
        let provider = WallpaperServiceProvider::new(catalog, WallpaperLocation::default(), assets);
        assert!(
            provider
                .status()
                .unwrap()
                .contains("previews are unavailable")
        );
        assert!(search(&provider, "").is_empty());
        let _ = ProviderConfig::default();
    }
}
