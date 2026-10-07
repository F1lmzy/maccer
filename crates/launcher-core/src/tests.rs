use crate::*;
use anyhow::{Result, bail};
use serde_json::json;
use std::{sync::Arc, time::Duration};

struct TestProvider {
    id: &'static str,
    fails: bool,
}

struct ExplicitProvider;
impl Provider for ExplicitProvider {
    fn id(&self) -> ProviderId {
        ProviderId("explicit".into())
    }
    fn name(&self) -> &str {
        "Explicit"
    }
    fn requires_explicit_scope(&self) -> bool {
        true
    }
    fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        Ok(vec![item("explicit", "command", "command")])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        Ok(ActionOutcome::Close)
    }
}

#[test]
fn explicit_providers_cannot_enter_mixed_search_even_if_misconfigured() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(Arc::new(ExplicitProvider), config(Some(">"), true, true, 1))
        .unwrap();
    let coordinator = SearchCoordinator::new(
        Arc::new(registry),
        Arc::new(HistoryStore::in_memory().unwrap()),
        8,
    );
    let mixed = coordinator
        .start("command".into())
        .receiver
        .recv_blocking()
        .unwrap();
    assert!(mixed.complete);
    assert!(mixed.items.is_empty());
    let explicit = coordinator
        .start("> command".into())
        .receiver
        .recv_blocking()
        .unwrap();
    assert_eq!(explicit.items.len(), 1);
}

struct DelayProvider {
    id: &'static str,
    delay: Duration,
}

impl Provider for DelayProvider {
    fn id(&self) -> ProviderId {
        ProviderId(self.id.into())
    }
    fn name(&self) -> &str {
        self.id
    }
    fn search(&self, query: &SearchQuery, ctx: &SearchContext) -> Result<Vec<Item>> {
        std::thread::sleep(self.delay);
        if ctx.cancellation.is_cancelled() {
            return Ok(Vec::new());
        }
        Ok(vec![item(self.id, self.id, &query.text)])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        Ok(ActionOutcome::Close)
    }
}
impl Provider for TestProvider {
    fn id(&self) -> ProviderId {
        ProviderId(self.id.into())
    }
    fn name(&self) -> &str {
        self.id
    }
    fn search(&self, query: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        if self.fails {
            bail!("provider failed")
        }
        Ok(vec![item(self.id, self.id, &query.text)])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        Ok(ActionOutcome::Close)
    }
}
fn item(provider: &str, id: &str, title: &str) -> Item {
    Item {
        id: ItemId(id.into()),
        provider: ProviderId(provider.into()),
        title: title.into(),
        subtitle: None,
        keywords: vec![],
        icon: None,
        score: 0.0,
        payload: json!({}),
    }
}
fn config(
    prefix: Option<&str>,
    enabled: bool,
    default_search: bool,
    priority: i32,
) -> ProviderConfig {
    ProviderConfig {
        enabled,
        prefix: prefix.map(str::to_owned),
        priority,
        default_search,
        max_results: 10,
    }
}
fn registry() -> ProviderRegistry {
    let mut registry = ProviderRegistry::new();
    registry
        .register(
            Arc::new(TestProvider {
                id: "files",
                fails: false,
            }),
            config(Some("/"), true, false, 1),
        )
        .unwrap();
    registry
        .register(
            Arc::new(TestProvider {
                id: "apps",
                fails: false,
            }),
            config(Some("/apps"), true, true, 2),
        )
        .unwrap();
    registry
        .register(
            Arc::new(TestProvider {
                id: "disabled",
                fails: false,
            }),
            config(Some("!"), false, true, 0),
        )
        .unwrap();
    registry
}

#[test]
fn longest_provider_prefix_wins_and_disabled_is_ignored() {
    let parsed = registry().parse("/apps safari");
    assert_eq!(parsed.mode, QueryMode::Provider(ProviderId("apps".into())));
    assert_eq!(parsed.search_text, "safari");
    assert_eq!(registry().parse("! x").mode, QueryMode::Mixed);
}
#[test]
fn prefix_without_query_remains_a_provider_query() {
    assert_eq!(
        registry().parse("/").mode,
        QueryMode::Provider(ProviderId("files".into()))
    );
    assert_eq!(registry().parse(";").mode, QueryMode::ProviderPicker);
}
#[test]
fn word_prefix_needs_boundary() {
    let mut r = ProviderRegistry::new();
    r.register(
        Arc::new(TestProvider {
            id: "web",
            fails: false,
        }),
        config(Some("web"), true, true, 0),
    )
    .unwrap();
    assert_eq!(r.parse("website").mode, QueryMode::Mixed);
    assert_eq!(r.parse("web cats").search_text, "cats");
}
#[test]
fn registration_rejects_reserved_picker_id_and_duplicate_prefixes() {
    let mut registry = ProviderRegistry::new();
    assert!(
        registry
            .register(
                Arc::new(TestProvider {
                    id: "empty-prefix",
                    fails: false,
                }),
                config(Some(" "), true, false, 0),
            )
            .is_err()
    );
    assert!(
        registry
            .register(
                Arc::new(TestProvider {
                    id: "picker",
                    fails: false
                }),
                config(Some(";"), true, false, 0),
            )
            .is_err()
    );
    registry
        .register(
            Arc::new(TestProvider {
                id: "one",
                fails: false,
            }),
            config(Some("/"), true, false, 0),
        )
        .unwrap();
    assert!(
        registry
            .register(
                Arc::new(TestProvider {
                    id: "two",
                    fails: false
                }),
                config(Some("/"), true, false, 0),
            )
            .is_err()
    );
}

#[test]
fn picker_omits_providers_without_a_selectable_prefix() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(
            Arc::new(TestProvider {
                id: "unprefixed",
                fails: false,
            }),
            config(None, true, true, 0),
        )
        .unwrap();
    let coordinator = SearchCoordinator::new(
        Arc::new(registry),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let session = coordinator.start(";".into());
    let update = session.receiver.recv_blocking().unwrap();
    assert!(update.complete);
    assert!(update.items.is_empty());
}

#[test]
fn empty_search_completes_even_with_no_eligible_providers() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(
            Arc::new(TestProvider {
                id: "disabled",
                fails: false,
            }),
            config(None, false, true, 0),
        )
        .unwrap();
    let coordinator = SearchCoordinator::new(
        Arc::new(registry),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let update = coordinator
        .start("query".into())
        .receiver
        .recv_blocking()
        .unwrap();
    assert!(update.complete);
    assert!(update.items.is_empty());
}

#[test]
fn fast_provider_streams_before_slow_provider_finishes() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(
            Arc::new(DelayProvider {
                id: "slow",
                delay: Duration::from_millis(250),
            }),
            config(None, true, true, 0),
        )
        .unwrap();
    registry
        .register(
            Arc::new(DelayProvider {
                id: "fast",
                delay: Duration::from_millis(5),
            }),
            config(None, true, true, 0),
        )
        .unwrap();
    let coordinator = SearchCoordinator::new(
        Arc::new(registry),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let session = coordinator.start("query".into());
    let update = session.receiver.recv_blocking().unwrap();
    assert!(!update.complete);
    assert!(update.items.iter().any(|item| item.provider.0 == "fast"));
}

#[test]
fn selection_survives_reordering_and_clears_on_empty() {
    let a = item("apps", "a", "A");
    let b = item("apps", "b", "B");
    let mut s = Selection::default();
    s.reconcile(&[a.clone(), b.clone()]);
    s.move_by(&[a.clone(), b.clone()], 1);
    s.reconcile(&[b.clone(), a]);
    assert_eq!(
        s.current(std::slice::from_ref(&b)).unwrap().id,
        ItemId("b".into())
    );
    s.reconcile(&[]);
    assert!(s.current(&[]).is_none());
}
#[test]
fn ranking_uses_fuzzy_match_provider_priority_and_usage() {
    let history = HistoryStore::in_memory().unwrap();
    let popular = UsageEvent {
        query: "saf".into(),
        provider: ProviderId("apps".into()),
        item: ItemId("safari".into()),
        action: ActionId("open".into()),
        timestamp: 7,
    };
    history.record(&popular).unwrap();
    let mut items = vec![
        item("web", "web", "Safari"),
        item("apps", "safari", "Safari"),
    ];
    let usage = history.snapshot_for(&items).unwrap();
    crate::ranking::rank(
        &mut items,
        "saf",
        &std::collections::HashMap::from([("apps".to_owned(), 10)]),
        &usage,
    );
    assert_eq!(items[0].provider, ProviderId("apps".into()));
    assert!(items[0].score > items[1].score);
}

#[test]
fn exact_application_name_outranks_fuzzy_results_and_score_boosts() {
    let mut boosted = item("web", "web", "Search with Aerospace Tools");
    boosted.score = 10_000.0;
    let mut items = vec![boosted, item("apps", "aerospace", "AeroSpace")];
    crate::ranking::rank(
        &mut items,
        "aerospace",
        &std::collections::HashMap::from([("web".to_owned(), 1_000)]),
        &Default::default(),
    );
    assert_eq!(items[0].id, ItemId("aerospace".into()));
}

#[test]
fn ranking_prefers_recent_use_when_match_and_frequency_tie() {
    let history = HistoryStore::in_memory().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    for (id, timestamp) in [("old", now - 365 * 86_400), ("recent", now)] {
        history
            .record(&UsageEvent {
                query: "match".into(),
                provider: ProviderId("apps".into()),
                item: ItemId(id.into()),
                action: ActionId("open".into()),
                timestamp,
            })
            .unwrap();
    }
    let mut items = vec![
        item("apps", "old", "Same match"),
        item("apps", "recent", "Same match"),
    ];
    let usage = history.snapshot_for(&items).unwrap();
    crate::ranking::rank(&mut items, "match", &Default::default(), &usage);
    assert_eq!(items[0].id, ItemId("recent".into()));
}

#[test]
fn history_records_and_reads_usage() {
    let h = HistoryStore::in_memory().unwrap();
    let e = UsageEvent {
        query: "saf".into(),
        provider: ProviderId("apps".into()),
        item: ItemId("safari".into()),
        action: ActionId("open".into()),
        timestamp: 42,
    };
    h.record(&e).unwrap();
    h.record(&e).unwrap();
    assert_eq!(
        h.stats(&e.provider, &e.item).unwrap(),
        UsageStats {
            count: 2,
            last_used: Some(42)
        }
    );
}
#[test]
fn search_streams_results_and_isolates_provider_failures() {
    let mut r = registry();
    r.register(
        Arc::new(TestProvider {
            id: "broken",
            fails: true,
        }),
        config(None, true, true, 0),
    )
    .unwrap();
    let c = SearchCoordinator::new(
        Arc::new(r),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let session = c.start("safari".into());
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut last = None;
    while std::time::Instant::now() < deadline {
        match session.receiver.try_recv() {
            Ok(update) => {
                if update.complete {
                    last = Some(update);
                    break;
                }
                last = Some(update);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(5)),
        }
    }
    let update = last.expect("search emitted update");
    assert!(
        update
            .items
            .iter()
            .any(|i| i.provider == ProviderId("apps".into()))
    );
    assert!(
        update
            .errors
            .iter()
            .any(|e| e.provider == ProviderId("broken".into()))
    );
}
#[test]
fn dropping_receiver_cancels_provider_work() {
    let mut registry = ProviderRegistry::new();
    registry
        .register(
            Arc::new(DelayProvider {
                id: "slow",
                delay: Duration::from_millis(60),
            }),
            config(None, true, true, 0),
        )
        .unwrap();
    let coordinator = SearchCoordinator::new(
        Arc::new(registry),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let session = coordinator.start("query".into());
    let cancellation = session.cancellation.clone();
    drop(session.receiver);
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !cancellation.is_cancelled() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(cancellation.is_cancelled());
}

#[test]
fn new_search_cancels_previous_generation() {
    let c = SearchCoordinator::new(
        Arc::new(registry()),
        Arc::new(HistoryStore::in_memory().unwrap()),
        10,
    );
    let first = c.start("one".into());
    let second = c.start("two".into());
    assert!(first.cancellation.is_cancelled());
    assert!(second.generation > first.generation);
}
