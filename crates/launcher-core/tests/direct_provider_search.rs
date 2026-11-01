use std::sync::Arc;

use launcher_core::{
    Action, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig, ProviderId,
    ProviderRegistry, SearchContext, SearchCoordinator, SearchQuery,
};

struct Prefixless;
impl Provider for Prefixless {
    fn id(&self) -> ProviderId {
        ProviderId("prefixless".into())
    }
    fn name(&self) -> &str {
        "Prefixless"
    }
    fn search(&self, query: &SearchQuery, _: &SearchContext) -> anyhow::Result<Vec<Item>> {
        Ok(vec![Item {
            id: ItemId("empty".into()),
            provider: self.id(),
            title: format!("query:{}", query.text),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: serde_json::json!({}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> anyhow::Result<ActionOutcome> {
        Ok(ActionOutcome::Close)
    }
}

#[test]
fn direct_provider_route_searches_prefixless_provider_with_empty_query() {
    let provider = Arc::new(Prefixless);
    let mut registry = ProviderRegistry::new();
    registry
        .register(
            provider,
            ProviderConfig {
                prefix: None,
                default_search: false,
                ..ProviderConfig::default()
            },
        )
        .unwrap();
    let coordinator = SearchCoordinator::new(
        Arc::new(registry),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let session = coordinator.start_provider(String::new(), ProviderId("prefixless".into()));
    let update = session.receiver.recv_blocking().unwrap();
    assert!(update.complete);
    assert_eq!(update.items[0].title, "query:");
}

#[test]
fn direct_provider_route_rejects_unknown_provider_without_panicking() {
    let coordinator = SearchCoordinator::new(
        Arc::new(ProviderRegistry::new()),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let session = coordinator.start_provider(String::new(), ProviderId("missing".into()));
    let update = session.receiver.recv_blocking().unwrap();
    assert!(update.complete);
    assert!(update.items.is_empty());
    assert!(update.errors.is_empty());
}
