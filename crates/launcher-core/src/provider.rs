use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ProviderId(pub String);
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ItemId(pub String);
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ActionId(pub String);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum IconDescriptor {
    /// Encoded PNG data, shared across result clones. Providers bound its size.
    Png(Arc<[u8]>),
    /// Application bundle whose Finder icon the UI resolves lazily, off the
    /// search path, and caches. Providers never decode it during search.
    ApplicationBundle(PathBuf),
    File(PathBuf),
    Text(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub id: ItemId,
    pub provider: ProviderId,
    pub title: String,
    pub subtitle: Option<String>,
    pub keywords: Vec<String>,
    pub icon: Option<IconDescriptor>,
    pub score: f64,
    pub payload: serde_json::Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Action {
    pub id: ActionId,
    pub title: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionOutcome {
    Close,
    KeepOpen(String),
    SetQuery(String),
    Output { title: String, text: String },
}

/// Lazy selected-item preview. PNG bytes are size-bounded by the provider.
#[derive(Clone, Debug)]
pub enum Preview {
    Text { text: String, truncated: bool },
    Image { png: Vec<u8> },
    Info(String),
}

pub trait Provider: Send + Sync {
    /// Changes when background data changes, so an open query can refresh.
    fn revision(&self) -> u64 {
        0
    }
    /// Loading or degraded-backend message shown alongside available results.
    fn status(&self) -> Option<String> {
        None
    }
    fn supports_preview(&self, _item: &Item) -> bool {
        false
    }
    fn preview_revision(&self, _item: &Item) -> u64 {
        self.revision()
    }
    fn supports_drag(&self, _item: &Item) -> bool {
        false
    }
    fn preview(&self, _item: &Item, _cancellation: &CancellationToken) -> Result<Option<Preview>> {
        Ok(None)
    }
    /// Called synchronously on the UI thread during a mouse drag event.
    fn begin_drag(&self, _item: &Item) -> Result<()> {
        bail!("dragging is not supported by this provider")
    }
    fn id(&self) -> ProviderId;
    fn name(&self) -> &str;
    /// Prefix-only providers must never participate in mixed search, regardless of config.
    fn requires_explicit_scope(&self) -> bool {
        false
    }
    /// Sensitive providers can opt out of persisted query/activation history.
    fn records_usage(&self) -> bool {
        true
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>>;
    fn actions(&self, item: &Item) -> Vec<Action>;
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome>;
    /// Long-running actions should override this and cooperate with cancellation.
    fn activate_with_cancellation(
        &self,
        item: &Item,
        action: &Action,
        cancellation: &CancellationToken,
    ) -> Result<ActionOutcome> {
        if cancellation.is_cancelled() {
            bail!("action cancelled");
        }
        self.activate(item, action)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    pub enabled: bool,
    pub prefix: Option<String>,
    pub priority: i32,
    pub default_search: bool,
    pub max_results: usize,
}
impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix: None,
            priority: 0,
            default_search: true,
            max_results: 10,
        }
    }
}

#[derive(Clone)]
pub struct ProviderDescriptor {
    pub id: ProviderId,
    pub name: String,
    pub prefix: Option<String>,
    pub config: ProviderConfig,
}

#[derive(Clone, Debug, Default)]
pub struct SearchQuery {
    pub raw: String,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct SearchContext {
    pub generation: u64,
    pub cancellation: CancellationToken,
    pub limit: usize,
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub(crate) struct RegisteredProvider {
    pub provider: Arc<dyn Provider>,
    pub config: ProviderConfig,
}

#[derive(Default)]
pub struct ProviderRegistry {
    pub(crate) providers: HashMap<String, RegisteredProvider>,
}
impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, provider: Arc<dyn Provider>, config: ProviderConfig) -> Result<()> {
        let id = provider.id();
        if id.0.trim().is_empty() {
            bail!("provider id cannot be empty");
        }
        if id.0 == "picker" {
            bail!("provider id 'picker' is reserved for the provider picker");
        }
        if let Some(prefix) = config.prefix.as_deref()
            && prefix.trim().is_empty()
        {
            bail!("provider prefix cannot be empty or whitespace");
        }
        if config.prefix.as_deref() == Some(";") {
            bail!("provider prefix ';' is reserved for the provider picker");
        }
        if let Some(prefix) = config.prefix.as_deref()
            && self
                .providers
                .values()
                .any(|registered| registered.config.prefix.as_deref() == Some(prefix))
        {
            bail!("provider prefix '{prefix}' is already registered");
        }
        if self.providers.contains_key(&id.0) {
            bail!("provider {} is already registered", id.0);
        }
        self.providers
            .insert(id.0.clone(), RegisteredProvider { provider, config });
        Ok(())
    }
    pub fn get(&self, id: &ProviderId) -> Option<Arc<dyn Provider>> {
        self.providers.get(&id.0).map(|p| p.provider.clone())
    }
    pub fn descriptors(&self) -> Vec<ProviderDescriptor> {
        let mut result: Vec<_> = self
            .providers
            .values()
            .map(|p| ProviderDescriptor {
                id: p.provider.id(),
                name: p.provider.name().to_owned(),
                prefix: p.config.prefix.clone(),
                config: p.config.clone(),
            })
            .collect();
        result.sort_by(|a, b| {
            b.config
                .priority
                .cmp(&a.config.priority)
                .then_with(|| a.id.0.cmp(&b.id.0))
        });
        result
    }
}
