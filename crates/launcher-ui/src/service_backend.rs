use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result};
use launcher_agent::connection::{IconPixels, ServiceConnection};
use launcher_core::{Action, ActionId, IconDescriptor, Item, ItemId, ProviderId};
use serde_json::Value;

/// A service action that carried a confirmation prompt and was staged instead
/// of sent. It stays bound to the search snapshot that authorized it, so a
/// later snapshot change cannot silently upgrade stale consent into an invoke.
#[derive(Clone, Debug)]
pub(crate) struct StagedAction {
    pub search_request: u64,
    pub provider: String,
    pub id: String,
    pub action: Action,
    pub prompt: String,
}

/// UI-side state for the opt-in service transport. It owns no embedded providers.
pub(crate) struct ServiceBackend {
    connection: ServiceConnection,
    next_request: u64,
    pub search_request: Option<u64>,
    pub provider_scope: bool,
    pub authorized_actions: HashMap<(String, String), Vec<Action>>,
    /// Confirmation prompts advertised by the current snapshot, keyed by
    /// `(provider, item id, action id)`. Provider-owned; never client text.
    pub action_confirmations: HashMap<(String, String, String), String>,
    /// A staged, not-yet-sent action awaiting explicit user consent.
    pub pending_confirmation: Option<StagedAction>,
    pub icon_sources: HashMap<(String, String), String>,
    pub details_available: HashMap<(String, String), bool>,
    pub details_request: u64,
    pub has_details: bool,
    pub preview: Option<crate::service_preview::ServicePreview>,
    pub detail_selection: Option<(u64, u64, String, String)>,
    pub pending_action: Option<u64>,
}

impl ServiceBackend {
    pub fn connect(path: &Path) -> Result<Self> {
        Ok(Self {
            connection: ServiceConnection::connect_frontend(
                path,
                launcher_protocol::FrontendKind::Gpui,
            )
            .context("starting service client")?,
            next_request: 1,
            search_request: None,
            provider_scope: false,
            authorized_actions: HashMap::new(),
            action_confirmations: HashMap::new(),
            pending_confirmation: None,
            icon_sources: HashMap::new(),
            details_available: HashMap::new(),
            details_request: 0,
            has_details: false,
            preview: None,
            detail_selection: None,
            pending_action: None,
        })
    }

    fn request(&mut self) -> u64 {
        let request = self.next_request;
        self.next_request = self.next_request.wrapping_add(1).max(1);
        request
    }

    pub fn search(&mut self, query: String) -> (u64, Result<(), String>) {
        let request = self.request();
        // Retain display-only icons and preview while replacements load. They
        // grant no authority: revoke the old snapshot and detail identity now.
        self.detail_selection = None;
        self.details_available.clear();
        self.search_request = None;
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        self.pending_confirmation = None;
        (request, self.connection.search(request, query))
    }

    pub fn home(&mut self) -> (u64, Result<(), String>) {
        let request = self.request();
        self.invalidate_details();
        self.details_available.clear();
        self.search_request = None;
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        self.pending_confirmation = None;
        self.provider_scope = false;
        (request, self.connection.home(request))
    }

    pub fn details(
        &mut self,
        search_request: u64,
        provider: &str,
        id: &str,
    ) -> Result<u64, String> {
        if !self.has_details
            || self.search_request != Some(search_request)
            || !self
                .details_available
                .get(&(provider.to_owned(), id.to_owned()))
                .copied()
                .unwrap_or(false)
        {
            self.detail_selection = None;
            return Err("details are not available in the current service snapshot".into());
        }
        self.details_request = self.details_request.wrapping_add(1).max(1);
        let request = self.details_request;
        self.detail_selection = Some((request, search_request, provider.to_owned(), id.to_owned()));
        self.connection
            .details(request, search_request, provider.to_owned(), id.to_owned())?;
        Ok(request)
    }

    pub fn invalidate_details(&mut self) {
        self.detail_selection = None;
        self.preview = None;
    }

    pub fn refresh(&mut self, old_search_request: u64) -> (u64, Result<(), String>) {
        let request = self.request();
        self.invalidate_details();
        self.search_request = None;
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        self.pending_confirmation = None;
        self.icon_sources.clear();
        self.details_available.clear();
        (
            request,
            self.connection.refresh(request, old_search_request),
        )
    }

    pub fn apply_details(&mut self, event: &Value) -> Option<(u64, String, String, Vec<Value>)> {
        let request = event.get("request")?.as_u64()?;
        let detail_request = event.get("detailRequest")?.as_u64()?;
        let provider = event.get("provider")?.as_str()?.to_owned();
        let id = event.get("id")?.as_str()?.to_owned();
        if self.detail_selection.as_ref()
            != Some(&(detail_request, request, provider.clone(), id.clone()))
            || self.search_request != Some(request)
        {
            return None;
        }
        let content = event.get("content")?.as_array()?.clone();
        Some((detail_request, provider, id, content))
    }

    pub fn cancel(&mut self) -> Result<(), String> {
        self.search_request = None;
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        self.pending_confirmation = None;
        self.icon_sources.clear();
        self.details_available.clear();
        self.detail_selection = None;
        let result = self.connection.cancel();
        // The actor advances its request watermark before attempting enqueue,
        // including when the bounded command queue rejects this cancellation.
        self.next_request = self.next_request.wrapping_add(1).max(1);
        result
    }

    /// Stages a confirmation or sends the action immediately.
    ///
    /// Returns `Ok(Some(request))` when the action was sent, or `Ok(None)` when
    /// the provider published a confirmation prompt and the action was staged
    /// for explicit consent through [`ServiceBackend::confirm`].
    pub fn activate(&mut self, item: &Item, action: &Action) -> Result<Option<u64>, String> {
        if self.pending_action.is_some() {
            return Err("an action is already running".into());
        }
        let search_request = self.search_request.ok_or("no current service results")?;
        let permitted = self
            .authorized_actions
            .get(&(item.provider.0.clone(), item.id.0.clone()))
            .is_some_and(|actions| actions.iter().any(|candidate| candidate.id == action.id));
        if !permitted {
            return Err("action is not present in the current service snapshot".into());
        }
        if let Some(prompt) = self
            .action_confirmations
            .get(&(
                item.provider.0.clone(),
                item.id.0.clone(),
                action.id.0.clone(),
            ))
            .cloned()
        {
            self.pending_confirmation = Some(StagedAction {
                search_request,
                provider: item.provider.0.clone(),
                id: item.id.0.clone(),
                action: action.clone(),
                prompt,
            });
            return Ok(None);
        }
        self.send_activation(
            item.provider.0.clone(),
            item.id.0.clone(),
            action.id.0.clone(),
            search_request,
            false,
        )
        .map(Some)
    }

    /// Sends the staged action with explicit consent. The staged action is
    /// consumed even if sending fails, so consent cannot be replayed.
    pub fn confirm(&mut self) -> Result<u64, String> {
        let staged = self
            .pending_confirmation
            .take()
            .ok_or("there is no action to confirm")?;
        if self.pending_action.is_some() {
            return Err("an action is already running".into());
        }
        if self.search_request != Some(staged.search_request) {
            return Err("the results changed; select the action again".into());
        }
        let permitted = self
            .authorized_actions
            .get(&(staged.provider.clone(), staged.id.clone()))
            .is_some_and(|actions| {
                actions
                    .iter()
                    .any(|candidate| candidate.id == staged.action.id)
            });
        if !permitted {
            return Err("action is not present in the current service snapshot".into());
        }
        self.send_activation(
            staged.provider,
            staged.id,
            staged.action.id.0,
            staged.search_request,
            true,
        )
    }

    /// Dismisses a staged action without sending anything.
    pub fn cancel_confirmation(&mut self) {
        self.pending_confirmation = None;
    }

    fn send_activation(
        &mut self,
        provider: String,
        id: String,
        action: String,
        search_request: u64,
        confirmed: bool,
    ) -> Result<u64, String> {
        let request = self.request();
        self.invalidate_details();
        if confirmed {
            self.connection
                .activate_confirmed(request, search_request, provider, id, action)?;
        } else {
            self.connection
                .activate(request, search_request, provider, id, action)?;
        }
        self.pending_action = Some(request);
        Ok(request)
    }

    pub fn finish_action(&mut self, request: u64) -> bool {
        if self.pending_action == Some(request) {
            self.pending_action = None;
            true
        } else {
            false
        }
    }

    pub fn try_recv(&self) -> Option<Value> {
        self.connection.try_recv()
    }

    pub fn icon(&self, token: &str) -> Option<IconPixels> {
        self.connection.icon(token)
    }

    pub fn record_icon_source(&mut self, provider: &str, id: &str, token: &str) {
        if !provider.is_empty() && !id.is_empty() && !token.is_empty() {
            self.icon_sources.insert(
                (provider.to_owned(), id.to_owned()),
                format!("image://maccer-icons/{token}"),
            );
        }
    }

    pub fn apply_results(&mut self, event: &Value) -> Option<(u64, Vec<Item>)> {
        let request = event.get("request")?.as_u64()?;
        let rows = event.get("items")?.as_array()?;
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        // A delivered view is a new snapshot, so stale consent cannot survive.
        self.pending_confirmation = None;
        self.icon_sources.clear();
        self.details_available.clear();
        self.detail_selection = None;
        self.preview = None;
        // A delivered newer view proves the actor has finished any earlier
        // single-flight mutation, including one whose stale outcome was hidden.
        self.pending_action = None;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let id = row.get("id")?.as_str()?.to_owned();
            let provider = row.get("provider")?.as_str()?.to_owned();
            let mut actions: Vec<Action> = Vec::new();
            for value in row
                .get("actions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(action_id) = value.get("id").and_then(Value::as_str) else {
                    continue;
                };
                let Some(title) = value.get("title").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(prompt) = value.get("confirmation").and_then(Value::as_str) {
                    self.action_confirmations.insert(
                        (provider.clone(), id.clone(), action_id.to_owned()),
                        prompt.to_owned(),
                    );
                }
                actions.push(Action {
                    id: ActionId(action_id.to_owned()),
                    title: title.to_owned(),
                });
            }
            self.authorized_actions
                .insert((provider.clone(), id.clone()), actions);
            self.details_available.insert(
                (provider.clone(), id.clone()),
                row.get("detailsAvailable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            );
            let title = row.get("title")?.as_str()?.to_owned();
            let subtitle = row
                .get("subtitle")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let fallback = row
                .get("iconText")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| title.chars().next().unwrap_or('•').to_string());
            if let Some(source) = row
                .get("iconSource")
                .and_then(Value::as_str)
                .filter(|source| source.starts_with("image://maccer-icons/"))
            {
                self.icon_sources
                    .insert((provider.clone(), id.clone()), source.to_owned());
            }
            let icon = Some(IconDescriptor::Text(fallback));
            items.push(Item {
                id: ItemId(id),
                provider: ProviderId(provider),
                title,
                subtitle,
                keywords: Vec::new(),
                icon,
                score: 0.0,
                payload: Value::Null,
            });
        }
        self.search_request = Some(request);
        self.provider_scope = event
            .get("route")
            .and_then(|route| route.get("type"))
            .and_then(Value::as_str)
            == Some("provider");
        Some((request, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn service_rows_keep_only_safe_display_fields_and_snapshot_actions() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir().join(format!("missing-gpui-service-{}.sock", std::process::id())),
        )
        .unwrap();
        let event = json!({
            "type":"results", "request":7,
            "route":{"type":"provider","providerId":"apps"},
            "items":[{"id":"app-1","provider":"apps","title":"Finder","subtitle":"Application","iconText":"F","iconSource":"", "actions":[{"id":"open","title":"Open"}], "payload":{"path":"/private/path"}}]
        });
        let (request, items) = backend.apply_results(&event).unwrap();
        assert_eq!(request, 7);
        assert!(backend.provider_scope);
        assert_eq!(items[0].payload, Value::Null);
        assert!(items[0].keywords.is_empty());
        assert!(matches!(items[0].icon, Some(IconDescriptor::Text(_))));
        assert_eq!(
            backend.authorized_actions[&(String::from("apps"), String::from("app-1"))][0]
                .id
                .0,
            "open"
        );
    }

    #[test]
    fn home_and_new_search_clear_saved_action_authority() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir().join(format!("missing-gpui-home-{}.sock", std::process::id())),
        )
        .unwrap();
        backend.apply_results(
            &json!({"request":1,"route":{"type":"provider","providerId":"apps"},"items":[]}),
        );
        assert!(backend.provider_scope);
        assert!(backend.home().1.is_ok());
        assert!(!backend.provider_scope);
        assert!(backend.search_request.is_none());
        assert!(backend.authorized_actions.is_empty());
    }

    #[test]
    fn a_new_delivered_view_releases_a_suppressed_action_and_keeps_icon_fallback() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir().join(format!("missing-gpui-action-{}.sock", std::process::id())),
        )
        .unwrap();
        let (request, items) = backend
            .apply_results(&json!({
                "request":1,
                "route":{"type":"provider","providerId":"apps"},
                "items":[{"id":"one","provider":"apps","title":"Finder","iconText":"F","iconSource":"image://maccer-icons/token","actions":[{"id":"open","title":"Open"}]}]
            }))
            .unwrap();
        assert_eq!(request, 1);
        assert_eq!(
            items[0].icon.as_ref().and_then(|icon| match icon {
                IconDescriptor::Text(text) => Some(text.as_str()),
                _ => None,
            }),
            Some("F")
        );
        assert_eq!(
            backend
                .icon_sources
                .get(&("apps".into(), "one".into()))
                .map(String::as_str),
            Some("image://maccer-icons/token")
        );
        backend.pending_action = Some(9);
        backend.apply_results(&json!({"request":2,"route":{"type":"home"},"items":[]}));
        assert_eq!(backend.pending_action, None);
    }

    fn confirmable_event(request: u64) -> Value {
        json!({
            "request":request,
            "route":{"type":"provider","providerId":"confirm"},
            "items":[{"id":"one","provider":"confirm","title":"Apply thing",
                "actions":[{"id":"apply","title":"Apply","confirmation":"Apply thing? It changes state."}]}]
        })
    }

    #[test]
    fn confirmation_is_staged_then_sent_once_with_consent() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir().join(format!("missing-gpui-confirm-{}.sock", std::process::id())),
        )
        .unwrap();
        let (_, items) = backend.apply_results(&confirmable_event(7)).unwrap();
        let item = items.into_iter().next().unwrap();
        let action = backend.authorized_actions[&("confirm".into(), "one".into())][0].clone();
        assert_eq!(
            backend.action_confirmations[&("confirm".into(), "one".into(), "apply".into())],
            "Apply thing? It changes state."
        );
        // The first selection stages the prompt and sends nothing.
        assert_eq!(backend.activate(&item, &action).unwrap(), None);
        let staged = backend.pending_confirmation.clone().unwrap();
        assert_eq!(staged.search_request, 7);
        assert_eq!(staged.prompt, "Apply thing? It changes state.");
        assert!(backend.pending_action.is_none());
        // Confirming sends exactly one invoke and consumes the staged consent.
        let request = backend.confirm().unwrap();
        assert_eq!(backend.pending_action, Some(request));
        assert!(backend.pending_confirmation.is_none());
        assert!(backend.confirm().is_err(), "consent cannot be replayed");
    }

    #[test]
    fn a_new_snapshot_clears_staged_consent_without_sending() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir().join(format!("missing-gpui-stale-{}.sock", std::process::id())),
        )
        .unwrap();
        let (_, items) = backend.apply_results(&confirmable_event(7)).unwrap();
        let item = items.into_iter().next().unwrap();
        let action = backend.authorized_actions[&("confirm".into(), "one".into())][0].clone();
        assert_eq!(backend.activate(&item, &action).unwrap(), None);
        assert!(backend.pending_confirmation.is_some());
        backend.apply_results(&confirmable_event(8)).unwrap();
        assert!(backend.pending_confirmation.is_none());
        assert!(backend.pending_action.is_none());
        assert!(backend.confirm().is_err());
    }

    #[test]
    fn actions_without_a_prompt_send_immediately() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir()
                .join(format!("missing-gpui-noprompt-{}.sock", std::process::id())),
        )
        .unwrap();
        let (_, items) = backend
            .apply_results(&json!({
                "request":1,"route":{"type":"provider","providerId":"apps"},
                "items":[{"id":"one","provider":"apps","title":"Finder","actions":[{"id":"open","title":"Open"}]}]
            }))
            .unwrap();
        let item = items.into_iter().next().unwrap();
        let action = backend.authorized_actions[&("apps".into(), "one".into())][0].clone();
        assert_eq!(backend.activate(&item, &action).unwrap(), Some(1));
        assert!(backend.pending_confirmation.is_none());
    }

    #[test]
    fn cold_icon_event_registers_renderer_source_without_replacing_fallback() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir().join(format!("missing-gpui-icon-{}.sock", std::process::id())),
        )
        .unwrap();
        let (_, items) = backend.apply_results(&json!({
            "request":1,
            "route":{"type":"provider","providerId":"apps"},
            "items":[{"id":"one","provider":"apps","title":"Finder","iconText":"F","actions":[]}]
        })).unwrap();
        backend.record_icon_source("apps", "one", "opaque-token");
        assert_eq!(
            backend
                .icon_sources
                .get(&("apps".into(), "one".into()))
                .map(String::as_str),
            Some("image://maccer-icons/opaque-token")
        );
        assert!(matches!(items[0].icon, Some(IconDescriptor::Text(ref text)) if text == "F"));
    }
}
