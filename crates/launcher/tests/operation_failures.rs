use anyhow::{Result, bail};
use launcher_agent::core::{
    Action, ActionId, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig,
    ProviderId, SearchContext, SearchQuery,
};
use launcher_agent::protocol::RpcRequest;
use launcher_agent::service::{ItemPresentation, Service, ServiceProvider};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
struct Failing {
    calls: Arc<AtomicUsize>,
    panic: bool,
}
impl Provider for Failing {
    fn id(&self) -> ProviderId {
        ProviderId("failing".into())
    }
    fn name(&self) -> &str {
        "Failing"
    }
    fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
        Ok(vec![Item {
            id: ItemId("one".into()),
            provider: self.id(),
            title: "One".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: Value::Null,
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![Action {
            id: ActionId("do".into()),
            title: "Do".into(),
        }]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.panic {
            panic!("synthetic panic")
        }
        bail!("synthetic failure")
    }
}
impl ServiceProvider for Failing {
    fn description(&self) -> &str {
        "failure test"
    }
    fn kind(&self) -> &str {
        "test"
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "test".into(),
            icon: None,
            content: vec![],
            data: None,
        }
    }
}
fn setup(
    panic: bool,
) -> (
    Arc<AtomicUsize>,
    Arc<Service>,
    launcher_agent::service::Session,
    Value,
    Value,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(Failing {
        calls: calls.clone(),
        panic,
    });
    let service = Service::new(
        vec![(provider, ProviderConfig::default())],
        Arc::new(HistoryStore::in_memory().unwrap()),
    )
    .unwrap();
    let mut session = service.session();
    let h = session.handle(RpcRequest::new(
        "h",
        "hello",
        json!({"major":1,"clientName":"test"}),
    ));
    assert!(h.error.is_none());
    let view = session
        .handle(RpcRequest::new(
            "o",
            "views.open",
            json!({"route":{"type":"provider","providerId":"failing"}}),
        ))
        .result
        .unwrap();
    let item = &view["sections"][0]["items"][0];
    let params = json!({"viewId":view["viewId"],"revision":view["revision"],"actionRef":item["actions"][0]["actionRef"],"operationId":"op-1"});
    (calls, service, session, view, params)
}
#[test]
fn provider_errors_after_activation_are_deduplicated() {
    let (calls, _service, mut session, _view, params) = setup(false);
    for _ in 0..2 {
        let response = session.handle(RpcRequest::new("invoke", "actions.invoke", params.clone()));
        assert_eq!(response.error.unwrap().data.code, "ACTION_FAILED");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[test]
fn provider_panics_after_activation_are_deduplicated() {
    let (calls, _service, mut session, _view, params) = setup(true);
    for _ in 0..2 {
        let response = session.handle(RpcRequest::new("invoke", "actions.invoke", params.clone()));
        assert_eq!(response.error.unwrap().data.code, "PROVIDER_PANIC");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
#[test]
fn validation_failure_does_not_consume_operation_id() {
    let (calls, _service, mut session, view, mut params) = setup(false);
    params["actionRef"] = json!("forged");
    let response = session.handle(RpcRequest::new("bad", "actions.invoke", params.clone()));
    assert_eq!(response.error.unwrap().data.code, "STALE_ACTION");
    params["actionRef"] = view["sections"][0]["items"][0]["actions"][0]["actionRef"].clone();
    let response = session.handle(RpcRequest::new("valid", "actions.invoke", params));
    assert_eq!(response.error.unwrap().data.code, "ACTION_FAILED");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
