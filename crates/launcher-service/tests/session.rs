use launcher_protocol::RpcRequest;
use launcher_service::Service;
use serde_json::{Value, json};

fn call(session: &mut launcher_service::Session, id: &str, method: &str, params: Value) -> Value {
    let response = session.handle(RpcRequest::new(id, method, params));
    assert!(
        response.error.is_none(),
        "{}",
        response.error.map(|e| e.message).unwrap_or_default()
    );
    response.result.unwrap()
}
fn initialized(service: &std::sync::Arc<Service>) -> launcher_service::Session {
    let mut s = service.session();
    let h = call(
        &mut s,
        "1",
        "hello",
        json!({"major":1,"minor":0,"clientName":"test"}),
    );
    assert_eq!(h["major"], 1);
    s
}
#[test]
fn home_menu_is_searchable_and_contains_provider_routes() {
    let service = Service::fixture().unwrap();
    let mut s = initialized(&service);
    let view = call(
        &mut s,
        "2",
        "views.open",
        json!({"route":{"type":"home"},"query":"","limit":20}),
    );
    let providers = &view["sections"][0]["items"];
    for id in [
        "apps",
        "themes",
        "wallpapers",
        "clipboard",
        "calculator",
        "web",
        "files",
        "shell",
    ] {
        assert!(
            providers.as_array().unwrap().iter().any(|i| i["id"] == id),
            "missing {id}"
        )
    }
    let themes = providers
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "themes")
        .unwrap();
    assert_eq!(themes["actions"][0]["kind"], "navigate");
    let params = json!({"viewId":view["viewId"],"revision":view["revision"],"actionRef":themes["actions"][0]["actionRef"],"operationId":"navigate-1"});
    let result = call(&mut s, "3", "actions.invoke", params);
    assert_eq!(result["presentation"]["route"]["providerId"], "themes");
    let scoped = call(
        &mut s,
        "4",
        "views.open",
        json!({"route":{"type":"provider","providerId":"themes"},"query":"","limit":20}),
    );
    assert_eq!(scoped["sections"][0]["items"][0]["kind"], "theme");
    assert_eq!(
        scoped["sections"][0]["items"][0]["data"]["slug"],
        "catppuccin"
    );
    assert!(scoped["sections"][0]["items"][0].get("payload").is_none());
}
#[test]
fn home_query_finds_menu_and_mixed_results_but_not_clipboard_contents() {
    let service = Service::fixture().unwrap();
    let mut s = initialized(&service);
    let view = call(
        &mut s,
        "2",
        "views.open",
        json!({"query":"thmes","limit":20}),
    );
    let menu = &view["sections"][0]["items"];
    assert!(menu.as_array().unwrap().iter().any(|i| i["id"] == "themes"));
    assert!(!view.to_string().contains("Fixture clipboard entry"));
    let view = call(
        &mut s,
        "3",
        "views.query",
        json!({"viewId":view["viewId"],"query":"safari"}),
    );
    assert!(
        view["sections"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["id"] == "results")
    );
    assert!(view.to_string().contains("Safari"));
}
#[test]
fn sessions_are_isolated_and_stale_actions_are_rejected() {
    let service = Service::fixture().unwrap();
    let mut a = initialized(&service);
    let mut b = initialized(&service);
    let va = call(&mut a, "2", "views.open", json!({"query":"","limit":20}));
    let vb = call(&mut b, "2", "views.open", json!({"query":"","limit":20}));
    let item = va["sections"][0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "themes")
        .unwrap();
    let params = json!({"viewId":vb["viewId"],"revision":vb["revision"],"actionRef":item["actions"][0]["actionRef"],"operationId":"forged"});
    let bad = b.handle(RpcRequest::new("3", "actions.invoke", params));
    assert_eq!(bad.error.unwrap().data.code, "STALE_ACTION");
    let old = json!({"viewId":va["viewId"],"revision":va["revision"],"actionRef":item["actions"][0]["actionRef"],"operationId":"old"});
    call(
        &mut a,
        "4",
        "views.query",
        json!({"viewId":va["viewId"],"query":"apps"}),
    );
    let bad = a.handle(RpcRequest::new("5", "actions.invoke", old));
    assert_eq!(bad.error.unwrap().data.code, "STALE_ACTION");
}
#[test]
fn protocol_validates_first_hello_and_limits() {
    let service = Service::fixture().unwrap();
    let mut s = service.session();
    let bad = s.handle(RpcRequest::new("1", "providers.list", json!({})));
    assert_eq!(bad.error.unwrap().data.code, "HELLO_REQUIRED");
    let bad = s.handle(RpcRequest::new(
        "2",
        "hello",
        json!({"major":2,"clientName":"test"}),
    ));
    assert_eq!(bad.error.unwrap().data.code, "UNSUPPORTED_VERSION");
    let _ = call(&mut s, "3", "hello", json!({"major":1,"clientName":"test"}));
    let bad = s.handle(RpcRequest::new(
        "4",
        "views.open",
        json!({"query":"x","limit":0}),
    ));
    assert_eq!(bad.error.unwrap().data.code, "INVALID_LIMIT");
    let bad = s.handle(RpcRequest::new(
        "5",
        "views.open",
        json!({"query":"x\u{0000}y","limit":10}),
    ));
    assert_eq!(bad.error.unwrap().data.code, "INVALID_QUERY");
}
#[test]
fn unknown_provider_route_is_rejected() {
    let service = Service::fixture().unwrap();
    let mut s = initialized(&service);
    let bad = s.handle(RpcRequest::new(
        "2",
        "views.open",
        json!({"route":{"type":"provider","providerId":"missing"},"limit":10}),
    ));
    assert_eq!(bad.error.unwrap().data.code, "PROVIDER_NOT_FOUND");
}
#[test]
fn invocation_is_idempotent_but_operation_ids_cannot_be_reused() {
    let service = Service::fixture().unwrap();
    let mut s = initialized(&service);
    let view = call(&mut s, "2", "views.open", json!({"query":"","limit":20}));
    let item = view["sections"][0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "themes")
        .unwrap();
    let params = json!({"viewId":view["viewId"],"revision":view["revision"],"actionRef":item["actions"][0]["actionRef"],"operationId":"same"});
    let first = call(&mut s, "3", "actions.invoke", params.clone());
    let retry = call(&mut s, "4", "actions.invoke", params.clone());
    assert_eq!(first, retry);
    let apps = view["sections"][0]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "apps")
        .unwrap();
    let changed = json!({"viewId":view["viewId"],"revision":view["revision"],"actionRef":apps["actions"][0]["actionRef"],"operationId":"same"});
    let bad = s.handle(RpcRequest::new("5", "actions.invoke", changed));
    assert_eq!(bad.error.unwrap().data.code, "OPERATION_ID_REUSE");
}
