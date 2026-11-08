use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::{CStr, CString, c_char},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::config::{Config, config_path, history_path};
use crate::core::{
    Action, ActionId, ActionOutcome, CancellationToken, HistoryStore, IconDescriptor, Item, ItemId,
    Provider, ProviderConfig, ProviderId, ProviderRegistry, SearchCoordinator, SearchUpdate,
    UsageEvent,
};
use crate::macos::{GlobalHotkey, NativePlatform};
use crate::providers::apps::ApplicationProvider;
use crate::providers::calculator::CalculatorProvider;
use crate::providers::web::WebProvider;
use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MAX_COMMAND_BYTES: usize = 64 * 1024;
const MAX_QUERY_BYTES: usize = 4096;
const EVENT_CAPACITY: usize = 256;
const ICON_CAPACITY: usize = 512;
/// Wallpaper previews are ~590 KiB each, so the preview cache is sized by
/// memory rather than by count: twelve of them is about 7 MiB.
const PREVIEW_CAPACITY: usize = 12;
const ICON_WORKERS: usize = 4;

pub mod connection;
mod frontend_client;
pub mod frontend_startup;
mod service_client;

#[repr(C)]
pub struct MaccerIconPixels {
    pub bgra: *mut u8,
    pub len: usize,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone)]
pub(crate) struct PixelIcon {
    pub(crate) bgra: Vec<u8>,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// The two asset size classes one connection keeps. Icons and wallpaper
/// previews never share a budget or a token namespace.
#[derive(Clone)]
pub(crate) struct AssetCaches {
    pub(crate) icons: Arc<AssetCache>,
    pub(crate) previews: Arc<AssetCache>,
}
impl AssetCaches {
    pub(crate) fn new() -> Self {
        Self {
            icons: Arc::new(AssetCache::icons()),
            previews: Arc::new(AssetCache::previews()),
        }
    }
}

#[derive(Default)]
struct AssetCacheState {
    next_token: u64,
    by_key: HashMap<String, (String, PixelIcon)>,
    by_token: HashMap<String, PixelIcon>,
    insertion_order: VecDeque<String>,
}

/// Bounded BGRA cache for one asset size class. Icons and wallpaper previews
/// use separate instances so a strip of large images can never evict app icons.
pub(crate) struct AssetCache {
    max_edge: u32,
    capacity: usize,
    state: Mutex<AssetCacheState>,
}

impl AssetCache {
    pub(crate) fn icons() -> Self {
        Self::new(crate::protocol::MAX_ICON_EDGE, ICON_CAPACITY)
    }

    pub(crate) fn previews() -> Self {
        Self::new(crate::protocol::MAX_PREVIEW_EDGE, PREVIEW_CAPACITY)
    }

    fn new(max_edge: u32, capacity: usize) -> Self {
        Self {
            max_edge,
            capacity,
            state: Mutex::new(AssetCacheState::default()),
        }
    }

    pub(crate) fn token(&self, key: &str) -> Option<String> {
        self.state
            .lock()
            .ok()?
            .by_key
            .get(key)
            .map(|(token, _)| token.clone())
    }

    pub(crate) fn insert(&self, key: String, pixels: PixelIcon) -> Option<String> {
        if pixels.width == 0
            || pixels.height == 0
            || pixels.width > self.max_edge
            || pixels.height > self.max_edge
            || pixels.bgra.len() != pixels.width as usize * pixels.height as usize * 4
        {
            return None;
        }
        let mut state = self.state.lock().ok()?;
        if let Some((token, _)) = state.by_key.get(&key) {
            return Some(token.clone());
        }
        while state.by_key.len() >= self.capacity {
            let old_key = state.insertion_order.pop_front()?;
            if let Some((old_token, _)) = state.by_key.remove(&old_key) {
                state.by_token.remove(&old_token);
            }
        }
        state.next_token = state.next_token.checked_add(1)?;
        let token = state.next_token.to_string();
        state
            .by_key
            .insert(key.clone(), (token.clone(), pixels.clone()));
        state.by_token.insert(token.clone(), pixels);
        state.insertion_order.push_back(key);
        Some(token)
    }

    pub(crate) fn pixels(&self, token: &str) -> Option<PixelIcon> {
        self.state.lock().ok()?.by_token.get(token).cloned()
    }
}

const BACKGROUND_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

struct IconRequest {
    request: u64,
    provider: String,
    id: String,
    key: String,
    png: Option<Vec<u8>>,
    path: Option<std::path::PathBuf>,
    fixture: bool,
}

enum IconSchedule {
    Cached(String),
    Queued(IconRequest),
    Pending,
}

fn send_current_icon_event(
    events: &SyncSender<Value>,
    destroyed: &std::sync::atomic::AtomicBool,
    latest_request: &AtomicU64,
    request: u64,
    mut event: Value,
) -> bool {
    while !destroyed.load(Ordering::Acquire) && latest_request.load(Ordering::Acquire) == request {
        match events.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(next)) => {
                event = next;
                thread::sleep(std::time::Duration::from_millis(1));
            }
        }
    }
    false
}

fn prepare_icon_request(
    cache: &AssetCache,
    pending: &Mutex<HashSet<(u64, String)>>,
    request: u64,
    provider: &str,
    id: &str,
    path: &std::path::Path,
    fixture: bool,
) -> IconSchedule {
    let key = path.to_string_lossy().into_owned();
    if let Some(token) = cache.token(&key) {
        return IconSchedule::Cached(format!("image://maccer-icons/{token}"));
    }
    let pending_key = (request, key.clone());
    let Ok(mut pending) = pending.lock() else {
        return IconSchedule::Pending;
    };
    if !pending.insert(pending_key) {
        return IconSchedule::Pending;
    }
    IconSchedule::Queued(IconRequest {
        request,
        provider: provider.to_owned(),
        id: id.to_owned(),
        key,
        png: None,
        path: Some(path.to_owned()),
        fixture,
    })
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Command {
    Search {
        request: u64,
        query: String,
    },
    Activate {
        request: u64,
        search_request: u64,
        provider: String,
        id: String,
        action: String,
        #[serde(default)]
        confirmed: bool,
        /// Values collected for the action's published input field, keyed by
        /// that field's id. Absent for an ordinary action.
        #[serde(default)]
        input: std::collections::BTreeMap<String, String>,
    },
    Cancel,
    Home {
        request: u64,
    },
    Details {
        request: u64,
        search_request: u64,
        provider: String,
        id: String,
    },
    Refresh {
        request: u64,
        search_request: u64,
    },
}

#[derive(Clone)]
struct AgentContext {
    coordinator: Arc<SearchCoordinator>,
    // Only workers take this gate. An old start must never cancel a newer session.
    search_gate: Arc<Mutex<()>>,
    registry: Arc<ProviderRegistry>,
    history: Arc<HistoryStore>,
    events: SyncSender<Value>,
    current: Arc<Mutex<HashMap<(String, String), Item>>>,
    current_query: Arc<Mutex<String>>,
    active: Arc<Mutex<Option<CancellationToken>>>,
    active_query: Arc<Mutex<Option<(u64, String)>>>,
    result_request: Arc<AtomicU64>,
    destroyed: Arc<std::sync::atomic::AtomicBool>,
    fixture: bool,
    clipboard: Option<Arc<crate::providers::clipboard::ClipboardProvider>>,
    latest_request: Arc<AtomicU64>,
    icon_sender: async_channel::Sender<IconRequest>,
    icon_cache: Arc<AssetCache>,
    icon_pending: Arc<Mutex<HashSet<(u64, String)>>>,
}

pub struct MaccerAgent {
    receiver: Receiver<Value>,
    events: SyncSender<Value>,
    context: Arc<Mutex<Option<AgentContext>>>,
    initialization_error: Arc<Mutex<Option<String>>>,
    fixture: bool,
    hotkey: Option<GlobalHotkey>,
    queued: Arc<Mutex<Option<(u64, String)>>>,
    latest_request: Arc<AtomicU64>,
    result_request: Arc<AtomicU64>,
    destroyed: Arc<std::sync::atomic::AtomicBool>,
    icon_cache: Arc<AssetCache>,
    icon_sender: async_channel::Sender<IconRequest>,
    clipboard_change_count: Option<i64>,
    last_clipboard_poll: std::time::Instant,
    ipc: Option<service_client::IpcHandle>,
    frontend_control: Option<frontend_client::FrontendControl>,
}

#[derive(Serialize)]
struct PublicAction {
    id: String,
    title: String,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn event_error(events: &SyncSender<Value>, request: Option<u64>, error: impl ToString) {
    let mut value = json!({"type":"error", "message":error.to_string()});
    if let Some(request) = request {
        value["request"] = json!(request);
    }
    let _ = events.try_send(value);
}

struct FixtureProvider;
impl Provider for FixtureProvider {
    fn id(&self) -> ProviderId {
        ProviderId("apps".into())
    }
    fn name(&self) -> &str {
        "Applications"
    }
    fn search(
        &self,
        query: &crate::core::SearchQuery,
        ctx: &crate::core::SearchContext,
    ) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() {
            return Ok(vec![]);
        }
        let apps = [
            ("safari", "Safari", "/Applications/Safari.app"),
            (
                "terminal",
                "Terminal",
                "/Applications/Utilities/Terminal.app",
            ),
            ("japanese", "日本語", "/Applications/日本語.app"),
        ];
        let results: Vec<Item> = apps
            .into_iter()
            .filter(|(_, title, _)| {
                query.text.is_empty() || title.to_lowercase().contains(&query.text.to_lowercase())
            })
            .map(|(id, title, path)| Item {
                id: ItemId(id.into()),
                provider: ProviderId("apps".into()),
                title: title.into(),
                subtitle: Some(path.into()),
                keywords: vec![],
                icon: Some(IconDescriptor::ApplicationBundle(std::path::PathBuf::from(
                    format!("fixture://{id}"),
                ))),
                score: 0.0,
                payload: json!({"fixture":true}),
            })
            .collect();
        Ok(results)
    }
    fn actions(&self, _item: &Item) -> Vec<Action> {
        vec![
            Action {
                id: ActionId("open".into()),
                title: "Open".into(),
            },
            Action {
                id: ActionId("reveal".into()),
                title: "Reveal in Finder".into(),
            },
        ]
    }
    fn activate(&self, _item: &Item, action: &Action) -> Result<ActionOutcome> {
        match action.id.0.as_str() {
            "open" | "reveal" => Ok(ActionOutcome::Close),
            _ => bail!("unsupported fixture action"),
        }
    }
}

struct FixtureActions;
impl Provider for FixtureActions {
    fn id(&self) -> ProviderId {
        ProviderId("fixture".into())
    }
    fn name(&self) -> &str {
        "Fixture actions"
    }
    fn requires_explicit_scope(&self) -> bool {
        true
    }
    fn search(
        &self,
        _: &crate::core::SearchQuery,
        ctx: &crate::core::SearchContext,
    ) -> Result<Vec<Item>> {
        if ctx.cancellation.is_cancelled() {
            return Ok(vec![]);
        }
        Ok(vec![Item {
            id: ItemId("fixture-actions".into()),
            provider: self.id(),
            title: "Fixture actions".into(),
            subtitle: Some("Safe test output and keep-open outcomes".into()),
            keywords: vec![],
            icon: None,
            score: 0.0,
            payload: json!({}),
        }])
    }
    fn actions(&self, _: &Item) -> Vec<Action> {
        vec![
            Action {
                id: ActionId("output".into()),
                title: "Show fixture output".into(),
            },
            Action {
                id: ActionId("keep".into()),
                title: "Keep launcher open".into(),
            },
        ]
    }
    fn activate(&self, _: &Item, action: &Action) -> Result<ActionOutcome> {
        match action.id.0.as_str() {
            "output" => Ok(ActionOutcome::Output {
                title: "Fixture output".into(),
                text: "<b>literal output</b>".into(),
            }),
            "keep" => Ok(ActionOutcome::KeepOpen("Fixture action completed".into())),
            _ => bail!("unsupported fixture action"),
        }
    }
}

#[derive(Clone, Copy)]
struct FixturePlatform;
impl crate::macos::Platform for FixturePlatform {
    fn launch_application(&self, _: &std::path::Path) -> Result<()> {
        Ok(())
    }
    fn reveal_in_finder(&self, _: &std::path::Path) -> Result<()> {
        Ok(())
    }
    fn open_url(&self, _: &str) -> Result<()> {
        Ok(())
    }
    fn copy_text(&self, _: &str) -> Result<()> {
        Ok(())
    }
}

fn load_agent_config(fixture: bool) -> Result<Config> {
    if fixture {
        return Ok(Config::default());
    }
    let path = config_path(None)?;
    if path.is_file() {
        Config::load(&path)
    } else {
        Ok(Config::default())
    }
}

fn clipboard_changed(previous: Option<i64>, current: i64) -> bool {
    previous.is_some_and(|value| value != current)
}

type EmbeddedRegistry = (
    Arc<ProviderRegistry>,
    Arc<HistoryStore>,
    usize,
    Option<Arc<crate::providers::clipboard::ClipboardProvider>>,
);

fn make_registry(fixture: bool) -> Result<EmbeddedRegistry> {
    // Fixtures are hermetic even when a developer has personal configuration.
    let config = load_agent_config(fixture)?;
    config.validate()?;
    let history = if fixture {
        Arc::new(HistoryStore::in_memory()?)
    } else {
        let path = history_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Arc::new(HistoryStore::open(&path)?)
    };
    let mut registry = ProviderRegistry::new();
    let platform: Arc<dyn crate::macos::Platform> = if fixture {
        Arc::new(FixturePlatform)
    } else {
        Arc::new(NativePlatform)
    };
    if fixture {
        registry.register(Arc::new(FixtureProvider), config.providers.apps.clone())?;
        registry.register(
            Arc::new(FixtureActions),
            ProviderConfig {
                enabled: true,
                prefix: Some("!fixture".into()),
                priority: 0,
                default_search: false,
                max_results: 10,
            },
        )?;
    } else {
        let roots = config.apps.search_paths()?;
        let apps: Arc<dyn Provider> = if !config.providers.apps.enabled {
            Arc::new(ApplicationProvider::new(vec![], platform.clone()))
        } else if config.apps.watch {
            Arc::new(ApplicationProvider::watched(
                roots.clone(),
                platform.clone(),
            )?)
        } else {
            Arc::new(ApplicationProvider::new(
                crate::macos::discover_in(&roots)?,
                platform.clone(),
            ))
        };
        registry.register(apps, config.providers.apps.clone())?;
    }
    registry.register(
        Arc::new(CalculatorProvider::new(platform.clone())),
        config.providers.calculator.clone(),
    )?;
    if !fixture {
        registry.register(
            Arc::new(WebProvider::new(
                platform.clone(),
                config.web.engines.clone(),
                config.web.default_engine.clone(),
            )?),
            config.providers.web.clone(),
        )?;
    }
    if !fixture && config.providers.files.enabled {
        let cache = dirs::cache_dir()
            .context("finding files cache directory")?
            .join("maccer/files.sqlite3");
        let mut files_config = config.files.fd_config()?;
        if let Some(parent) = history_path()?.parent() {
            files_config.excluded.push(parent.to_path_buf());
        }
        let index = crate::macos::fd_index::FdIndex::open(files_config, &cache)?;
        index.start();
        registry.register(
            Arc::new(
                crate::providers::files::FileProvider::new(
                    index,
                    platform.clone(),
                    dirs::home_dir().context("finding home directory")?,
                )
                .with_roots(config.files.expanded_roots()?)
                .with_ignored_previews(config.files.expanded_ignored_previews()?)
                .with_history(history.clone()),
            ),
            config.providers.files.clone(),
        )?;
    }
    let clipboard = if !fixture && config.providers.clipboard.enabled {
        let provider = Arc::new(crate::providers::clipboard::ClipboardProvider::new(
            platform.clone(),
            config.clipboard.max_entries,
        ));
        registry.register(provider.clone(), config.providers.clipboard.clone())?;
        Some(provider)
    } else {
        None
    };
    if !fixture {
        registry.register(
            Arc::new(crate::providers::shell::ShellProvider::new(
                platform,
                config.shell.commands.clone(),
                dirs::home_dir().context("finding shell working directory")?,
            )?),
            config.providers.shell.clone(),
        )?;
    }
    Ok((
        Arc::new(registry),
        history,
        config.launcher.max_results,
        clipboard,
    ))
}

fn public_items(update: SearchUpdate, context: &AgentContext, request: u64) -> Option<Value> {
    let mut items = Vec::new();
    let mut retained = context.current.lock().unwrap_or_else(|e| e.into_inner());
    if context.latest_request.load(Ordering::Acquire) != request {
        return None;
    }
    retained.clear();
    context.result_request.store(request, Ordering::Release);
    for item in update.items {
        let provider = context.registry.get(&item.provider);
        let source_actions = if item.provider.0 == "picker" {
            vec![Action {
                id: ActionId("select".into()),
                title: "Use provider".into(),
            }]
        } else {
            provider
                .as_ref()
                .map(|p| p.actions(&item))
                .unwrap_or_default()
        };
        let actions = source_actions
            .into_iter()
            .filter(|action| action.id.0 != "paste")
            .map(|action| PublicAction {
                id: action.id.0,
                title: action.title,
            })
            .collect::<Vec<_>>();
        let icon_text = match &item.icon {
            Some(IconDescriptor::Text(text)) => text.clone(),
            _ => item.title.chars().next().unwrap_or('?').to_string(),
        };
        let icon_source = match &item.icon {
            Some(IconDescriptor::ApplicationBundle(path)) => context
                .icon_cache
                .token(&path.to_string_lossy())
                .map(|token| format!("image://maccer-icons/{token}"))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let row = json!({"id":item.id.0,"provider":item.provider.0,"title":item.title,"subtitle":item.subtitle,"iconSource":icon_source,"iconText":icon_text,"actions":actions});
        items.push(row);
        retained.insert((item.provider.0.clone(), item.id.0.clone()), item);
        while retained.len() > ICON_CAPACITY * 8 {
            if let Some(key) = retained.keys().next().cloned() {
                retained.remove(&key);
            } else {
                break;
            }
        }
    }
    let errors: Vec<_> = update
        .errors
        .into_iter()
        .map(|error| json!({"provider":error.provider.0,"message":error.message}))
        .collect();
    Some(
        json!({"type":"results","request":request,"items":items,"complete":update.complete,"errors":errors}),
    )
}

fn spawn_search(context: AgentContext, request: u64, query: String) {
    let events = context.events.clone();
    let _ = thread::Builder::new()
        .name("maccer-search".into())
        .spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_search(context, request, query)
            }))
            .is_err()
            {
                event_error(&events, Some(request), "launcher search worker panicked");
            }
        });
}

fn run_search(context: AgentContext, request: u64, query: String) {
    let gate = context
        .search_gate
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if context.destroyed.load(Ordering::Acquire)
        || context.latest_request.load(Ordering::Acquire) != request
    {
        return;
    }
    let session = context.coordinator.start(query.clone());
    if let Ok(mut active) = context.active.lock() {
        *active = Some(session.cancellation.clone());
    }
    if let Ok(mut current_query) = context.active_query.lock() {
        *current_query = Some((request, query));
    }
    if context.destroyed.load(Ordering::Acquire)
        || context.latest_request.load(Ordering::Acquire) != request
    {
        session.cancellation.cancel();
        return;
    }
    drop(gate);
    while let Ok(update) = session.receiver.recv_blocking() {
        if session.cancellation.is_cancelled()
            || context.latest_request.load(Ordering::Acquire) != request
        {
            break;
        }
        let icon_items = update
            .items
            .iter()
            .filter_map(|item| match &item.icon {
                Some(IconDescriptor::Png(bytes)) => Some((
                    item.provider.0.clone(),
                    item.id.0.clone(),
                    Some(bytes.to_vec()),
                    None,
                )),
                Some(IconDescriptor::ApplicationBundle(path)) => Some((
                    item.provider.0.clone(),
                    item.id.0.clone(),
                    None,
                    Some(path.clone()),
                )),
                _ => None,
            })
            .collect::<Vec<_>>();
        let Some(event) = public_items(update, &context, request) else {
            break;
        };
        // Result delivery may wait on a worker, never on the Qt owner thread.
        // In particular a complete update cannot be silently dropped behind icons.
        if context.events.send(event).is_err() {
            break;
        }
        for (provider, id, png, path) in icon_items {
            if context.destroyed.load(Ordering::Acquire)
                || context.latest_request.load(Ordering::Acquire) != request
            {
                break;
            }
            let schedule = if let Some(path) = path.as_deref() {
                prepare_icon_request(
                    &context.icon_cache,
                    &context.icon_pending,
                    request,
                    &provider,
                    &id,
                    path,
                    context.fixture,
                )
            } else {
                let key = format!("png:{provider}:{id}");
                match context.icon_pending.lock() {
                    Ok(mut pending) => {
                        if pending.insert((request, key.clone())) {
                            IconSchedule::Queued(IconRequest {
                                request,
                                provider: provider.clone(),
                                id: id.clone(),
                                key,
                                png,
                                path: None,
                                fixture: context.fixture,
                            })
                        } else {
                            IconSchedule::Pending
                        }
                    }
                    Err(_) => IconSchedule::Pending,
                }
            };
            match schedule {
                IconSchedule::Cached(source) => {
                    let event = json!({"type":"icon","request":request,"provider":provider,"id":id,"source":source});
                    let _ = send_current_icon_event(
                        &context.events,
                        &context.destroyed,
                        &context.latest_request,
                        request,
                        event,
                    );
                }
                IconSchedule::Queued(task) => {
                    let pending_key = (task.request, task.key.clone());
                    if context.icon_sender.send_blocking(task).is_err() {
                        if let Ok(mut pending) = context.icon_pending.lock() {
                            pending.remove(&pending_key);
                        }
                        break;
                    }
                }
                IconSchedule::Pending => {}
            }
        }
        if session.cancellation.is_cancelled() {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_activation(
    context: AgentContext,
    request: u64,
    search_request: u64,
    provider: String,
    id: String,
    action_id: String,
    cancellation: CancellationToken,
    query: String,
) {
    if cancellation.is_cancelled() {
        return;
    }
    if context.destroyed.load(Ordering::Acquire)
        || context.latest_request.load(Ordering::Acquire) != request
    {
        return;
    }
    let item = context
        .current
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&(provider.clone(), id.clone()))
        .cloned();
    if context.result_request.load(Ordering::Acquire) != search_request {
        event_error(&context.events, Some(request), "selected results are stale");
        return;
    }
    let Some(item) = item else {
        event_error(&context.events, Some(request), "selected result is stale");
        return;
    };
    if item.provider.0 == "picker" && action_id == "select" {
        if let Some(prefix) = item.payload.get("prefix").and_then(Value::as_str) {
            let _=context.events.try_send(json!({"type":"outcome","request":request,"kind":"setQuery","text":format!("{prefix} ")}));
        } else {
            event_error(
                &context.events,
                Some(request),
                "provider has no search prefix",
            );
        }
        return;
    }
    let Some(provider_impl) = context.registry.get(&item.provider) else {
        event_error(&context.events, Some(request), "provider is unavailable");
        return;
    };
    let action = provider_impl
        .actions(&item)
        .into_iter()
        .find(|a| a.id.0 == action_id && a.id.0 != "paste");
    let Some(action) = action else {
        event_error(&context.events, Some(request), "unsupported action");
        return;
    };
    match provider_impl.activate_with_cancellation(&item, &action, &cancellation) {
        Ok(outcome) if !cancellation.is_cancelled() => {
            if matches!(&outcome, ActionOutcome::PasteText(_)) {
                event_error(
                    &context.events,
                    Some(request),
                    "paste is not supported in the embedded launcher",
                );
                return;
            }
            let history_result = if provider_impl.records_usage() {
                context.history.record(&UsageEvent {
                    query: query.clone(),
                    provider: item.provider.clone(),
                    item: item.id.clone(),
                    action: action.id.clone(),
                    timestamp: now(),
                })
            } else {
                Ok(())
            };
            if let Err(error) = history_result {
                event_error(&context.events, Some(request), error);
                return;
            }
            let event = match outcome {
                ActionOutcome::Close => {
                    json!({"type":"outcome","request":request,"kind":"close","focusTransferred":transfers_focus(context.fixture, &item.provider.0, &action.id.0)})
                }
                ActionOutcome::KeepOpen(text) => {
                    json!({"type":"outcome","request":request,"kind":"keepOpen","text":text})
                }
                ActionOutcome::SetQuery(text) => {
                    json!({"type":"outcome","request":request,"kind":"setQuery","text":text})
                }
                ActionOutcome::RefreshSearch => {
                    json!({"type":"outcome","request":request,"kind":"refresh"})
                }
                ActionOutcome::Output { title, text } => {
                    json!({"type":"outcome","request":request,"kind":"output","title":title,"text":text})
                }
                ActionOutcome::PasteText(_) => {
                    unreachable!("unsupported paste outcome filtered before history")
                }
            };
            let _ = context.events.send(event);
        }
        Ok(_) => {}
        Err(error) if !cancellation.is_cancelled() => {
            event_error(&context.events, Some(request), error)
        }
        Err(_) => {}
    }
}

fn transfers_focus(fixture: bool, provider: &str, action: &str) -> bool {
    !fixture
        && match provider {
            "apps" => matches!(action, "open" | "reveal"),
            "files" => matches!(action, "open" | "open-directory" | "reveal" | "quick-look"),
            "web" => action == "search",
            _ => false,
        }
}

fn make_agent(
    fixture: bool,
    shortcut_error: String,
    hotkey: Option<GlobalHotkey>,
) -> Result<MaccerAgent> {
    let (event_tx, event_rx) = mpsc::sync_channel(EVENT_CAPACITY);
    let context = Arc::new(Mutex::new(None));
    let queued = Arc::new(Mutex::new(None));
    let context_for_thread = context.clone();
    let initialization_error = Arc::new(Mutex::new(None));
    let initialization_error_for_thread = initialization_error.clone();
    let events = event_tx.clone();
    let queued_for_thread = queued.clone();
    let result_request = Arc::new(AtomicU64::new(0));
    let destroyed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let result_request_for_thread = result_request.clone();
    let result_request_for_context = result_request.clone();
    let destroyed_for_context = destroyed.clone();
    let latest_request = Arc::new(AtomicU64::new(0));
    let latest_request_for_thread = latest_request.clone();
    let destroyed_for_thread = destroyed.clone();
    let ready_shortcut_error = shortcut_error.clone();
    let (icon_sender, icon_receiver) = async_channel::bounded::<IconRequest>(ICON_CAPACITY);
    let icon_cache = Arc::new(AssetCache::icons());
    let icon_pending = Arc::new(Mutex::new(HashSet::<(u64, String)>::new()));
    let icon_cache_for_context = icon_cache.clone();
    let icon_sender_for_agent = icon_sender.clone();
    for worker_id in 0..ICON_WORKERS {
        let receiver = icon_receiver.clone();
        let icon_events = events.clone();
        let cache = icon_cache.clone();
        let pending = icon_pending.clone();
        let latest = result_request_for_thread.clone();
        let destroyed = destroyed_for_thread.clone();
        let _ = thread::Builder::new()
            .name(format!("maccer-icon-{worker_id}"))
            .spawn(move || {
                while let Ok(task) = receiver.recv_blocking() {
                    let pending_key = (task.request, task.key.clone());
                    if destroyed.load(Ordering::Acquire) {
                        if let Ok(mut pending) = pending.lock() {
                            pending.remove(&pending_key);
                        }
                        break;
                    }
                    if latest.load(Ordering::Acquire) != task.request {
                        if let Ok(mut pending) = pending.lock() {
                            pending.remove(&pending_key);
                        }
                        continue;
                    }
                    let cached_source = task
                        .path
                        .as_ref()
                        .and_then(|_| cache.token(&task.key))
                        .map(|token| format!("image://maccer-icons/{token}"));
                    let pixel_icon = task
                        .path
                        .as_ref()
                        .filter(|_| cached_source.is_none())
                        .and_then(|path| {
                            if task.fixture {
                                let seed = task
                                    .id
                                    .bytes()
                                    .fold(0u8, |acc, byte| acc.wrapping_add(byte));
                                Some(PixelIcon {
                                    bgra: vec![
                                        seed, 80, 220, 255, seed, 80, 220, 255, seed, 80, 220,
                                        255, seed, 80, 220, 255,
                                    ],
                                    width: 2,
                                    height: 2,
                                })
                            } else {
                                crate::macos::application_icon_pixels(path).map(|pixels| {
                                    PixelIcon {
                                        bgra: pixels.bgra,
                                        width: pixels.width,
                                        height: pixels.height,
                                    }
                                })
                            }
                        });
                    let source = cached_source.or_else(|| {
                        if let Some(pixels) = pixel_icon {
                            cache
                                .insert(task.key.clone(), pixels)
                                .map(|token| format!("image://maccer-icons/{token}"))
                        } else {
                            task.png.map(|bytes| {
                                format!("data:image/png;base64,{}", STANDARD.encode(bytes))
                            })
                        }
                    });
                    if let Ok(mut pending) = pending.lock() {
                        pending.remove(&pending_key);
                    }
                    if let Some(source) = source {
                        let event = json!({"type":"icon","request":task.request,"provider":task.provider,"id":task.id,"source":source});
                        let _ = send_current_icon_event(
                            &icon_events,
                            &destroyed,
                            &latest,
                            task.request,
                            event,
                        );
                    }
                }
            });
    }
    thread::Builder::new()
        .name("maccer-agent-init".into())
        .spawn(move || {
            if destroyed_for_thread.load(Ordering::Acquire) {
                return;
            }
            let result = make_registry(fixture).map(|(registry, history, limit, clipboard)| {
                let coordinator = Arc::new(SearchCoordinator::new(
                    registry.clone(),
                    history.clone(),
                    limit,
                ));
                AgentContext {
                    coordinator,
                    search_gate: Arc::new(Mutex::new(())),
                    registry,
                    history,
                    events: events.clone(),
                    current: Arc::new(Mutex::new(HashMap::new())),
                    current_query: Arc::new(Mutex::new(String::new())),
                    active: Arc::new(Mutex::new(None)),
                    active_query: Arc::new(Mutex::new(None)),
                    result_request: result_request_for_context.clone(),
                    destroyed: destroyed_for_context.clone(),
                    fixture,
                    clipboard,
                    latest_request: latest_request_for_thread.clone(),
                    icon_sender: icon_sender.clone(),
                    icon_cache: icon_cache_for_context.clone(),
                    icon_pending: icon_pending.clone(),
                }
            });
            match result {
                Ok(ctx) => {
                    if destroyed_for_thread.load(Ordering::Acquire) {
                        return;
                    }
                    *context_for_thread.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(ctx.clone());
                    let _ = events
                        .try_send(json!({"type":"ready","shortcutError":ready_shortcut_error}));
                    let monitor = ctx.clone();
                    thread::Builder::new()
                        .name("maccer-revision-monitor".into())
                        .spawn(move || {
                            let mut revisions: HashMap<String, u64> = HashMap::new();
                            while !monitor.destroyed.load(Ordering::Acquire) {
                                thread::sleep(BACKGROUND_POLL_INTERVAL);
                                let Some((request, query)) = monitor
                                    .active_query
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone()
                                else {
                                    continue;
                                };
                                let current: HashMap<String, u64> = monitor
                                    .registry
                                    .descriptors()
                                    .into_iter()
                                    .filter_map(|d| {
                                        monitor.registry.get(&d.id).map(|p| (d.id.0, p.revision()))
                                    })
                                    .collect();
                                if !revisions.is_empty()
                                    && revisions != current
                                    && monitor.latest_request.load(Ordering::Acquire) == request
                                {
                                    spawn_search(monitor.clone(), request, query);
                                }
                                revisions = current;
                            }
                        })
                        .ok();
                    if destroyed_for_thread.load(Ordering::Acquire) {
                        return;
                    }
                    if let Some((request, query)) = queued_for_thread
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                    {
                        ctx.latest_request.store(request, Ordering::Release);
                        ctx.current_query
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone_from(&query);
                        spawn_search(ctx.clone(), request, query);
                    }
                }
                Err(_) if destroyed_for_thread.load(Ordering::Acquire) => {}
                Err(error) => {
                    let message = format!("{error:#}");
                    *initialization_error_for_thread
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
                    event_error(&events, None, message);
                }
            }
        })
        .context("starting launcher provider initialization")?;
    Ok(MaccerAgent {
        receiver: event_rx,
        events: event_tx,
        context,
        initialization_error,
        fixture,
        hotkey,
        queued,
        latest_request,
        result_request,
        destroyed,
        icon_cache,
        icon_sender: icon_sender_for_agent,
        clipboard_change_count: None,
        last_clipboard_poll: std::time::Instant::now() - std::time::Duration::from_secs(1),
        ipc: None,
        frontend_control: None,
    })
}

fn c_string(text: String) -> *mut c_char {
    CString::new(text.replace('\0', "�"))
        .map(CString::into_raw)
        .unwrap_or(ptr::null_mut())
}
fn cancel_current(agent: &MaccerAgent) {
    if let Some(context) = agent
        .context
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        && let Ok(mut active) = context.active.lock()
        && let Some(token) = active.take()
    {
        token.cancel();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn maccer_agent_create(fixture: bool) -> *mut MaccerAgent {
    std::panic::catch_unwind(|| {
        let (hotkey, shortcut_error) = if fixture {
            (None, String::new())
        } else {
            let config = load_agent_config(false);
            match config.and_then(|config| GlobalHotkey::new(&config.launcher.hotkey)) {
                Ok(hotkey) => (Some(hotkey), String::new()),
                Err(error) => (None, error.to_string()),
            }
        };
        Some(Box::into_raw(Box::new(
            make_agent(fixture, shortcut_error, hotkey).ok()?,
        )))
    })
    .ok()
    .flatten()
    .unwrap_or(ptr::null_mut())
}

/// Connect to an already-running service without initializing the embedded backend.
///
/// # Safety
/// `socket_path` must be null or point to a valid NUL-terminated string for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_connect(socket_path: *const c_char) -> *mut MaccerAgent {
    std::panic::catch_unwind(|| {
        if socket_path.is_null() {
            return Some(ptr::null_mut());
        }
        // SAFETY: caller provides a NUL-terminated path for this call.
        let bytes = unsafe { CStr::from_ptr(socket_path) }.to_bytes();
        if bytes.is_empty() || bytes.len() > 4096 {
            return Some(ptr::null_mut());
        }
        let Ok(path) = std::str::from_utf8(bytes) else {
            return Some(ptr::null_mut());
        };
        let path = std::path::PathBuf::from(path);
        if !path.is_absolute() {
            return Some(ptr::null_mut());
        }

        let (events, receiver) = mpsc::sync_channel(EVENT_CAPACITY);
        let caches = AssetCaches::new();
        let ipc = service_client::IpcHandle::start(&path, events.clone(), caches.clone()).ok()?;
        let (icon_sender, _icon_receiver) = async_channel::bounded::<IconRequest>(1);
        Some(Box::into_raw(Box::new(MaccerAgent {
            receiver,
            events,
            context: Arc::new(Mutex::new(None)),
            initialization_error: Arc::new(Mutex::new(None)),
            fixture: true,
            hotkey: None,
            queued: Arc::new(Mutex::new(None)),
            latest_request: Arc::new(AtomicU64::new(0)),
            result_request: Arc::new(AtomicU64::new(0)),
            destroyed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            icon_cache: caches.icons,
            icon_sender,
            clipboard_change_count: None,
            last_clipboard_poll: std::time::Instant::now(),
            ipc: Some(ipc),
            frontend_control: None,
        })))
    })
    .ok()
    .flatten()
    .unwrap_or(ptr::null_mut())
}

/// Connect to a service and claim presentation control for `gpui` or `pill`.
/// Search remains available even when another frontend owns presentation.
///
/// # Safety
/// Both pointers must be null or valid NUL-terminated strings for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_connect_frontend(
    socket_path: *const c_char,
    frontend: *const c_char,
) -> *mut MaccerAgent {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if socket_path.is_null() || frontend.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: caller guarantees both pointers remain valid for this call.
        let kind = match unsafe { CStr::from_ptr(frontend) }.to_bytes() {
            b"gpui" => crate::protocol::FrontendKind::Gpui,
            b"pill" => crate::protocol::FrontendKind::Pill,
            _ => return ptr::null_mut(),
        };
        // SAFETY: forwarded unchanged; the existing entry point validates the path.
        let agent = unsafe { maccer_agent_connect(socket_path) };
        if agent.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: newly allocated handle is exclusively owned here.
        let agent_ref = unsafe { &mut *agent };
        let Some(path) = (unsafe { CStr::from_ptr(socket_path) }).to_str().ok() else {
            unsafe { maccer_agent_destroy(agent) };
            return ptr::null_mut();
        };
        match frontend_client::FrontendControl::start(
            std::path::Path::new(path),
            kind,
            agent_ref.events.clone(),
        ) {
            Ok(control) => {
                agent_ref.frontend_control = Some(control);
                agent
            }
            Err(_) => {
                unsafe { maccer_agent_destroy(agent) };
                ptr::null_mut()
            }
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Send one JSON command to the agent.
///
/// # Safety
/// `handle` must be a live handle from `maccer_agent_create` or a connect
/// function, and `json_ptr` must point to a valid NUL-terminated string. Calls
/// on the same handle must be serialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_send(
    handle: *mut MaccerAgent,
    json_ptr: *const c_char,
) -> *mut c_char {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if handle.is_null() || json_ptr.is_null() {
            return c_string("agent and JSON must be non-null".into());
        }
        // SAFETY: caller provides a NUL-terminated C string for this call.
        let bytes = unsafe { CStr::from_ptr(json_ptr) }.to_bytes();
        if bytes.len() > MAX_COMMAND_BYTES {
            return c_string("command exceeds 64 KiB".into());
        }
        let command: Command = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(error) => return c_string(format!("invalid command: {error}")),
        };
        // SAFETY: handle comes from create and calls are serialized as documented.
        let agent = unsafe { &*handle };
        if let Some(ipc) = &agent.ipc {
            let command = match command {
                Command::Search { request, query } => {
                    service_client::IpcCommand::Search { request, query }
                }
                Command::Activate {
                    request,
                    search_request,
                    provider,
                    id,
                    action,
                    confirmed,
                    input,
                } => service_client::IpcCommand::Activate {
                    request,
                    search_request,
                    provider,
                    id,
                    action,
                    confirmed,
                    input,
                },
                Command::Cancel => service_client::IpcCommand::Cancel,
                Command::Home { request } => service_client::IpcCommand::Home { request },
                Command::Details {
                    request,
                    search_request,
                    provider,
                    id,
                } => service_client::IpcCommand::Details {
                    request,
                    search_request,
                    provider,
                    id,
                },
                Command::Refresh {
                    request,
                    search_request,
                } => service_client::IpcCommand::Refresh {
                    request,
                    search_request,
                },
            };
            return match ipc.send(command) {
                Ok(()) => ptr::null_mut(),
                Err(message) => c_string(message),
            };
        }
        match command {
            Command::Search { request, query } => {
                if query.len() > MAX_QUERY_BYTES {
                    return c_string("query exceeds 4096 bytes".into());
                }
                if request <= agent.latest_request.load(Ordering::Acquire) {
                    return ptr::null_mut();
                }
                agent.latest_request.store(request, Ordering::Release);
                agent.result_request.store(0, Ordering::Release);
                *agent.queued.lock().unwrap_or_else(|e| e.into_inner()) = None;
                cancel_current(agent);
                if let Some(context) = agent
                    .context
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .cloned()
                {
                    context.latest_request.store(request, Ordering::Release);
                    *context
                        .current_query
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = query.clone();
                    context
                        .current
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clear();
                    spawn_search(context, request, query);
                } else {
                    let mut queued = agent.queued.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(error) = agent
                        .initialization_error
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                    {
                        event_error(&agent.events, Some(request), error);
                        return ptr::null_mut();
                    }
                    *queued = Some((request, query.clone()));
                    let initialized = agent
                        .context
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .as_ref()
                        .cloned();
                    if let Some(context) = initialized {
                        queued.take();
                        drop(queued);
                        context.latest_request.store(request, Ordering::Release);
                        *context
                            .current_query
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = query.clone();
                        context
                            .current
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .clear();
                        spawn_search(context, request, query);
                    }
                }
            }
            Command::Activate {
                request,
                search_request,
                provider,
                id,
                action,
                // Embedded providers publish no input field, so a value here is
                // only meaningful on the service path.
                confirmed: _,
                input: _,
            } => {
                if request <= agent.latest_request.load(Ordering::Acquire) {
                    return ptr::null_mut();
                }
                agent.latest_request.store(request, Ordering::Release);
                cancel_current(agent);
                let context = agent
                    .context
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .cloned();
                if let Some(context) = context {
                    if context.result_request.load(Ordering::Acquire) != search_request {
                        event_error(&agent.events, Some(request), "selected results are stale");
                        return ptr::null_mut();
                    }
                    if let Ok(mut active_query) = context.active_query.lock() {
                        *active_query = None;
                    }
                    let query = context
                        .current_query
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let token = CancellationToken::default();
                    if let Ok(mut active) = context.active.lock() {
                        *active = Some(token.clone());
                    }
                    let events = context.events.clone();
                    let _ = thread::Builder::new()
                        .name("maccer-activation".into())
                        .spawn(move || {
                            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                run_activation(
                                    context,
                                    request,
                                    search_request,
                                    provider,
                                    id,
                                    action,
                                    token,
                                    query,
                                )
                            }))
                            .is_err()
                            {
                                event_error(
                                    &events,
                                    Some(request),
                                    "launcher action worker panicked",
                                );
                            }
                        });
                } else {
                    event_error(
                        &agent.events,
                        Some(request),
                        "launcher is still initializing",
                    );
                }
            }
            Command::Details { .. } => {
                return c_string("details are only available in service mode".into());
            }
            Command::Refresh { .. } => {
                return c_string("asset recovery is only available in service mode".into());
            }
            Command::Home { .. } => {
                return c_string("home navigation is only available in service mode".into());
            }
            Command::Cancel => {
                let invalid = agent.latest_request.fetch_add(1, Ordering::AcqRel) + 1;
                agent.result_request.store(0, Ordering::Release);
                *agent.queued.lock().unwrap_or_else(|e| e.into_inner()) = None;
                if let Some(context) = agent
                    .context
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    context.latest_request.store(invalid, Ordering::Release);
                    context.result_request.store(0, Ordering::Release);
                    context
                        .current
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clear();
                }
                cancel_current(agent);
                if let Some(context) = agent
                    .context
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                {
                    *context
                        .active_query
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = None;
                }
                *agent.queued.lock().unwrap_or_else(|e| e.into_inner()) = None;
            }
        }
        ptr::null_mut()
    }))
    .unwrap_or_else(|_| c_string("panic while handling launcher command".into()))
}

/// Pop one queued JSON event, or return null when the queue is empty.
///
/// # Safety
/// `handle` must be a live handle and calls on it must be serialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_poll(handle: *mut MaccerAgent) -> *mut c_char {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if handle.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: caller provides a live handle and serializes calls.
        let agent = unsafe { &mut *handle };
        if agent
            .hotkey
            .as_ref()
            .is_some_and(GlobalHotkey::try_toggle_requested)
        {
            let _ = agent.events.try_send(json!({"type":"toggle"}));
        }
        if !agent.fixture
            && let Some(context) = agent
                .context
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
                .cloned()
            && agent.last_clipboard_poll.elapsed() >= BACKGROUND_POLL_INTERVAL
        {
            agent.last_clipboard_poll = std::time::Instant::now();
            if let Some(clipboard) = &context.clipboard
                && let Ok(Some(snapshot)) =
                    crate::macos::clipboard::snapshot_if_changed(agent.clipboard_change_count)
            {
                let previous = agent.clipboard_change_count.replace(snapshot.change_count);
                if clipboard_changed(previous, snapshot.change_count) {
                    clipboard.capture(snapshot.change_count, snapshot.text.as_deref());
                } else {
                    clipboard.capture(snapshot.change_count, None);
                }
            }
        }
        match agent.receiver.try_recv() {
            Ok(value) => c_string(value.to_string()),
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => ptr::null_mut(),
        }
    }))
    .unwrap_or(ptr::null_mut())
}

/// Destroy an agent and release its resources.
///
/// # Safety
/// `handle` must be null or the unique pointer returned by a create/connect
/// function and must not be used again after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_destroy(handle: *mut MaccerAgent) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if handle.is_null() {
            return;
        }
        // SAFETY: caller transfers the unique pointer returned by create exactly once.
        let agent = unsafe { Box::from_raw(handle) };
        agent.destroyed.store(true, Ordering::Release);
        if let Some(frontend) = &agent.frontend_control {
            frontend.stop();
        }
        if let Some(ipc) = &agent.ipc {
            ipc.invalidate();
        }
        agent.icon_sender.close();
        agent.latest_request.fetch_add(1, Ordering::AcqRel);
        agent.result_request.store(0, Ordering::Release);
        cancel_current(&agent);
        *agent.queued.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // Detached workers own only Arcs and never borrow the handle. Dropping
        // coordinator/history does not join them; cooperative provider calls may finish later.
        drop(agent);
    }));
}

/// Resolve an opaque icon token to owned BGRA pixels.
///
/// # Safety
/// `handle` must be a live handle, `token_ptr` must be null or point to a valid
/// NUL-terminated token, and calls on the handle must be serialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_icon(
    handle: *mut MaccerAgent,
    token_ptr: *const c_char,
) -> *mut MaccerIconPixels {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if handle.is_null() || token_ptr.is_null() {
            return None;
        }
        // SAFETY: caller provides a valid NUL-terminated token and live serialized handle.
        let token = unsafe { CStr::from_ptr(token_ptr) }.to_str().ok()?;
        if token.is_empty()
            || !token.bytes().all(|byte| byte.is_ascii_digit())
            || token.parse::<u64>().ok()?.to_string() != token
        {
            return None;
        }
        let agent = unsafe { &*handle };
        let pixels = agent.icon_cache.pixels(token)?;
        let mut bgra = pixels.bgra.into_boxed_slice();
        let len = bgra.len();
        let ptr = bgra.as_mut_ptr();
        std::mem::forget(bgra);
        Some(Box::into_raw(Box::new(MaccerIconPixels {
            bgra: ptr,
            len,
            width: pixels.width,
            height: pixels.height,
        })))
    }))
    .ok()
    .flatten()
    .unwrap_or(ptr::null_mut())
}

/// Free pixels returned by `maccer_agent_icon`.
///
/// # Safety
/// `icon` must be null or a pointer returned by `maccer_agent_icon` that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_icon_free(icon: *mut MaccerIconPixels) {
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if icon.is_null() {
            return;
        }
        // SAFETY: icon was returned by maccer_agent_icon and has not been freed.
        let icon = unsafe { Box::from_raw(icon) };
        if !icon.bgra.is_null() && icon.len > 0 {
            // SAFETY: icon owns this exact boxed pixel allocation and length.
            unsafe {
                drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                    icon.bgra, icon.len,
                )))
            };
        }
    }));
}

/// Free a string returned by this library.
///
/// # Safety
/// `value` must be null or a pointer returned by this library that has not
/// already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn maccer_agent_string_free(value: *mut c_char) {
    if !value.is_null() {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
            drop(CString::from_raw(value));
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_request_can_wait_for_an_icon_while_an_older_request_is_resolving() {
        let cache = Arc::new(AssetCache::icons());
        let pending = Mutex::new(HashSet::new());
        let path = std::path::PathBuf::from("/Applications/Safari.app");
        let first = prepare_icon_request(&cache, &pending, 1, "apps", "safari", &path, false);
        let first_task = match first {
            IconSchedule::Queued(task) => task,
            _ => panic!("first request should queue"),
        };
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let cache_for_worker = cache.clone();
        let path_key = path.to_string_lossy().into_owned();
        let resolver = thread::spawn(move || {
            assert_eq!(first_task.request, 1);
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            let pixels = PixelIcon {
                bgra: vec![1, 2, 3, 4],
                width: 1,
                height: 1,
            };
            cache_for_worker.insert(path_key, pixels).unwrap();
        });
        started_rx.recv().unwrap(); // request 1 is blocked inside fake native resolution.

        // Request 2 gets a results row while request 1 is still being resolved.
        let second = prepare_icon_request(&cache, &pending, 2, "apps", "safari", &path, false);
        assert!(matches!(second, IconSchedule::Queued(_)));
        release_tx.send(()).unwrap();
        resolver.join().unwrap();
        // A cache hit discovered after the results row was built must still notify request 2.
        match prepare_icon_request(&cache, &pending, 2, "apps", "safari", &path, false) {
            IconSchedule::Cached(source) => assert!(source.starts_with("image://maccer-icons/")),
            _ => panic!("warm icon must be delivered to the current request"),
        }
        // If B was already queued before A populated the cache, B still owns a queued
        // completion and must be allowed to publish it rather than inheriting A's request.
        let queued = match second {
            IconSchedule::Queued(task) => task,
            _ => unreachable!(),
        };
        assert_eq!(queued.request, 2);
        assert_ne!(queued.request, 1);
        let (events, receiver) = mpsc::sync_channel(1);
        let destroyed = std::sync::atomic::AtomicBool::new(false);
        let latest = AtomicU64::new(2);
        assert!(send_current_icon_event(
            &events,
            &destroyed,
            &latest,
            queued.request,
            json!({"type":"icon","request":queued.request,"provider":queued.provider,"id":queued.id,"source":"image://maccer-icons/1"}),
        ));
        assert_eq!(receiver.try_recv().unwrap()["request"], 2);
    }

    #[test]
    fn fixture_native_icon_pixels_are_cached_and_returned_inline_after_cancel() {
        let agent = maccer_agent_create(true);
        assert!(!agent.is_null());
        poll_until(agent, |event| event["type"] == "ready");
        let search = CString::new(r#"{"op":"search","request":1,"query":"safari"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, search.as_ptr()) }.is_null());
        let first = poll_until(agent, |event| {
            event["type"] == "results" && event["request"] == 1
        });
        assert_eq!(first["items"][0]["iconSource"], "");
        let icon_event = poll_until(agent, |event| {
            event["type"] == "icon" && event["request"] == 1
        });
        let source = icon_event["source"].as_str().unwrap();
        assert!(source.starts_with("image://maccer-icons/"));
        let token = CString::new(source.rsplit('/').next().unwrap()).unwrap();
        let pixels = unsafe { maccer_agent_icon(agent, token.as_ptr()) };
        assert!(!pixels.is_null());
        let value = unsafe { &*pixels };
        assert_eq!((value.width, value.height, value.len), (2, 2, 16));
        assert!(!value.bgra.is_null());
        let bytes = unsafe { std::slice::from_raw_parts(value.bgra, value.len) };
        assert_eq!(
            bytes,
            &[
                b'v', 80, 220, 255, b'v', 80, 220, 255, b'v', 80, 220, 255, b'v', 80, 220, 255
            ]
        );
        unsafe { maccer_agent_icon_free(pixels) };

        let cancel = CString::new(r#"{"op":"cancel"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, cancel.as_ptr()) }.is_null());
        let again = CString::new(r#"{"op":"search","request":3,"query":"safari"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, again.as_ptr()) }.is_null());
        let warm = poll_until(agent, |event| {
            event["type"] == "results" && event["request"] == 3
        });
        assert_eq!(warm["items"][0]["iconSource"], source);
        unsafe { maccer_agent_destroy(agent) };
    }

    #[test]
    fn icon_cache_is_bounded_and_never_reuses_tokens() {
        let cache = AssetCache::icons();
        let pixels = PixelIcon {
            bgra: vec![1, 2, 3, 4],
            width: 1,
            height: 1,
        };
        let mut tokens = HashSet::new();
        for index in 0..(ICON_CAPACITY + 2) {
            let token = cache
                .insert(format!("path-{index}"), pixels.clone())
                .unwrap();
            assert!(tokens.insert(token));
        }
        let state = cache.state.lock().unwrap();
        assert_eq!(state.by_key.len(), ICON_CAPACITY);
        assert_eq!(state.by_token.len(), ICON_CAPACITY);
        assert_eq!(tokens.len(), ICON_CAPACITY + 2);
        assert!(!state.by_key.contains_key("path-0"));
    }

    #[test]
    fn icon_accessor_rejects_missing_and_malformed_tokens() {
        let agent = maccer_agent_create(true);
        assert!(!agent.is_null());
        assert!(unsafe { maccer_agent_icon(agent, ptr::null()) }.is_null());
        let bad = CString::new("not-a-token").unwrap();
        assert!(unsafe { maccer_agent_icon(agent, bad.as_ptr()) }.is_null());
        unsafe { maccer_agent_destroy(agent) };
    }

    #[test]
    fn focus_transfer_classification_keeps_copy_actions_in_previous_app() {
        assert!(transfers_focus(false, "files", "open-directory"));
        assert!(transfers_focus(false, "apps", "open"));
        assert!(transfers_focus(false, "web", "search"));
        assert!(!transfers_focus(false, "files", "copy-path"));
        assert!(!transfers_focus(false, "web", "copy-url"));
        assert!(!transfers_focus(true, "apps", "open"));
    }

    #[test]
    fn latest_search_completes_when_old_workers_wait_at_session_start() {
        let agent = maccer_agent_create(true);
        assert!(!agent.is_null());
        poll_until(agent, |event| event["type"] == "ready");
        let context = unsafe { &*agent }
            .context
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .clone();
        let gate = context.search_gate.lock().unwrap();
        // All starts wait at the same gate. They can run in any order after release.
        for request in 1..=32 {
            let query = if request == 32 { "日本語" } else { "Safari" };
            let message =
                CString::new(json!({"op":"search","request":request,"query":query}).to_string())
                    .unwrap();
            assert!(unsafe { maccer_agent_send(agent, message.as_ptr()) }.is_null());
        }
        drop(gate);
        let event = poll_until(agent, |event| {
            event["type"] == "results" && event["request"] == 32 && event["complete"] == true
        });
        assert_eq!(event["items"][0]["title"], "日本語");
        unsafe { maccer_agent_destroy(agent) };
    }

    #[test]
    fn fixture_config_ignores_user_configuration_and_clipboard_baseline_is_not_capture() {
        let config = load_agent_config(true).unwrap();
        assert_eq!(config.launcher.hotkey, Config::default().launcher.hotkey);
        assert!(!clipboard_changed(None, 10));
        assert!(!clipboard_changed(Some(10), 10));
        assert!(clipboard_changed(Some(10), 11));
    }

    #[test]
    fn fixture_registry_searches_and_has_safe_actions() {
        let (registry, _, _, _) = make_registry(true).unwrap();
        let provider = registry.get(&ProviderId("apps".into())).unwrap();
        let result = provider
            .search(
                &crate::core::SearchQuery {
                    raw: "safari".into(),
                    text: "safari".into(),
                },
                &crate::core::SearchContext {
                    generation: 1,
                    cancellation: CancellationToken::default(),
                    limit: 10,
                },
            )
            .unwrap();
        assert_eq!(result[0].title, "Safari");
        assert_eq!(provider.actions(&result[0]).len(), 2);
    }
    fn poll_until(agent: *mut MaccerAgent, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let raw = unsafe { maccer_agent_poll(agent) };
            if !raw.is_null() {
                let text = unsafe { CStr::from_ptr(raw) }
                    .to_string_lossy()
                    .into_owned();
                unsafe { maccer_agent_string_free(raw) };
                let event: Value = serde_json::from_str(&text).unwrap();
                if predicate(&event) {
                    return event;
                }
            } else {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        panic!("timed out waiting for fixture event")
    }

    #[test]
    fn fixture_abi_searches_and_activates_without_native_side_effects() {
        let agent = maccer_agent_create(true);
        assert!(!agent.is_null());
        let query =
            std::ffi::CString::new(r#"{"op":"search","request":41,"query":"safari"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, query.as_ptr()) }.is_null());
        poll_until(agent, |event| event["type"] == "ready");
        let results = poll_until(agent, |event| event["type"] == "results");
        assert_eq!(results["request"], 41);
        assert_eq!(results["items"][0]["title"], "Safari");
        let activation = std::ffi::CString::new(
            r#"{"op":"activate","request":42,"search_request":41,"provider":"apps","id":"safari","action":"open"}"#,
        )
        .unwrap();
        assert!(unsafe { maccer_agent_send(agent, activation.as_ptr()) }.is_null());
        let outcome = poll_until(agent, |event| event["type"] == "outcome");
        assert_eq!(outcome["kind"], "close");
        assert_eq!(outcome["focusTransferred"], false);
        unsafe { maccer_agent_destroy(agent) };
    }

    #[test]
    fn activation_requires_current_result_generation_and_cancel_invalidates_results() {
        let agent = maccer_agent_create(true);
        assert!(!agent.is_null());
        poll_until(agent, |event| event["type"] == "ready");
        let search =
            std::ffi::CString::new(r#"{"op":"search","request":1,"query":"safari"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, search.as_ptr()) }.is_null());
        let result = poll_until(agent, |event| {
            event["type"] == "results" && event["request"] == 1
        });
        assert_eq!(result["items"][0]["id"], "safari");

        let stale = std::ffi::CString::new(r#"{"op":"activate","request":2,"search_request":0,"provider":"apps","id":"safari","action":"open"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, stale.as_ptr()) }.is_null());
        let error = poll_until(agent, |event| event["type"] == "error");
        assert_eq!(error["message"], "selected results are stale");

        let next =
            std::ffi::CString::new(r#"{"op":"search","request":3,"query":"Terminal"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, next.as_ptr()) }.is_null());
        poll_until(agent, |event| {
            event["type"] == "results" && event["request"] == 3
        });
        let stale_result = std::ffi::CString::new(r#"{"op":"activate","request":4,"search_request":1,"provider":"apps","id":"safari","action":"open"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, stale_result.as_ptr()) }.is_null());
        let error = poll_until(agent, |event| event["type"] == "error");
        assert_eq!(error["message"], "selected results are stale");

        let cancel = std::ffi::CString::new(r#"{"op":"cancel"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, cancel.as_ptr()) }.is_null());
        let canceled = std::ffi::CString::new(r#"{"op":"activate","request":6,"search_request":3,"provider":"apps","id":"terminal","action":"open"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, canceled.as_ptr()) }.is_null());
        let error = poll_until(agent, |event| event["type"] == "error");
        assert_eq!(error["message"], "selected results are stale");
        unsafe { maccer_agent_destroy(agent) };
    }

    #[test]
    fn fixture_output_and_keep_open_outcomes_are_safe_and_do_not_launch() {
        let agent = maccer_agent_create(true);
        poll_until(agent, |event| event["type"] == "ready");
        let search =
            std::ffi::CString::new(r#"{"op":"search","request":1,"query":"!fixture"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, search.as_ptr()) }.is_null());
        let results = poll_until(agent, |event| event["type"] == "results");
        let fixture = results["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["id"] == "fixture-actions")
            .unwrap();
        assert_eq!(fixture["actions"].as_array().unwrap().len(), 2);
        let activate = std::ffi::CString::new(r#"{"op":"activate","request":2,"search_request":1,"provider":"fixture","id":"fixture-actions","action":"output"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, activate.as_ptr()) }.is_null());
        let outcome = poll_until(agent, |event| event["type"] == "outcome");
        assert_eq!(outcome["kind"], "output");
        assert_eq!(outcome["text"], "<b>literal output</b>");
        let keep = std::ffi::CString::new(r#"{"op":"activate","request":3,"search_request":1,"provider":"fixture","id":"fixture-actions","action":"keep"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, keep.as_ptr()) }.is_null());
        let outcome = poll_until(agent, |event| event["type"] == "outcome");
        assert_eq!(outcome["kind"], "keepOpen");
        assert_eq!(outcome["text"], "Fixture action completed");
        unsafe { maccer_agent_destroy(agent) };
    }

    #[test]
    fn destroy_during_initialization_discards_queued_search() {
        let agent = maccer_agent_create(true);
        assert!(!agent.is_null());
        let search =
            std::ffi::CString::new(r#"{"op":"search","request":1,"query":"safari"}"#).unwrap();
        assert!(unsafe { maccer_agent_send(agent, search.as_ptr()) }.is_null());
        unsafe { maccer_agent_destroy(agent) };
    }

    #[test]
    fn fixture_command_rejects_bad_and_oversized_inputs() {
        let agent = make_agent(true, String::new(), None).unwrap();
        let err = unsafe {
            maccer_agent_send(
                &agent as *const _ as *mut _,
                std::ffi::CString::new("{}").unwrap().as_ptr(),
            )
        };
        assert!(!err.is_null());
        unsafe {
            maccer_agent_string_free(err);
        }
        let long = format!(
            "{{\"op\":\"search\",\"request\":1,\"query\":\"{}\"}}",
            "x".repeat(MAX_COMMAND_BYTES)
        );
        let c = std::ffi::CString::new(long).unwrap();
        let err = unsafe { maccer_agent_send(&agent as *const _ as *mut _, c.as_ptr()) };
        assert!(!err.is_null());
        unsafe {
            maccer_agent_string_free(err);
        }
    }
}
