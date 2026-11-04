use launcher_agent::core::{
    Action, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig, ProviderId,
    SearchContext, SearchQuery,
};
use launcher_agent::protocol::{Route, RpcRequest};
use launcher_agent::service::{ItemPresentation, Service, ServiceProvider, Session};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

struct Delayed {
    entered: mpsc::SyncSender<()>,
    release: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
}
impl Provider for Delayed {
    fn id(&self) -> ProviderId {
        ProviderId("delay".into())
    }
    fn name(&self) -> &str {
        "Delayed fixture"
    }
    fn search(&self, query: &SearchQuery, context: &SearchContext) -> anyhow::Result<Vec<Item>> {
        if query.text == "slow" {
            self.entered.send(()).unwrap();
            while !self.release.load(Ordering::Acquire) {
                if context.cancellation.is_cancelled() {
                    self.cancelled.store(true, Ordering::Release);
                    anyhow::bail!("another session cancelled this search");
                }
                thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(vec![Item {
            id: ItemId(query.text.clone()),
            provider: self.id(),
            title: query.text.clone(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: Value::Null,
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![]
    }
    fn activate(&self, _: &Item, _: &Action) -> anyhow::Result<ActionOutcome> {
        unreachable!("fixture has no actions")
    }
}
impl ServiceProvider for Delayed {
    fn description(&self) -> &str {
        "Synthetic provider for session isolation tests"
    }
    fn kind(&self) -> &str {
        "fixture"
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "fixture".into(),
            icon: None,
            content: vec![],
            data: None,
        }
    }
}
fn initialize(service: &Arc<Service>) -> Session {
    let mut session = service.session();
    assert!(
        session
            .handle(RpcRequest::new(
                "hello",
                "hello",
                json!({"major":1,"clientName":"isolation"})
            ))
            .error
            .is_none()
    );
    session
}
fn request(session: &mut Session, method: &str, params: Value) -> Value {
    let response = session.handle(RpcRequest::new("request", method, params));
    assert!(response.error.is_none(), "{:?}", response.error);
    response.result.unwrap()
}

#[test]
fn overlapping_searches_in_two_sessions_do_not_cancel_each_other() {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let release = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(AtomicBool::new(false));
    let service = Service::new(
        vec![(
            Arc::new(Delayed {
                entered: entered_tx,
                release: release.clone(),
                cancelled: cancelled.clone(),
            }),
            ProviderConfig::default(),
        )],
        Arc::new(HistoryStore::in_memory().unwrap()),
    )
    .unwrap();
    let mut a = initialize(&service);
    let mut b = initialize(&service);
    let slow = thread::spawn(move || {
        request(
            &mut a,
            "views.open",
            json!({"route":{"type":"provider","providerId":"delay"},"query":"slow"}),
        )
    });
    entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let fast = request(
        &mut b,
        "views.open",
        json!({"route":{"type":"provider","providerId":"delay"},"query":"fast"}),
    );
    release.store(true, Ordering::Release);
    let slow = slow.join().unwrap();
    assert_eq!(fast["sections"][0]["items"][0]["title"], "fast");
    assert_eq!(slow["sections"][0]["items"][0]["title"], "slow");
    assert!(!cancelled.load(Ordering::Acquire));
}

#[test]
fn disabled_providers_have_neither_menu_entries_nor_accessible_routes() {
    let (entered, _) = mpsc::sync_channel(1);
    let service = Service::new(
        vec![(
            Arc::new(Delayed {
                entered,
                release: Arc::new(AtomicBool::new(true)),
                cancelled: Arc::new(AtomicBool::new(false)),
            }),
            ProviderConfig {
                enabled: false,
                ..ProviderConfig::default()
            },
        )],
        Arc::new(HistoryStore::in_memory().unwrap()),
    )
    .unwrap();
    assert!(service.providers().is_empty());
    let mut session = initialize(&service);
    let home = request(&mut session, "views.open", json!({}));
    assert!(home["sections"].as_array().unwrap().is_empty());
    let response = session.handle(RpcRequest::new(
        "scope",
        "views.open",
        json!({"route":Route::Provider {provider_id:"delay".into()}}),
    ));
    assert_eq!(response.error.unwrap().data.code, "PROVIDER_NOT_FOUND");
}
