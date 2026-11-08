use std::{collections::HashMap, path::Path};

use crate::agent::connection::{IconPixels, ServiceConnection};
use crate::core::{Action, ActionId, IconDescriptor, Item, ItemId, ProviderId};
use crate::ui::theme::parse_color;
use anyhow::{Context, Result};
use gpui::Hsla;
use serde_json::Value;

/// How a snapshot's body should be laid out. Providers publish this hint; an
/// unknown value stays an ordinary list so an older service is unaffected.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum ServiceLayout {
    #[default]
    List,
    Filmstrip,
}
impl ServiceLayout {
    fn from_wire(hint: &str) -> Self {
        if hint == "filmstrip" {
            Self::Filmstrip
        } else {
            Self::List
        }
    }
}

/// One theme as a palette card: the theme's own surface, its text, its accent
/// and the six accents a strip shows as dots. Parsed here so the renderer never
/// touches provider strings.
#[derive(Clone, Debug)]
pub(crate) struct ThemeCardView {
    pub background: Hsla,
    pub foreground: Hsla,
    pub accent: Hsla,
    pub swatches: Vec<Hsla>,
    /// The theme the desktop is wearing.
    pub applied: bool,
}

/// One provider-published field the frontend must collect before it invokes an
/// action. Every word comes from the provider; the frontend only knows the two
/// shapes it can draw.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServiceActionInput {
    pub id: String,
    /// `text` or `select`. Anything else is not rendered.
    pub kind: String,
    pub title: String,
    pub label: String,
    pub placeholder: Option<String>,
    pub submit_label: String,
    pub options: Vec<ServiceActionInputOption>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ServiceActionInputOption {
    pub value: String,
    pub label: String,
}

impl ServiceActionInput {
    pub fn is_select(&self) -> bool {
        self.kind == crate::protocol::ACTION_INPUT_SELECT
    }
}

/// Reads one provider field out of a row's action record. A spec that does not
/// validate is dropped rather than drawn, so a broken dialog can never appear.
fn action_input_view(value: &Value) -> Option<ServiceActionInput> {
    let id = value.get("id")?.as_str()?.to_owned();
    let kind = value.get("kind")?.as_str()?.to_owned();
    let title = value.get("title")?.as_str()?.to_owned();
    let label = value.get("label")?.as_str()?.to_owned();
    let submit_label = value.get("submitLabel")?.as_str()?.to_owned();
    let placeholder = value
        .get("placeholder")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let options: Vec<ServiceActionInputOption> = value
        .get("options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|option| {
            Some(ServiceActionInputOption {
                value: option.get("value")?.as_str()?.to_owned(),
                label: option.get("label")?.as_str()?.to_owned(),
            })
        })
        .take(crate::protocol::MAX_ACTION_INPUT_OPTIONS)
        .collect();
    let input = ServiceActionInput {
        id,
        kind,
        title,
        label,
        placeholder,
        submit_label,
        options,
    };
    let wire = crate::protocol::ActionInput {
        id: input.id.clone(),
        kind: input.kind.clone(),
        title: input.title.clone(),
        label: input.label.clone(),
        placeholder: input.placeholder.clone(),
        submit_label: input.submit_label.clone(),
        options: input
            .options
            .iter()
            .map(|option| crate::protocol::ActionInputOption {
                value: option.value.clone(),
                label: option.label.clone(),
            })
            .collect(),
    };
    wire.validate().ok()?;
    Some(input)
}

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
    /// Values collected for the action's published field, sent with consent.
    pub input: std::collections::BTreeMap<String, String>,
}

/// A command the provider published for its whole scope rather than for one
/// row. The frontend lists it in the action panel, and it invokes exactly like
/// a row action because the backend registered it as one.
#[derive(Clone, Debug)]
pub(crate) struct ServiceScopeAction {
    pub item: Item,
    pub action: Action,
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
    /// Fields an action needs collected before it can run, keyed the same way.
    pub action_inputs: HashMap<(String, String, String), ServiceActionInput>,
    /// Commands that belong to the scope rather than to a row, in the order the
    /// provider published them. They are never results.
    pub scope_actions: Vec<ServiceScopeAction>,
    /// A staged, not-yet-sent action awaiting explicit user consent.
    pub pending_confirmation: Option<StagedAction>,
    pub icon_sources: HashMap<(String, String), String>,
    /// Wallpaper thumbnails the snapshot advertised, keyed by item. They carry
    /// their own `image://maccer-previews/` source and never mix with icons.
    pub preview_sources: HashMap<(String, String), String>,
    /// Palette cards the snapshot advertised, keyed by item. A strip with one
    /// for every row is drawn as theme cards instead of wallpaper images.
    pub theme_cards: HashMap<(String, String), ThemeCardView>,
    /// Body shape the provider published for this snapshot.
    pub layout: ServiceLayout,
    pub details_available: HashMap<(String, String), bool>,
    pub details_request: u64,
    pub has_details: bool,
    pub preview: Option<crate::ui::service_preview::ServicePreview>,
    pub detail_selection: Option<(u64, u64, String, String)>,
    pub pending_action: Option<u64>,
}

impl ServiceBackend {
    pub fn connect(path: &Path) -> Result<Self> {
        Ok(Self {
            connection: ServiceConnection::connect_frontend(
                path,
                crate::protocol::FrontendKind::Gpui,
            )
            .context("starting service client")?,
            next_request: 1,
            search_request: None,
            provider_scope: false,
            authorized_actions: HashMap::new(),
            action_confirmations: HashMap::new(),
            action_inputs: HashMap::new(),
            scope_actions: Vec::new(),
            pending_confirmation: None,
            icon_sources: HashMap::new(),
            preview_sources: HashMap::new(),
            theme_cards: HashMap::new(),
            layout: ServiceLayout::default(),
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

    fn begin_snapshot_change(&mut self) -> u64 {
        let request = self.request();
        // Retain display-only icons and preview while replacements load. They
        // grant no authority: revoke the old snapshot and detail identity now.
        self.detail_selection = None;
        self.details_available.clear();
        self.search_request = None;
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        self.action_inputs.clear();
        self.scope_actions.clear();
        self.pending_confirmation = None;
        request
    }

    pub fn search(&mut self, query: String) -> (u64, Result<(), String>) {
        let request = self.begin_snapshot_change();
        (request, self.connection.search(request, query))
    }

    /// Search the home view regardless of the provider scope the previous
    /// snapshot left active. The caller can keep painting its cached home list
    /// while this replacement is in flight.
    pub fn search_home(&mut self) -> (u64, Result<(), String>) {
        let request = self.begin_snapshot_change();
        self.provider_scope = false;
        // Home is the provider menu, not a strip. Drop display metadata from a
        // provider scope so cached home rows cannot inherit its filmstrip or
        // wallpaper-card layout for the frame before the refresh arrives.
        self.layout = ServiceLayout::List;
        self.icon_sources.clear();
        self.preview_sources.clear();
        self.theme_cards.clear();
        (request, self.connection.home_search(request))
    }

    pub fn home(&mut self) -> (u64, Result<(), String>) {
        let request = self.request();
        self.invalidate_details();
        self.details_available.clear();
        self.search_request = None;
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        self.action_inputs.clear();
        self.scope_actions.clear();
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
        self.action_inputs.clear();
        self.scope_actions.clear();
        self.pending_confirmation = None;
        self.icon_sources.clear();
        self.preview_sources.clear();
        self.theme_cards.clear();
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
        self.action_inputs.clear();
        self.scope_actions.clear();
        self.pending_confirmation = None;
        self.icon_sources.clear();
        self.preview_sources.clear();
        self.theme_cards.clear();
        self.details_available.clear();
        self.detail_selection = None;
        let result = self.connection.cancel();
        // The actor advances its request watermark before attempting enqueue,
        // including when the bounded command queue rejects this cancellation.
        self.next_request = self.next_request.wrapping_add(1).max(1);
        result
    }

    /// The field this action needs collected first, if the provider published
    /// one. The frontend draws it and calls [`ServiceBackend::activate`] with
    /// the value under [`ServiceActionInput::id`].
    pub fn action_input(&self, item: &Item, action: &Action) -> Option<ServiceActionInput> {
        self.action_inputs
            .get(&(
                item.provider.0.clone(),
                item.id.0.clone(),
                action.id.0.clone(),
            ))
            .cloned()
    }

    /// Commands the current scope publishes, in provider order. A frontend shows
    /// them beside the selected row's actions.
    pub fn scope_actions(&self) -> &[ServiceScopeAction] {
        &self.scope_actions
    }

    /// Stages a confirmation or sends the action immediately.
    ///
    /// Returns `Ok(Some(request))` when the action was sent, or `Ok(None)` when
    /// the provider published a confirmation prompt and the action was staged
    /// for explicit consent through [`ServiceBackend::confirm`].
    pub fn activate(
        &mut self,
        item: &Item,
        action: &Action,
        input: std::collections::BTreeMap<String, String>,
    ) -> Result<Option<u64>, String> {
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
                input,
            });
            return Ok(None);
        }
        self.send_activation(
            item.provider.0.clone(),
            item.id.0.clone(),
            action.id.0.clone(),
            search_request,
            false,
            input,
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
            staged.input,
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
        input: std::collections::BTreeMap<String, String>,
    ) -> Result<u64, String> {
        let request = self.request();
        self.invalidate_details();
        if confirmed {
            self.connection.activate_confirmed(
                request,
                search_request,
                provider,
                id,
                action,
                input,
            )?;
        } else {
            self.connection.activate_with_input(
                request,
                search_request,
                provider,
                id,
                action,
                false,
                input,
            )?;
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

    pub fn preview(&self, token: &str) -> Option<IconPixels> {
        self.connection.preview(token)
    }

    pub fn record_preview_source(&mut self, provider: &str, id: &str, token: &str) {
        if !provider.is_empty() && !id.is_empty() && !token.is_empty() {
            self.preview_sources.insert(
                (provider.to_owned(), id.to_owned()),
                format!("image://maccer-previews/{token}"),
            );
        }
    }

    pub fn record_icon_source(&mut self, provider: &str, id: &str, token: &str) {
        if !provider.is_empty() && !id.is_empty() && !token.is_empty() {
            self.icon_sources.insert(
                (provider.to_owned(), id.to_owned()),
                format!("image://maccer-icons/{token}"),
            );
        }
    }

    /// Registers one provider-published scope command under the same maps a row
    /// action uses, so `activate` needs no special case: it is a command whose
    /// subject is the provider's own identity for it.
    fn register_scope_action(&mut self, entry: &Value) {
        let field = |key: &str| {
            entry
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
        };
        let (Some(provider), Some(id), Some(action_id), Some(title)) = (
            field("providerId"),
            field("id"),
            field("actionId"),
            field("title"),
        ) else {
            return;
        };
        if let Some(prompt) = entry.get("confirmation").and_then(Value::as_str) {
            self.action_confirmations.insert(
                (provider.to_owned(), id.to_owned(), action_id.to_owned()),
                prompt.to_owned(),
            );
        }
        if let Some(field) = entry.get("input").and_then(action_input_view) {
            self.action_inputs.insert(
                (provider.to_owned(), id.to_owned(), action_id.to_owned()),
                field,
            );
        }
        let action = Action {
            id: ActionId(action_id.to_owned()),
            title: title.to_owned(),
        };
        let actions = self
            .authorized_actions
            .entry((provider.to_owned(), id.to_owned()))
            .or_default();
        if !actions.iter().any(|existing| existing.id == action.id) {
            actions.push(action.clone());
        }
        self.scope_actions.push(ServiceScopeAction {
            item: Item {
                id: ItemId(id.to_owned()),
                provider: ProviderId(provider.to_owned()),
                title: title.to_owned(),
                subtitle: entry
                    .get("subtitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                keywords: Vec::new(),
                icon: None,
                score: 0.0,
                payload: Value::Null,
            },
            action,
        });
    }

    pub fn apply_results(&mut self, event: &Value) -> Option<(u64, Vec<Item>)> {
        let request = event.get("request")?.as_u64()?;
        let rows = event.get("items")?.as_array()?;
        self.layout = ServiceLayout::from_wire(
            event
                .get("layout")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
        );
        self.authorized_actions.clear();
        self.action_confirmations.clear();
        self.action_inputs.clear();
        self.scope_actions.clear();
        // A delivered view is a new snapshot, so stale consent cannot survive.
        self.pending_confirmation = None;
        self.icon_sources.clear();
        self.preview_sources.clear();
        self.theme_cards.clear();
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
                if let Some(field) = value.get("input").and_then(action_input_view) {
                    self.action_inputs
                        .insert((provider.clone(), id.clone(), action_id.to_owned()), field);
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
            if let Some(card) = row.get("data").and_then(theme_card_view) {
                self.theme_cards
                    .insert((provider.clone(), id.clone()), card);
            }
            let icon = Some(IconDescriptor::Text(fallback));
            let payload = web_link_payload(row.get("data")).unwrap_or(Value::Null);
            items.push(Item {
                id: ItemId(id),
                provider: ProviderId(provider),
                title,
                subtitle,
                keywords: Vec::new(),
                icon,
                score: 0.0,
                payload,
            });
        }
        for entry in event
            .get("scopeActions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.register_scope_action(entry);
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

/// Reads a palette card out of a `themeSummary` record. A card missing a
/// colour is not drawn as one, so a broken palette never paints a half card.
fn theme_card_view(data: &Value) -> Option<ThemeCardView> {
    if data.get("type").and_then(Value::as_str) != Some("themeSummary") {
        return None;
    }
    let card = data.get("card")?;
    let color = |key: &str| card.get(key).and_then(Value::as_str).and_then(parse_color);
    let swatches = card
        .get("swatches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(parse_color)
        .take(6)
        .collect();
    Some(ThemeCardView {
        background: color("background")?,
        foreground: color("foreground")?,
        accent: color("accent")?,
        swatches,
        applied: card
            .get("applied")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

/// Copies a service-published web link into the frontend's private item
/// payload. Only absolute HTTP(S) URLs of a bounded length survive.
fn web_link_payload(data: Option<&Value>) -> Option<Value> {
    let data = data?;
    if data.get("type").and_then(Value::as_str) != Some("webLink") {
        return None;
    }
    if data.get("schemaVersion").and_then(Value::as_u64) != Some(1) {
        return None;
    }
    let url = data.get("url").and_then(Value::as_str)?;
    if url.len() > 4096 {
        return None;
    }
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(serde_json::json!({"url": url}))
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
    fn web_link_data_becomes_a_private_item_payload_for_the_native_preview() {
        let mut backend = ServiceBackend::connect(
            &std::env::temp_dir().join(format!("missing-gpui-weblink-{}.sock", std::process::id())),
        )
        .unwrap();
        let (_, items) = backend
            .apply_results(&json!({
                "request":8,
                "route":{"type":"provider","providerId":"web"},
                "items":[{
                    "id":"web:google",
                    "provider":"web",
                    "title":"Search with Google",
                    "actions":[{"id":"search","title":"Search"}],
                    "data":{
                        "type":"webLink",
                        "schemaVersion":1,
                        "url":"https://www.google.com/search?q=rust"
                    }
                }]
            }))
            .unwrap();
        assert_eq!(
            items[0].payload["url"],
            "https://www.google.com/search?q=rust"
        );
    }

    #[test]
    fn untrusted_web_link_data_never_reaches_the_frontend_payload() {
        for data in [
            json!({"type":"webLink","schemaVersion":1,"url":"file:///etc/passwd"}),
            json!({"type":"webLink","schemaVersion":1,"url":"javascript:alert(1)"}),
            json!({"type":"webLink","schemaVersion":2,"url":"https://example.com/"}),
        ] {
            let mut backend = ServiceBackend::connect(&std::env::temp_dir().join(format!(
                "missing-gpui-weblink-bad-{}.sock",
                std::process::id()
            )))
            .unwrap();
            let (_, items) = backend
                .apply_results(&json!({
                    "request":9,
                    "route":{"type":"provider","providerId":"web"},
                    "items":[{
                        "id":"web:google",
                        "provider":"web",
                        "title":"Search with Google",
                        "actions":[],
                        "data": data
                    }]
                }))
                .unwrap();
            assert!(items[0].payload.is_null());
        }
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
        assert_eq!(
            backend
                .activate(&item, &action, Default::default())
                .unwrap(),
            None
        );
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
        assert_eq!(
            backend
                .activate(&item, &action, Default::default())
                .unwrap(),
            None
        );
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
        assert_eq!(
            backend
                .activate(&item, &action, Default::default())
                .unwrap(),
            Some(1)
        );
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
