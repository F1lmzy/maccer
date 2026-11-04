use launcher_agent::core::{
    Action, ActionId, ActionOutcome, HistoryStore, Item, ItemId, Provider, ProviderConfig,
    ProviderId, SearchContext, SearchQuery,
};
use launcher_agent::protocol::{ContentBlock, RpcRequest};
use launcher_agent::service::{ItemPresentation, Service, ServiceProvider, Session};
use serde_json::{Value, json};
use std::sync::Arc;

fn session(service: &Arc<Service>) -> Session {
    let mut session = service.session();
    assert!(
        session
            .handle(RpcRequest::new(
                "hello",
                "hello",
                json!({"major":1,"clientName":"regression"})
            ))
            .error
            .is_none()
    );
    session
}
fn call(session: &mut Session, method: &str, params: Value) -> Value {
    let reply = session.handle(RpcRequest::new("test", method, params));
    assert!(reply.error.is_none(), "{:?}", reply.error);
    reply.result.unwrap()
}

#[test]
fn whitespace_home_queries_are_home_browsing_not_mixed_search() {
    let mut session = session(&Service::fixture().unwrap());
    let view = call(&mut session, "views.open", json!({"query":"  \t  "}));
    assert_eq!(view["sections"].as_array().unwrap().len(), 1);
    assert_eq!(view["sections"][0]["id"], "providers");
    assert!(
        view["sections"][0]["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["kind"] == "provider")
    );
}

#[test]
fn view_limit_applies_to_the_combined_menu_and_result_sections() {
    let mut session = session(&Service::fixture().unwrap());
    let view = call(
        &mut session,
        "views.open",
        json!({"query":"theme", "limit":1}),
    );
    let count: usize = view["sections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|section| section["items"].as_array().unwrap().len())
        .sum();
    assert_eq!(count, 1);
    assert_eq!(view["sections"][0]["items"][0]["id"], "themes");
    assert_eq!(view["truncated"], true);
}

#[test]
fn operation_ids_are_nonempty_and_bounded() {
    let mut session = session(&Service::fixture().unwrap());
    let view = call(&mut session, "views.open", json!({}));
    for operation_id in [String::new(), "x".repeat(129), "a\0b".into()] {
        let error = session.handle(RpcRequest::new("bad", "actions.invoke", json!({"viewId":view["viewId"],"revision":view["revision"],"actionRef":view["sections"][0]["items"][0]["actions"][0]["actionRef"],"operationId":operation_id}))).error.unwrap();
        assert_eq!(error.data.code, "INVALID_OPERATION_ID");
    }
}

struct UnicodeProvider;
impl Provider for UnicodeProvider {
    fn id(&self) -> ProviderId {
        ProviderId("unicode".into())
    }
    fn name(&self) -> &str {
        "é文a"
    }
    fn search(&self, _: &SearchQuery, _: &SearchContext) -> anyhow::Result<Vec<Item>> {
        Ok(vec![Item {
            id: ItemId("unicode:1".into()),
            provider: self.id(),
            title: "Unicode result".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: json!({"private":"hidden"}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![Action {
            id: ActionId("select".into()),
            title: "Select".into(),
        }]
    }
    fn activate(&self, _: &Item, _: &Action) -> anyhow::Result<ActionOutcome> {
        Ok(ActionOutcome::Close)
    }
}
impl ServiceProvider for UnicodeProvider {
    fn description(&self) -> &str {
        "unicode"
    }
    fn kind(&self) -> &str {
        "example"
    }
    fn presentation(&self, _: &Item) -> ItemPresentation {
        ItemPresentation {
            kind: "example".into(),
            icon: None,
            content: vec![ContentBlock::Text {
                text: "safe".into(),
                truncated: false,
                fallback_text: "safe".into(),
            }],
            data: None,
        }
    }
}

#[test]
fn unmatched_multibyte_menu_queries_do_not_panic() {
    let service = Service::new(
        vec![(
            Arc::new(UnicodeProvider),
            ProviderConfig {
                default_search: false,
                ..ProviderConfig::default()
            },
        )],
        Arc::new(HistoryStore::in_memory().unwrap()),
    )
    .unwrap();
    let mut session = session(&service);
    let view = call(&mut session, "views.open", json!({"query":"é文b"}));
    assert!(view["sections"].as_array().unwrap().is_empty());
}
