use crate::{Service, menu};
use anyhow::Result;
use launcher_core::{
    Action, ActionId, ActionOutcome, CancellationToken, Item, ProviderId, SearchCoordinator,
};
use launcher_protocol::*;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Clone)]
struct ActionRef {
    view_id: String,
    revision: String,
    item: Item,
    action: Action,
    provider_revision: Option<u64>,
    navigate: Option<Route>,
    requires_confirmation: bool,
}
#[derive(Clone)]
struct DetailsRef {
    item: Item,
    provider_id: ProviderId,
    provider_revision: u64,
}
struct ActiveView {
    id: String,
    revision: u64,
    route: Route,
    query: String,
    limit: usize,
}
pub struct Session {
    service: Arc<Service>,
    id: String,
    hello: bool,
    view: Option<ActiveView>,
    coordinator: SearchCoordinator,
    actions: HashMap<String, ActionRef>,
    details: HashMap<(String, String), DetailsRef>,
    asset_grants: HashMap<(String, String), (ProviderId, u64)>,
    operations: HashMap<String, (InvokeActionParams, Result<OperationResult, RpcError>)>,
    next_view: u64,
    next_action: u64,
    cancellation: Option<CancellationToken>,
}
impl Session {
    pub(crate) fn new(service: Arc<Service>, id: String) -> Self {
        let coordinator = service.coordinator();
        Self {
            service,
            id,
            hello: false,
            view: None,
            coordinator,
            actions: HashMap::new(),
            details: HashMap::new(),
            asset_grants: HashMap::new(),
            operations: HashMap::new(),
            next_view: 0,
            next_action: 0,
            cancellation: None,
        }
    }
    pub fn handle(&mut self, request: RpcRequest) -> RpcResponse {
        self.handle_transport(request).0
    }
    pub(crate) fn handle_transport(
        &mut self,
        request: RpcRequest,
    ) -> (RpcResponse, Option<Vec<u8>>) {
        let id = request.id.clone();
        if request.method == "assets.get"
            && request.jsonrpc == "2.0"
            && !request.id.is_empty()
            && request.id.len() <= 128
            && !request.id.contains('\0')
        {
            return self.asset_response(request);
        }
        let result = self.dispatch(request);
        let response = match result {
            Ok(value) => RpcResponse::success(id, value),
            Err(error) => RpcResponse::failure(id, error),
        };
        (response, None)
    }
    fn asset_response(&mut self, request: RpcRequest) -> (RpcResponse, Option<Vec<u8>>) {
        let id = request.id.clone();
        let result = (|| {
            if !self.hello {
                return Err(err(
                    -32001,
                    "HELLO_REQUIRED",
                    "hello must be the first request",
                ));
            }
            let params: GetAssetParams = parse(request.params)?;
            if params.representation != "bgra8" {
                return Err(err(
                    -32602,
                    "UNSUPPORTED_REPRESENTATION",
                    "only bgra8 is supported",
                ));
            }
            if self.view.is_none() {
                return Err(err(-32004, "VIEW_NOT_FOUND", "no active view"));
            }
            let (provider_id, provider_revision) = self
                .asset_grants
                .get(&(params.asset_id.clone(), params.revision.clone()))
                .ok_or_else(|| {
                    err(
                        -32009,
                        "ASSET_NOT_AUTHORIZED",
                        "asset was not advertised in the current view",
                    )
                })?;
            let provider = self
                .service
                .registry
                .get(provider_id)
                .ok_or_else(|| err(-32009, "ASSET_EXPIRED", "asset provider is unavailable"))?;
            if provider.revision() != *provider_revision {
                return Err(err(
                    -32009,
                    "ASSET_EXPIRED",
                    "provider data changed after the asset was advertised",
                ));
            }
            let store =
                self.service.assets.as_ref().ok_or_else(|| {
                    err(-32601, "METHOD_NOT_FOUND", "asset transfer is unavailable")
                })?;
            let (metadata, bytes) = store.get(&params.asset_id, &params.revision).map_err(|e| {
                let text = e.to_string();
                if text == "ASSET_EXPIRED" {
                    err(-32009, "ASSET_EXPIRED", "asset reference expired")
                } else if text == "ASSET_UNAVAILABLE" {
                    err(-32004, "ASSET_UNAVAILABLE", "asset could not be resolved")
                } else {
                    err(-32603, "ASSET_FAILED", "asset resolution failed")
                }
            })?;
            Ok((serde_json::to_value(metadata).map_err(internal)?, bytes))
        })();
        match result {
            Ok((metadata, bytes)) => (RpcResponse::success(id, metadata), Some(bytes)),
            Err(error) => (RpcResponse::failure(id, error), None),
        }
    }
    fn dispatch(&mut self, request: RpcRequest) -> Result<Value, RpcError> {
        if request.jsonrpc != "2.0"
            || request.id.is_empty()
            || request.id.len() > 128
            || request.id.contains('\0')
        {
            return Err(err(-32600, "INVALID_REQUEST", "invalid request envelope"));
        }
        if !self.hello && request.method != "hello" {
            return Err(err(
                -32001,
                "HELLO_REQUIRED",
                "hello must be the first request",
            ));
        }
        match request.method.as_str() {
            "hello" => {
                let p: HelloParams = parse(request.params)?;
                if self.hello {
                    return Err(err(
                        -32600,
                        "ALREADY_INITIALIZED",
                        "hello already completed",
                    ));
                }
                let mut h = crate::hello(&self.service.instance_id, &self.id, p)?;
                if self.service.assets.is_some() {
                    h.capabilities.push("iconAssetsBgra8".into());
                }
                if self.service.theme.is_some() {
                    h.capabilities.push("themeState".into());
                }
                if self.service.frontend.get().is_some() {
                    h.capabilities.push("frontendControl".into());
                }
                self.hello = true;
                serde_json::to_value(h).map_err(internal)
            }
            "providers.list" => {
                empty_params(&request.params)?;
                serde_json::to_value(self.service.providers()).map_err(internal)
            }
            "frontend.poll" => {
                let router = self.service.frontend.get().ok_or_else(|| {
                    err(
                        -32601,
                        "METHOD_NOT_FOUND",
                        "frontend control is not configured",
                    )
                })?;
                let params: FrontendPollParams = parse(request.params)?;
                serde_json::to_value(router.poll(&self.id, params)?).map_err(internal)
            }
            "views.open" => {
                let p: OpenViewParams = parse(request.params)?;
                self.validate_query(&p.query)?;
                if p.limit == 0 || p.limit > MAX_RESULTS {
                    return Err(err(
                        -32602,
                        "INVALID_LIMIT",
                        "limit must be between 1 and 100",
                    ));
                }
                if let Route::Provider { provider_id } = &p.route
                    && !self
                        .service
                        .providers
                        .iter()
                        .any(|r| r.config.enabled && r.provider.id().0 == *provider_id)
                {
                    return Err(err(-32004, "PROVIDER_NOT_FOUND", "provider is unavailable"));
                }
                self.next_view += 1;
                self.actions.clear();
                self.asset_grants.clear();
                self.details.clear();
                self.view = Some(ActiveView {
                    id: format!("view-{}", self.next_view),
                    revision: 0,
                    route: p.route,
                    query: p.query,
                    limit: p.limit,
                });
                let snapshot = self.refresh()?;
                serde_json::to_value(snapshot).map_err(internal)
            }
            "views.query" => {
                let p: QueryViewParams = parse(request.params)?;
                self.validate_query(&p.query)?;
                let v = self
                    .view
                    .as_mut()
                    .filter(|v| v.id == p.view_id)
                    .ok_or_else(|| {
                        err(
                            -32004,
                            "VIEW_NOT_FOUND",
                            "view is not active in this session",
                        )
                    })?;
                v.query = p.query;
                let snapshot = self.refresh()?;
                serde_json::to_value(snapshot).map_err(internal)
            }
            "views.close" => {
                let p: CloseViewParams = parse(request.params)?;
                if self.view.as_ref().is_none_or(|v| v.id != p.view_id) {
                    return Err(err(
                        -32004,
                        "VIEW_NOT_FOUND",
                        "view is not active in this session",
                    ));
                }
                if let Some(t) = self.cancellation.take() {
                    t.cancel()
                }
                self.view = None;
                self.actions.clear();
                self.details.clear();
                self.asset_grants.clear();
                Ok(Value::Null)
            }
            "actions.invoke" => {
                let p: InvokeActionParams = parse(request.params)?;
                serde_json::to_value(self.invoke(p)?).map_err(internal)
            }
            "views.details" => {
                let p: GetDetailsParams = parse(request.params)?;
                serde_json::to_value(self.get_details(p)?).map_err(internal)
            }
            "state.theme" => {
                empty_params(&request.params)?;
                let theme =
                    self.service.theme.as_ref().ok_or_else(|| {
                        err(-32601, "METHOD_NOT_FOUND", "theme state is unavailable")
                    })?;
                serde_json::to_value(theme.snapshot()).map_err(internal)
            }
            _ => Err(err(-32601, "METHOD_NOT_FOUND", "method is not available")),
        }
    }
    fn get_details(&mut self, params: GetDetailsParams) -> Result<ItemDetails, RpcError> {
        if self.view.as_ref().is_none_or(|view| {
            view.id != params.view_id || view.revision.to_string() != params.revision
        }) {
            return Err(err(
                -32009,
                "STALE_DETAILS",
                "details do not belong to the current view",
            ));
        }
        let key = (params.provider_id.clone(), params.item_id.clone());
        let details_ref = self.details.get(&key).cloned().ok_or_else(|| {
            err(
                -32009,
                "DETAILS_NOT_AUTHORIZED",
                "item details were not advertised in the current view",
            )
        })?;
        if details_ref.provider_id.0 != params.provider_id {
            return Err(err(
                -32009,
                "DETAILS_NOT_AUTHORIZED",
                "item details provider does not match",
            ));
        }
        let registration = self
            .service
            .providers
            .iter()
            .find(|registration| {
                registration.config.enabled && registration.provider.id() == details_ref.provider_id
            })
            .ok_or_else(|| err(-32009, "STALE_DETAILS", "details provider is unavailable"))?;
        if registration.provider.revision() != details_ref.provider_revision {
            return Err(err(
                -32009,
                "STALE_DETAILS",
                "provider data changed after details were advertised",
            ));
        }
        let content = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registration.provider.details(&details_ref.item)
        }))
        .map_err(|_| err(-32603, "PROVIDER_PANIC", "provider details panicked"))?
        .map_err(|_| err(-32603, "PROVIDER_DETAILS_FAILED", "provider details failed"))?;
        if registration.provider.revision() != details_ref.provider_revision {
            return Err(err(
                -32009,
                "STALE_DETAILS",
                "provider data changed while details were being loaded",
            ));
        }
        if content.len() > 32 {
            return Err(err(
                -32603,
                "DETAILS_TOO_LARGE",
                "provider returned more than 32 detail blocks",
            ));
        }
        let encoded = serde_json::to_vec(&content).map_err(internal)?;
        if encoded.len() > 64 * 1024 {
            return Err(err(
                -32603,
                "DETAILS_TOO_LARGE",
                "provider details exceed 65536 bytes",
            ));
        }
        if self.service.assets.is_some() {
            for block in &content {
                if let ContentBlock::Image { asset, .. } = block {
                    self.asset_grants.insert(
                        (asset.id.clone(), asset.revision.clone()),
                        (
                            details_ref.provider_id.clone(),
                            details_ref.provider_revision,
                        ),
                    );
                }
            }
        }
        Ok(ItemDetails {
            view_id: params.view_id,
            revision: params.revision,
            provider_id: params.provider_id,
            item_id: params.item_id,
            content,
        })
    }
    fn validate_query(&self, q: &str) -> Result<(), RpcError> {
        if q.len() > MAX_QUERY_BYTES || q.contains('\0') {
            Err(err(
                -32602,
                "INVALID_QUERY",
                "query exceeds 8192 bytes or contains NUL",
            ))
        } else {
            Ok(())
        }
    }
    fn refresh(&mut self) -> Result<ViewSnapshot, RpcError> {
        if let Some(t) = self.cancellation.take() {
            t.cancel()
        }
        self.actions.clear();
        self.details.clear();
        self.asset_grants.clear();
        let view = self
            .view
            .as_mut()
            .ok_or_else(|| err(-32004, "VIEW_NOT_FOUND", "no active view"))?;
        view.revision = view.revision.wrapping_add(1);
        let revision = view.revision.to_string();
        let view_id = view.id.clone();
        let route = view.route.clone();
        let query = view.query.clone();
        let limit = view.limit;
        let mut sections = Vec::new();
        let mut errors = Vec::new();
        let mut truncated = false;
        if route == Route::Home {
            let mut menu_items = Vec::new();
            for reg in self.service.providers.iter().filter(|r| r.config.enabled) {
                let id = reg.provider.id().0;
                let name = reg.provider.name().to_owned();
                let description = reg.provider.description().to_owned();
                let kind = reg.provider.kind().to_owned();
                if let Some(score) =
                    menu::score(&query, &format!("{name} {description} {kind} {id}"))
                {
                    let item = menu::menu_item(&id, &name, &description);
                    menu_items.push((score, reg.config.priority, item, kind));
                }
            }
            menu_items.sort_by(|a, b| {
                if query.trim().is_empty() {
                    b.1.cmp(&a.1).then_with(|| a.2.title.cmp(&b.2.title))
                } else {
                    a.0.cmp(&b.0)
                        .then_with(|| b.1.cmp(&a.1))
                        .then_with(|| a.2.title.cmp(&b.2.title))
                }
            });
            truncated = menu_items.len() > limit;
            let selected: Vec<_> = menu_items.into_iter().take(limit).collect();
            let mut items = Vec::new();
            for (_, _, item, kind) in selected {
                let provider_id = item.id.0.clone();
                items.push(self.project_item(
                    item,
                    menu::menu_presentation(&kind),
                    &view_id,
                    &revision,
                    Some(Route::Provider { provider_id }),
                )?)
            }
            if !items.is_empty() {
                sections.push(Section {
                    id: "providers".into(),
                    title: "Providers".into(),
                    layout_hint: Some("list".into()),
                    items,
                })
            }
            if !query.trim().is_empty() {
                let remaining = limit.saturating_sub(sections.iter().map(|s| s.items.len()).sum());
                let snapshot =
                    self.search(&view_id, &revision, Route::Home, query.clone(), remaining)?;
                if let Some(s) = snapshot {
                    truncated |= s.truncated;
                    errors.extend(s.errors);
                    sections.extend(s.sections.into_iter().filter(|s| s.id == "results"));
                }
            }
        } else {
            let snapshot = self.search(&view_id, &revision, route.clone(), query.clone(), limit)?;
            if let Some(s) = snapshot {
                truncated = s.truncated;
                errors = s.errors;
                sections = s.sections;
            }
        }
        Ok(ViewSnapshot {
            view_id,
            revision,
            route,
            query,
            sections,
            complete: true,
            truncated,
            errors,
        })
    }
    fn search(
        &mut self,
        view_id: &str,
        revision: &str,
        route: Route,
        query: String,
        limit: usize,
    ) -> Result<Option<ViewSnapshot>, RpcError> {
        let session = match route {
            Route::Home => self.coordinator.start(query.clone()),
            Route::Provider { ref provider_id } => self
                .coordinator
                .start_provider(query.clone(), ProviderId(provider_id.clone())),
        };
        let token = session.cancellation.clone();
        self.cancellation = Some(token.clone());
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut final_update = None;
        while Instant::now() < deadline && !token.is_cancelled() {
            match session.receiver.try_recv() {
                Ok(update) if update.complete => {
                    final_update = Some(update);
                    break;
                }
                Ok(_) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(2))
                }
                Err(async_channel::TryRecvError::Closed) => break,
            }
        }
        let Some(update) = final_update else {
            token.cancel();
            return Err(err(
                -32010,
                "SEARCH_TIMEOUT",
                "provider search did not complete in time",
            ));
        };
        let mut items = Vec::new();
        let truncated = update.items.len() > limit;
        let mut ranked = update.items;
        ranked.truncate(limit);
        for item in ranked {
            let Some(reg) = self
                .service
                .providers
                .iter()
                .find(|r| r.provider.id() == item.provider)
            else {
                continue;
            };
            let presentation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reg.provider.presentation(&item)
            }))
            .map_err(|_| err(-32603, "PROVIDER_PANIC", "provider presentation failed"))?;
            items.push(self.project_item(item, presentation, view_id, revision, None)?);
        }
        let errors = update
            .errors
            .into_iter()
            .map(|e| ProviderError {
                provider_id: e.provider.0,
                code: "PROVIDER_ERROR".into(),
                message: e.message,
            })
            .collect();
        let sections = if items.is_empty() {
            vec![]
        } else {
            vec![Section {
                id: "results".into(),
                title: "Results".into(),
                layout_hint: Some("list".into()),
                items,
            }]
        };
        Ok(Some(ViewSnapshot {
            view_id: view_id.into(),
            revision: revision.into(),
            route,
            query,
            sections,
            complete: update.complete,
            truncated,
            errors,
        }))
    }
    fn project_item(
        &mut self,
        item: Item,
        presentation: menu::ItemPresentation,
        view_id: &str,
        revision: &str,
        navigate: Option<Route>,
    ) -> Result<ResultItem, RpcError> {
        let is_result = navigate.is_none();
        let actions = if let Some(route) = navigate {
            let id = self.issue_action(
                view_id,
                revision,
                item.clone(),
                Action {
                    id: ActionId("open".into()),
                    title: "Open".into(),
                },
                Some(route),
                false,
            );
            vec![ActionDescriptor {
                id: "open".into(),
                action_ref: id,
                title: "Open".into(),
                is_primary: true,
                kind: "navigate".into(),
                confirmation: None,
            }]
        } else {
            let Some(provider) = self.service.registry.get(&item.provider) else {
                return Err(err(
                    -32603,
                    "PROVIDER_MISSING",
                    "result provider disappeared",
                ));
            };
            let source =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| provider.actions(&item)))
                    .map_err(|_| {
                        err(-32603, "PROVIDER_PANIC", "provider action listing panicked")
                    })?;
            let mut actions = Vec::new();
            let service_provider = self
                .service
                .providers
                .iter()
                .find(|r| r.config.enabled && r.provider.id() == item.provider)
                .map(|r| r.provider.clone());
            for action in source {
                let confirmation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    service_provider
                        .as_ref()
                        .and_then(|p| p.action_confirmation(&item, &action))
                }))
                .map_err(|_| {
                    err(
                        -32603,
                        "PROVIDER_PANIC",
                        "provider confirmation lookup panicked",
                    )
                })?;
                let id = self.issue_action(
                    view_id,
                    revision,
                    item.clone(),
                    action.clone(),
                    None,
                    confirmation.is_some(),
                );
                actions.push(ActionDescriptor {
                    id: action.id.0,
                    action_ref: id,
                    title: action.title,
                    is_primary: actions.is_empty(),
                    kind: "invoke".into(),
                    confirmation,
                })
            }
            actions
        };
        let public_id = if item.provider.0 == "menu" {
            item.id.0.clone()
        } else {
            self.service
                .providers
                .iter()
                .find(|r| r.provider.id() == item.provider)
                .map(|r| r.provider.public_id(&item))
                .unwrap_or_else(|| item.id.0.clone())
        };
        let provider_revision = self
            .service
            .registry
            .get(&item.provider)
            .map(|p| p.revision())
            .unwrap_or(0);
        let details_available = if is_result {
            let Some(registration) = self.service.providers.iter().find(|registration| {
                registration.config.enabled && registration.provider.id() == item.provider
            }) else {
                return Err(err(
                    -32603,
                    "PROVIDER_MISSING",
                    "result provider disappeared",
                ));
            };
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registration.provider.details_available(&item)
            }))
            .map_err(|_| {
                err(
                    -32603,
                    "PROVIDER_PANIC",
                    "provider details listing panicked",
                )
            })?
        } else {
            false
        };
        if details_available {
            let public_id = self
                .service
                .providers
                .iter()
                .find(|registration| registration.provider.id() == item.provider)
                .map(|registration| registration.provider.public_id(&item))
                .unwrap_or_else(|| item.id.0.clone());
            self.details.insert(
                (item.provider.0.clone(), public_id),
                DetailsRef {
                    item: item.clone(),
                    provider_id: item.provider.clone(),
                    provider_revision,
                },
            );
        }
        if self.service.assets.is_some() {
            if let Some(Icon::Asset { asset, .. }) = &presentation.icon {
                self.asset_grants.insert(
                    (asset.id.clone(), asset.revision.clone()),
                    (item.provider.clone(), provider_revision),
                );
            }
            for block in &presentation.content {
                if let ContentBlock::Image { asset, .. } = block {
                    self.asset_grants.insert(
                        (asset.id.clone(), asset.revision.clone()),
                        (item.provider.clone(), provider_revision),
                    );
                }
            }
        }
        Ok(ResultItem {
            id: public_id,
            provider_id: item.provider.0,
            revision: revision.into(),
            kind: presentation.kind,
            title: item.title,
            subtitle: item.subtitle,
            icon: presentation.icon,
            content: presentation.content,
            data: presentation.data,
            actions,
            details_available,
        })
    }
    fn issue_action(
        &mut self,
        view_id: &str,
        revision: &str,
        item: Item,
        action: Action,
        navigate: Option<Route>,
        requires_confirmation: bool,
    ) -> String {
        self.next_action += 1;
        let id = format!("{}-a{}", self.id, self.next_action);
        let provider_revision = if navigate.is_none() {
            self.service
                .registry
                .get(&item.provider)
                .map(|provider| provider.revision())
        } else {
            None
        };
        self.actions.insert(
            id.clone(),
            ActionRef {
                view_id: view_id.into(),
                revision: revision.into(),
                item,
                action,
                provider_revision,
                navigate,
                requires_confirmation,
            },
        );
        id
    }
    fn invoke(&mut self, p: InvokeActionParams) -> Result<OperationResult, RpcError> {
        if p.operation_id.is_empty() || p.operation_id.len() > 128 || p.operation_id.contains('\0')
        {
            return Err(err(
                -32602,
                "INVALID_OPERATION_ID",
                "operationId must contain 1 to 128 bytes without NUL",
            ));
        }
        if let Some((old, result)) = self.operations.get(&p.operation_id) {
            if old == &p {
                return result.clone();
            }
            return Err(err(
                -32602,
                "OPERATION_ID_REUSE",
                "operationId was already used for different parameters",
            ));
        }
        if self.operations.len() >= 64 {
            return Err(err(
                -32020,
                "OPERATION_LIMIT",
                "operation history is full for this session",
            ));
        }
        let view = self
            .view
            .as_ref()
            .ok_or_else(|| err(-32004, "VIEW_NOT_FOUND", "view is not active"))?;
        if p.view_id != view.id || p.revision != view.revision.to_string() {
            return Err(err(
                -32009,
                "STALE_ACTION",
                "action does not belong to the current view revision",
            ));
        }
        let reference = self.actions.get(&p.action_ref).cloned().ok_or_else(|| {
            err(
                -32009,
                "STALE_ACTION",
                "action reference is unknown or expired",
            )
        })?;
        if reference.view_id != p.view_id || reference.revision != p.revision {
            return Err(err(
                -32009,
                "STALE_ACTION",
                "action reference belongs to another view",
            ));
        }
        let usage_record = reference.navigate.is_none().then(|| {
            (
                reference.item.provider.clone(),
                reference.item.id.clone(),
                reference.action.id.clone(),
            )
        });
        let result = if let Some(route) = reference.navigate.clone() {
            OperationResult {
                operation_id: p.operation_id.clone(),
                status: "succeeded".into(),
                presentation: Some(Presentation::Navigate { route }),
                content: vec![],
            }
        } else {
            let provider = self
                .service
                .registry
                .get(&reference.item.provider)
                .ok_or_else(|| err(-32004, "PROVIDER_NOT_FOUND", "provider is unavailable"))?;
            if reference
                .provider_revision
                .is_some_and(|revision| provider.revision() != revision)
            {
                return Err(err(
                    -32009,
                    "STALE_ACTION",
                    "provider data changed after this result was published",
                ));
            }
            let actual = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                provider.actions(&reference.item)
            }))
            .map_err(|_| err(-32603, "PROVIDER_PANIC", "provider action listing panicked"))?;
            if !actual.iter().any(|a| a.id == reference.action.id) {
                return Err(err(
                    -32009,
                    "STALE_ACTION",
                    "provider no longer offers this action",
                ));
            }
            let registration = self
                .service
                .providers
                .iter()
                .find(|r| r.provider.id() == reference.item.provider)
                .ok_or_else(|| err(-32004, "PROVIDER_NOT_FOUND", "provider is unavailable"))?;
            let confirmation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registration
                    .provider
                    .action_confirmation(&reference.item, &reference.action)
            }))
            .map_err(|_| {
                err(
                    -32603,
                    "PROVIDER_PANIC",
                    "provider confirmation lookup panicked",
                )
            })?;
            if (reference.requires_confirmation || confirmation.is_some()) && !p.confirmed {
                return Err(err(
                    -32010,
                    "CONFIRMATION_REQUIRED",
                    "this action requires explicit confirmation",
                ));
            }
            let activation = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                provider.activate(&reference.item, &reference.action)
            }));
            let outcome = match activation {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    return self
                        .cache_operation_error(p, err(-32000, "ACTION_FAILED", error.to_string()));
                }
                Err(_) => {
                    return self.cache_operation_error(
                        p,
                        err(-32603, "PROVIDER_PANIC", "provider action panicked"),
                    );
                }
            };
            match outcome {
                ActionOutcome::Close => OperationResult {
                    operation_id: p.operation_id.clone(),
                    status: "succeeded".into(),
                    presentation: Some(Presentation::Close),
                    content: vec![],
                },
                ActionOutcome::KeepOpen(message) => OperationResult {
                    operation_id: p.operation_id.clone(),
                    status: "succeeded".into(),
                    presentation: Some(Presentation::KeepOpen { message }),
                    content: vec![],
                },
                // A provider-local mutation needs a new authoritative snapshot,
                // not another invocation. Reuse SetQuery with the exact current
                // query; clients keep their existing route when no route is sent.
                ActionOutcome::RefreshSearch => OperationResult {
                    operation_id: p.operation_id.clone(),
                    status: "succeeded".into(),
                    presentation: Some(Presentation::SetQuery {
                        query: self
                            .view
                            .as_ref()
                            .map(|view| view.query.clone())
                            .unwrap_or_default(),
                    }),
                    content: vec![],
                },
                ActionOutcome::SetQuery(query) => OperationResult {
                    operation_id: p.operation_id.clone(),
                    status: "succeeded".into(),
                    presentation: Some(Presentation::SetQuery { query }),
                    content: vec![],
                },
                _ => {
                    return self.cache_operation_error(
                        p,
                        err(
                            -32000,
                            "UNSUPPORTED_OUTCOME",
                            "outcome is not supported in this service slice",
                        ),
                    );
                }
            }
        };
        if let Some((provider_id, item_id, action_id)) = usage_record
            && self
                .service
                .registry
                .get(&provider_id)
                .is_some_and(|provider| provider.records_usage())
        {
            let query = self
                .view
                .as_ref()
                .map(|v| v.query.clone())
                .unwrap_or_default();
            let event = launcher_core::UsageEvent {
                query,
                provider: provider_id,
                item: item_id,
                action: action_id,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64,
            };
            if self.service.history.record(&event).is_err() {
                eprintln!("Maccer service: usage history write failed after a successful action");
            }
        }
        self.operations
            .insert(p.operation_id.clone(), (p, Ok(result.clone())));
        Ok(result)
    }
    fn cache_operation_error(
        &mut self,
        p: InvokeActionParams,
        error: RpcError,
    ) -> Result<OperationResult, RpcError> {
        self.operations
            .insert(p.operation_id.clone(), (p, Err(error.clone())));
        Err(error)
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        if let Some(t) = self.cancellation.take() {
            t.cancel()
        }
        if let Some(router) = self.service.frontend.get() {
            router.release(&self.id);
        }
    }
}
fn parse<T: DeserializeOwned>(v: Value) -> Result<T, RpcError> {
    serde_json::from_value(v).map_err(|e| err(-32602, "INVALID_PARAMS", e.to_string()))
}
fn empty_params(v: &Value) -> Result<(), RpcError> {
    if v.as_object().is_some_and(|o| o.is_empty()) {
        Ok(())
    } else {
        Err(err(
            -32602,
            "INVALID_PARAMS",
            "params must be an empty object",
        ))
    }
}
fn err(code: i32, stable: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(code, stable, message)
}
fn internal(e: serde_json::Error) -> RpcError {
    err(-32603, "INTERNAL_ERROR", e.to_string())
}
