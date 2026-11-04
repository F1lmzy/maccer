//! Configured web engines behind server-retained action references.
use crate::core::{Action, ActionOutcome, Item, Provider, ProviderId, SearchContext, SearchQuery};
use crate::protocol::Icon;
use crate::providers::web::WebProvider;
use crate::service::{ItemPresentation, ServiceProvider};
use anyhow::Result;

pub struct WebServiceProvider {
    inner: WebProvider,
}
impl WebServiceProvider {
    pub fn new(inner: WebProvider) -> Self {
        Self { inner }
    }
}
impl Provider for WebServiceProvider {
    fn id(&self) -> ProviderId {
        self.inner.id()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        let mut items = self.inner.search(query, ctx)?;
        // Core merging sorts equal scores by title. Preserve the configured
        // default-engine order rather than alphabetically preferring another engine.
        for (index, item) in items.iter_mut().enumerate() {
            item.score = -(index as f64);
        }
        Ok(items)
    }
    fn actions(&self, item: &Item) -> Vec<Action> {
        self.inner.actions(item)
    }
    fn activate(&self, item: &Item, action: &Action) -> Result<ActionOutcome> {
        self.inner.activate(item, action)
    }
}
impl ServiceProvider for WebServiceProvider {
    fn description(&self) -> &str {
        "Search the web with configured engines"
    }
    fn kind(&self) -> &str {
        "web"
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "web".into(),
            icon: Some(Icon::Glyph {
                name: "web".into(),
                fallback_text: "↗".into(),
            }),
            content: vec![],
            data: None,
        }
    }
}
