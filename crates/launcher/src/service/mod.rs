//! Shared Maccer service sessions, searchable home, and optional live application adapters.
mod applications;
mod assets;
mod clipboard;
mod fixtures;
mod frontend;
mod menu;
mod server;
mod session;
mod theme_apply;
mod theme_catalog;
mod themer;
mod themes;
mod web;

use crate::core::{HistoryStore, Provider, ProviderConfig, ProviderRegistry, SearchCoordinator};
use crate::protocol::{HelloResult, PROTOCOL_MAJOR, PROTOCOL_MINOR};
use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

pub use applications::{
    ApplicationServiceProvider, application_service, application_service_with_theme,
    application_service_with_theme_catalog, application_service_with_themes,
};
pub use assets::{AssetStore, IconResolver, NativeIconResolver};
pub use clipboard::{ClipboardMonitor, ClipboardServiceProvider};
pub use frontend::FrontendRouter;
pub use menu::ItemPresentation;
pub use server::{MAX_CLIENTS, Server};
pub use session::Session;
pub use theme_apply::ThemeApply;
pub use theme_catalog::{CatalogSnapshot, ThemeCatalog};
pub use themer::ThemerState;
pub use themes::ThemeServiceProvider;
pub use web::WebServiceProvider;

pub trait ServiceProvider: Provider {
    fn public_id(&self, item: &crate::core::Item) -> String {
        item.id.0.clone()
    }
    fn description(&self) -> &str;
    fn kind(&self) -> &str;
    fn presentation(&self, item: &crate::core::Item) -> ItemPresentation;
    /// Whether this result has lazily fetchable, provider-projected detail content.
    fn details_available(&self, _item: &crate::core::Item) -> bool {
        false
    }
    /// Plain confirmation prompt the frontend must show before invoking this
    /// action, or `None` when no confirmation is required. The service, not the
    /// client, decides this for every action it publishes.
    fn action_confirmation(
        &self,
        _item: &crate::core::Item,
        _action: &crate::core::Action,
    ) -> Option<String> {
        None
    }
    /// Returns safe display blocks. The service enforces count and serialized-byte limits.
    fn details(&self, _item: &crate::core::Item) -> Result<Vec<crate::protocol::ContentBlock>> {
        Ok(Vec::new())
    }
}

pub(crate) struct Registration {
    provider: Arc<dyn ServiceProvider>,
    config: ProviderConfig,
}
pub struct Service {
    pub(crate) providers: Vec<Registration>,
    pub(crate) registry: Arc<ProviderRegistry>,
    pub(crate) history: Arc<HistoryStore>,
    pub(crate) assets: Option<Arc<AssetStore>>,
    pub(crate) theme: Option<Arc<ThemerState>>,
    pub(crate) instance_id: String,
    pub(crate) frontend: std::sync::OnceLock<Arc<FrontendRouter>>,
    next_session: AtomicU64,
}
impl Service {
    pub fn new(
        registrations: Vec<(Arc<dyn ServiceProvider>, ProviderConfig)>,
        history: Arc<HistoryStore>,
    ) -> Result<Arc<Self>> {
        Self::build(registrations, history, None, None)
    }
    pub fn with_assets(
        registrations: Vec<(Arc<dyn ServiceProvider>, ProviderConfig)>,
        history: Arc<HistoryStore>,
        assets: Arc<AssetStore>,
    ) -> Result<Arc<Self>> {
        Self::build(registrations, history, Some(assets), None)
    }
    /// Attach read-only active theme state. `assets`/`theme` are optional so
    /// hermetic callers keep advertising no theme capability.
    pub fn with_state(
        registrations: Vec<(Arc<dyn ServiceProvider>, ProviderConfig)>,
        history: Arc<HistoryStore>,
        assets: Option<Arc<AssetStore>>,
        theme: Arc<ThemerState>,
    ) -> Result<Arc<Self>> {
        Self::build(registrations, history, assets, Some(theme))
    }
    fn build(
        registrations: Vec<(Arc<dyn ServiceProvider>, ProviderConfig)>,
        history: Arc<HistoryStore>,
        assets: Option<Arc<AssetStore>>,
        theme: Option<Arc<ThemerState>>,
    ) -> Result<Arc<Self>> {
        let mut registry = ProviderRegistry::new();
        let mut providers = Vec::new();
        for (provider, config) in registrations {
            let core: Arc<dyn Provider> = provider.clone();
            registry.register(core, config.clone())?;
            providers.push(Registration { provider, config });
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        Ok(Arc::new(Self {
            providers,
            registry: Arc::new(registry),
            history,
            assets,
            theme,
            instance_id: format!("{nonce:x}"),
            frontend: std::sync::OnceLock::new(),
            next_session: AtomicU64::new(1),
        }))
    }
    /// Attach the single configured frontend controller before serving clients.
    pub fn attach_frontend(&self, frontend: Arc<FrontendRouter>) -> Result<()> {
        self.frontend
            .set(frontend)
            .map_err(|_| anyhow::anyhow!("frontend routing is already attached"))
    }
    /// Creates inert fixture providers; never reads desktop state or activates applications.
    pub fn fixture() -> Result<Arc<Self>> {
        fixtures::service()
    }
    pub fn session(self: &Arc<Self>) -> Session {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        Session::new(self.clone(), format!("session-{id}"))
    }
    pub fn providers(&self) -> Vec<crate::protocol::ProviderDescriptor> {
        self.providers
            .iter()
            .filter(|p| p.config.enabled)
            .map(|p| crate::protocol::ProviderDescriptor {
                id: p.provider.id().0,
                name: p.provider.name().into(),
                description: p.provider.description().into(),
                enabled: true,
                explicit_scope: p.provider.requires_explicit_scope(),
                kind: p.provider.kind().into(),
            })
            .collect()
    }
    pub(crate) fn coordinator(&self) -> SearchCoordinator {
        SearchCoordinator::new(
            self.registry.clone(),
            self.history.clone(),
            crate::protocol::MAX_RESULTS,
        )
    }
}

pub(crate) fn hello(
    instance_id: &str,
    session_id: &str,
    params: crate::protocol::HelloParams,
) -> Result<HelloResult, crate::protocol::RpcError> {
    use crate::protocol::RpcError;
    if params.major != PROTOCOL_MAJOR {
        return Err(RpcError::new(
            -32602,
            "UNSUPPORTED_VERSION",
            format!("protocol major {} is unsupported", params.major),
        ));
    }
    if params.client_name.trim().is_empty() || params.client_name.len() > 128 {
        return Err(RpcError::new(
            -32602,
            "INVALID_CLIENT",
            "clientName must contain 1 to 128 bytes",
        ));
    }
    Ok(HelloResult {
        major: PROTOCOL_MAJOR,
        minor: PROTOCOL_MINOR,
        instance_id: instance_id.into(),
        session_id: session_id.into(),
        capabilities: vec![
            "searchableHome".into(),
            "providerNavigation".into(),
            "actionInvocation".into(),
            "actionConfirmation".into(),
            "itemDetails".into(),
        ],
    })
}
#[cfg(test)]
mod tests {
    #[test]
    fn fixture_service_has_menu_providers() {
        let s = super::Service::fixture().unwrap();
        let p = s.providers();
        assert!(p.iter().any(|p| p.id == "themes"));
        assert!(p.iter().any(|p| p.id == "wallpapers"));
    }
}
