use launcher_core::{HistoryStore, Item, ItemId, ProviderId};
#[test]
fn indexed_candidate_pools_do_not_exceed_sqlite_bind_limits() {
    let history = HistoryStore::in_memory().unwrap();
    let items: Vec<_> = (0..20_000)
        .map(|n| Item {
            id: ItemId(format!("file:{n}")),
            provider: ProviderId("files".into()),
            title: n.to_string(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.,
            payload: serde_json::Value::Null,
        })
        .collect();
    assert!(history.snapshot_for(&items).unwrap().is_empty());
}
