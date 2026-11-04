use anyhow::Result;
use launcher_agent::core::{
    Action, ActionId, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig,
    ProviderId, SearchContext, SearchQuery,
};
use launcher_agent::protocol::RpcRequest;
use launcher_agent::service::{ItemPresentation, Service, ServiceProvider};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Provider that advertises a confirmation prompt for its single action when
/// `requires` is true. `activate` counts calls so tests can prove the prompt is
/// enforced before any side effect.
struct Confirming {
    calls: Arc<AtomicUsize>,
    requires: Arc<AtomicBool>,
    panics: Arc<AtomicBool>,
}
impl Provider for Confirming {
    fn id(&self) -> ProviderId {
        ProviderId("confirming".into())
    }
    fn name(&self) -> &str {
        "Confirming"
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
            id: ActionId("apply".into()),
            title: "Apply".into(),
        }]
    }
    fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ActionOutcome::KeepOpen("applied".into()))
    }
}
impl ServiceProvider for Confirming {
    fn description(&self) -> &str {
        "confirmation test"
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
    fn action_confirmation(&self, _: &Item, _: &Action) -> Option<String> {
        assert!(!self.panics.load(Ordering::SeqCst), "prompt failed");
        self.requires
            .load(Ordering::SeqCst)
            .then(|| "Apply \"One\"? This changes the desktop.".to_string())
    }
}

struct Harness {
    calls: Arc<AtomicUsize>,
    requires: Arc<AtomicBool>,
    panics: Arc<AtomicBool>,
    session: launcher_agent::service::Session,
    view: Value,
}
impl Harness {
    fn new(requires: bool) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let requires = Arc::new(AtomicBool::new(requires));
        let panics = Arc::new(AtomicBool::new(false));
        let provider = Arc::new(Confirming {
            calls: calls.clone(),
            requires: requires.clone(),
            panics: panics.clone(),
        });
        let service = Service::new(
            vec![(provider, ProviderConfig::default())],
            Arc::new(HistoryStore::in_memory().unwrap()),
        )
        .unwrap();
        let mut session = service.session();
        let hello = session.handle(RpcRequest::new(
            "h",
            "hello",
            json!({"major":1,"clientName":"test"}),
        ));
        assert!(
            hello.result.unwrap()["capabilities"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "actionConfirmation"),
            "hello must advertise actionConfirmation"
        );
        let view = session
            .handle(RpcRequest::new(
                "o",
                "views.open",
                json!({"route":{"type":"provider","providerId":"confirming"}}),
            ))
            .result
            .unwrap();
        Self {
            calls,
            requires,
            panics,
            session,
            view,
        }
    }
    fn params(&self, operation_id: &str, confirmed: Option<bool>) -> Value {
        let item = &self.view["sections"][0]["items"][0];
        let mut params = json!({
            "viewId": self.view["viewId"],
            "revision": self.view["revision"],
            "actionRef": item["actions"][0]["actionRef"],
            "operationId": operation_id,
        });
        if let Some(confirmed) = confirmed {
            params["confirmed"] = json!(confirmed);
        }
        params
    }
    fn invoke(&mut self, params: Value) -> launcher_agent::protocol::RpcResponse {
        self.session
            .handle(RpcRequest::new("i", "actions.invoke", params))
    }
}

#[test]
fn confirmation_is_advertised_and_unconfirmed_invocation_is_rejected_before_activation() {
    let mut h = Harness::new(true);
    let descriptor = &h.view["sections"][0]["items"][0]["actions"][0];
    assert_eq!(descriptor["kind"], "invoke");
    assert_eq!(
        descriptor["confirmation"],
        "Apply \"One\"? This changes the desktop."
    );

    for confirmed in [Some(false), None] {
        let response = h.invoke(h.params("op-unconfirmed", confirmed));
        let error = response.error.unwrap();
        assert_eq!(error.data.code, "CONFIRMATION_REQUIRED");
        assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    }

    let response = h.invoke(h.params("op-confirmed", Some(true)));
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn rejected_confirmation_does_not_consume_operation_id_and_success_is_deduplicated() {
    let mut h = Harness::new(true);
    let rejected = h.invoke(h.params("op-shared", None));
    assert_eq!(rejected.error.unwrap().data.code, "CONFIRMATION_REQUIRED");
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);

    // The same operationId is still fresh, so a confirmed retry executes once.
    let accepted = h.invoke(h.params("op-shared", Some(true)));
    assert!(accepted.error.is_none(), "{:?}", accepted.error);
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);

    let duplicate = h.invoke(h.params("op-shared", Some(true)));
    assert!(duplicate.error.is_none(), "{:?}", duplicate.error);
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn confirmation_lookup_panics_cannot_publish_an_unprotected_action() {
    let mut h = Harness::new(true);
    h.panics.store(true, Ordering::SeqCst);
    let response = h.session.handle(RpcRequest::new(
        "q",
        "views.query",
        json!({"viewId":h.view["viewId"],"query":""}),
    ));
    assert_eq!(response.error.unwrap().data.code, "PROVIDER_PANIC");
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_published_confirmation_requirement_cannot_be_downgraded_in_place() {
    let mut h = Harness::new(true);
    h.requires.store(false, Ordering::SeqCst);
    let rejected = h.invoke(h.params("downgrade", None));
    assert_eq!(rejected.error.unwrap().data.code, "CONFIRMATION_REQUIRED");
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    assert!(h.invoke(h.params("downgrade", Some(true))).error.is_none());
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn provider_without_confirmation_accepts_unconfirmed_invocation() {
    let mut h = Harness::new(false);
    assert!(
        h.view["sections"][0]["items"][0]["actions"][0]
            .get("confirmation")
            .is_none()
    );
    let response = h.invoke(h.params("op-plain", None));
    assert!(response.error.is_none(), "{:?}", response.error);
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);
}
