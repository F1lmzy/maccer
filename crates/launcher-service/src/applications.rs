//! Frontend-neutral service adapter for the real application provider.
//!
//! `provider-apps` keeps bundle paths and the launch path in its private core
//! item payload. This adapter is the only place those items cross the service
//! boundary, so it projects them into protocol data that never contains a path:
//! the public ID is an opaque hash and icons become lazy asset references.

use crate::{
    AssetStore, IconResolver, ItemPresentation, Service, ServiceProvider, ThemeCatalog,
    ThemeServiceProvider, ThemerState,
};
use anyhow::Result;
use launcher_core::{
    Action, ActionOutcome, CancellationToken, HistoryStore, IconDescriptor, Item, Preview,
    Provider, ProviderConfig, ProviderId, SearchContext, SearchQuery,
};
use launcher_protocol::{ContentBlock, DomainData, Icon, MetadataEntry};
use provider_apps::ApplicationProvider;
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::Arc,
};

/// Wraps the core application provider so its results can cross the service
/// boundary without exposing private bundle paths or payloads.
pub struct ApplicationServiceProvider {
    inner: ApplicationProvider,
    assets: Arc<AssetStore>,
}

impl ApplicationServiceProvider {
    pub fn new(provider: ApplicationProvider, assets: Arc<AssetStore>) -> Self {
        Self {
            inner: provider,
            assets,
        }
    }
}

/// Deterministic, path-free public identifier for an application result.
///
/// Core item IDs embed an absolute bundle path when the bundle has no
/// identifier, so the public ID is a hash instead. Hashing the stable core ID
/// keeps public IDs identical across catalog revisions and independent of
/// asset-cache evictions.
fn opaque_id(item: &Item) -> String {
    let mut hasher = DefaultHasher::new();
    item.id.0.hash(&mut hasher);
    format!("application-{:016x}", hasher.finish())
}

impl ServiceProvider for ApplicationServiceProvider {
    fn public_id(&self, item: &Item) -> String {
        opaque_id(item)
    }
    fn description(&self) -> &str {
        "Installed applications"
    }
    fn kind(&self) -> &str {
        "application"
    }
    fn presentation(&self, item: &Item) -> ItemPresentation {
        let fallback = item.title.clone();
        let icon = match &item.icon {
            // Lazy: only registers an opaque reference. No icon I/O happens here.
            Some(IconDescriptor::ApplicationBundle(path)) => Icon::Asset {
                asset: self.assets.reference(path, self.inner.revision()),
                fallback_text: fallback.clone(),
            },
            _ => Icon::Glyph {
                name: "application".into(),
                fallback_text: fallback.clone(),
            },
        };
        ItemPresentation {
            kind: "application".into(),
            icon: Some(icon),
            content: vec![],
            data: Some(DomainData::Application {
                schema_version: 1,
                application_id: opaque_id(item),
            }),
        }
    }
    fn details_available(&self, _: &Item) -> bool {
        true
    }
    fn details(&self, item: &Item) -> Result<Vec<ContentBlock>> {
        let mut entries = vec![
            MetadataEntry {
                label: "Name".into(),
                value: item.title.clone(),
            },
            MetadataEntry {
                label: "Type".into(),
                value: "Application".into(),
            },
            MetadataEntry {
                label: "Application ID".into(),
                value: opaque_id(item),
            },
        ];
        if let Some(subtitle) = &item.subtitle {
            entries.push(MetadataEntry {
                label: "Description".into(),
                value: subtitle.clone(),
            });
        }
        let mut content = vec![ContentBlock::Metadata {
            entries,
            fallback_text: format!("{} application details", item.title),
        }];
        if let Some(IconDescriptor::ApplicationBundle(path)) = &item.icon {
            content.push(ContentBlock::Image {
                asset: self.assets.reference(path, self.inner.revision()),
                alt: format!("{} icon", item.title),
                fallback_text: format!("{} icon", item.title),
            });
        }
        Ok(content)
    }
}

impl Provider for ApplicationServiceProvider {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn revision(&self) -> u64 {
        self.inner.revision()
    }
    fn status(&self) -> Option<String> {
        self.inner.status()
    }
    fn supports_preview(&self, item: &Item) -> bool {
        self.inner.supports_preview(item)
    }
    fn preview_revision(&self, item: &Item) -> u64 {
        self.inner.preview_revision(item)
    }
    fn supports_drag(&self, item: &Item) -> bool {
        self.inner.supports_drag(item)
    }
    fn preview(&self, item: &Item, cancellation: &CancellationToken) -> Result<Option<Preview>> {
        self.inner.preview(item, cancellation)
    }
    fn begin_drag(&self, item: &Item) -> Result<()> {
        self.inner.begin_drag(item)
    }
    fn requires_explicit_scope(&self) -> bool {
        self.inner.requires_explicit_scope()
    }
    fn records_usage(&self) -> bool {
        self.inner.records_usage()
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        self.inner.search(query, ctx)
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        self.inner.actions(item)
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        self.inner.activate(item, action)
    }
    fn activate_with_cancellation(
        &self,
        item: &Item,
        action: &Action,
        cancellation: &CancellationToken,
    ) -> Result<ActionOutcome> {
        self.inner
            .activate_with_cancellation(item, action, cancellation)
    }
}

/// Builds a service whose only provider is real application search, with icons
/// delivered lazily through `resolver`.
///
/// Live construction never touches the desktop by itself; activation happens
/// only after a client invokes an authorized action.
pub fn application_service(
    provider: ApplicationProvider,
    config: ProviderConfig,
    history: Arc<HistoryStore>,
    resolver: Arc<dyn IconResolver>,
) -> Result<Arc<Service>> {
    let assets = AssetStore::new(resolver);
    let registration: Arc<dyn ServiceProvider> =
        Arc::new(ApplicationServiceProvider::new(provider, assets.clone()));
    Service::with_assets(vec![(registration, config)], history, assets)
}

/// Same as [`application_service`], but also publishes read-only active theme
/// state. Used by the live `--apps` daemon; the plain constructor stays themeless.
pub fn application_service_with_theme(
    provider: ApplicationProvider,
    config: ProviderConfig,
    history: Arc<HistoryStore>,
    resolver: Arc<dyn IconResolver>,
    theme: Arc<ThemerState>,
) -> Result<Arc<Service>> {
    let assets = AssetStore::new(resolver);
    let registration: Arc<dyn ServiceProvider> =
        Arc::new(ApplicationServiceProvider::new(provider, assets.clone()));
    Service::with_state(vec![(registration, config)], history, Some(assets), theme)
}

/// Explicit live construction with read-only theme browsing. Older constructors
/// remain hermetic and never discover or run a catalog helper.
pub fn application_service_with_theme_catalog(
    provider: ApplicationProvider,
    config: ProviderConfig,
    history: Arc<HistoryStore>,
    resolver: Arc<dyn IconResolver>,
    theme: Arc<ThemerState>,
    catalog: Arc<ThemeCatalog>,
    theme_config: ProviderConfig,
) -> Result<Arc<Service>> {
    application_service_with_themes(
        provider,
        config,
        history,
        resolver,
        theme,
        Arc::new(ThemeServiceProvider::new(catalog)),
        theme_config,
    )
}

/// Explicit live construction with a caller-selected read-only or confirmed-Apply
/// Themes provider. Older factories never enable mutations implicitly.
pub fn application_service_with_themes(
    provider: ApplicationProvider,
    config: ProviderConfig,
    history: Arc<HistoryStore>,
    resolver: Arc<dyn IconResolver>,
    theme: Arc<ThemerState>,
    themes: Arc<ThemeServiceProvider>,
    theme_config: ProviderConfig,
) -> Result<Arc<Service>> {
    let assets = AssetStore::new(resolver);
    let apps: Arc<dyn ServiceProvider> =
        Arc::new(ApplicationServiceProvider::new(provider, assets.clone()));
    let themes: Arc<dyn ServiceProvider> = themes;
    Service::with_state(
        vec![(apps, config), (themes, theme_config)],
        history,
        Some(assets),
        theme,
    )
}
