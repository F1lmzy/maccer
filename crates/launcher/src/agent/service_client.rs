use crate::agent::{IconCache, PixelIcon, event_error};
use crate::client::Client;
use crate::protocol::{
    AssetRef, ContentBlock, GetDetailsParams, Icon, InvokeActionParams, Presentation, Route,
    ViewSnapshot,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

const COMMAND_CAPACITY: usize = 16;
const RESULT_LIMIT: usize = 100;
const MAX_ICONS_PER_VIEW: usize = 8;
/// Read-only active theme state is polled at most this often. Bounded so a
/// steady stream of searches cannot starve state, and cheap because the
/// service answers from an in-memory snapshot.
const THEME_POLL_INTERVAL: Duration = Duration::from_millis(1000);

pub(crate) enum IpcCommand {
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
        confirmed: bool,
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

pub(crate) struct IpcHandle {
    sender: SyncSender<IpcCommand>,
    latest_request: Arc<AtomicU64>,
    latest_details: Arc<AtomicU64>,
    destroyed: Arc<AtomicBool>,
}
impl IpcHandle {
    pub(crate) fn start(
        path: &Path,
        events: SyncSender<Value>,
        icon_cache: Arc<IconCache>,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(COMMAND_CAPACITY);
        let latest_request = Arc::new(AtomicU64::new(0));
        let destroyed = Arc::new(AtomicBool::new(false));
        let latest_details = Arc::new(AtomicU64::new(0));
        let worker_latest = latest_request.clone();
        let worker_details = latest_details.clone();
        let worker_destroyed = destroyed.clone();
        let path = path.to_owned();
        thread::Builder::new()
            .name("maccer-service-client".into())
            .spawn(move || {
                run(
                    path,
                    receiver,
                    events,
                    icon_cache,
                    worker_latest,
                    worker_details,
                    worker_destroyed,
                );
            })?;
        Ok(Self {
            sender,
            latest_request,
            latest_details,
            destroyed,
        })
    }

    pub(crate) fn send(&self, command: IpcCommand) -> Result<(), String> {
        if self.destroyed.load(Ordering::Acquire) {
            return Err("service client is closed".into());
        }
        if let IpcCommand::Search { query, .. } = &command
            && query.len() > 4096
        {
            return Err("query exceeds 4096 bytes".into());
        }
        match &command {
            IpcCommand::Search { request, .. }
            | IpcCommand::Activate { request, .. }
            | IpcCommand::Refresh { request, .. } => {
                let previous = self.latest_request.load(Ordering::Acquire);
                if *request <= previous {
                    return Ok(());
                }
                self.latest_request.store(*request, Ordering::Release);
            }
            IpcCommand::Cancel => {
                self.latest_request.fetch_add(1, Ordering::AcqRel);
            }
            IpcCommand::Home { request } => {
                let previous = self.latest_request.load(Ordering::Acquire);
                if *request <= previous {
                    return Ok(());
                }
                self.latest_request.store(*request, Ordering::Release);
            }
            IpcCommand::Details { request, .. } => {
                let previous = self.latest_details.load(Ordering::Acquire);
                if *request <= previous {
                    return Ok(());
                }
                self.latest_details.store(*request, Ordering::Release);
            }
        }
        self.sender.try_send(command).map_err(|err| match err {
            mpsc::TrySendError::Full(_) => "service command queue is full".to_owned(),
            mpsc::TrySendError::Disconnected(_) => "service client is disconnected".to_owned(),
        })
    }

    pub(crate) fn invalidate(&self) {
        self.destroyed.store(true, Ordering::Release);
        self.latest_request.fetch_add(1, Ordering::AcqRel);
        // Drop sender when MaccerAgent is dropped; worker checks `destroyed` after
        // every bounded RPC. No join occurs on the frontend thread.
    }
}

struct PendingIcon {
    request: u64,
    provider: String,
    id: String,
    asset: AssetRef,
}

/// Active theme state publication. State is independent of the search/action
/// request sequence: it is never filtered by `latest`, only by `destroyed`.
struct ThemePoll {
    enabled: bool,
    next_at: Instant,
    /// Revision most recently delivered to the frontend queue.
    published: Option<String>,
    /// Revision plus event waiting for room in a full frontend queue.
    pending: Option<(String, Value)>,
}
impl ThemePoll {
    fn new(enabled: bool) -> Self {
        Self {
            enabled,
            next_at: Instant::now(),
            published: None,
            pending: None,
        }
    }

    /// Flush a pending event, poll for a changed revision when due, then flush
    /// again. Called at the top of the actor loop so queries cannot starve state.
    fn tick(&mut self, client: &mut Client, events: &SyncSender<Value>) {
        self.flush(events);
        if !self.enabled || self.pending.is_some() || Instant::now() < self.next_at {
            return;
        }
        self.next_at = Instant::now() + THEME_POLL_INTERVAL;
        // Only a genuine transport/validation error skips an update; the last
        // good revision stays published and is retried on the next interval.
        if let Ok(state) = client.theme_state() {
            let revision = state.revision.clone();
            if self.published.as_deref() != Some(&revision) {
                self.pending = Some((revision, json!({"type":"themeState","state":state})));
                self.flush(events);
            }
        }
    }

    /// A full queue retains the update for a later attempt so the revision is
    /// only advanced once the frontend can actually observe it.
    fn flush(&mut self, events: &SyncSender<Value>) {
        let Some((revision, event)) = self.pending.take() else {
            return;
        };
        match events.try_send(event) {
            Ok(()) => self.published = Some(revision),
            Err(mpsc::TrySendError::Full(event)) => self.pending = Some((revision, event)),
            Err(mpsc::TrySendError::Disconnected(_)) => {}
        }
    }
}

fn run(
    path: std::path::PathBuf,
    receiver: mpsc::Receiver<IpcCommand>,
    events: SyncSender<Value>,
    cache: Arc<IconCache>,
    latest: Arc<AtomicU64>,
    latest_details: Arc<AtomicU64>,
    destroyed: Arc<AtomicBool>,
) {
    let mut client = match Client::connect(&path) {
        Ok(client) => client,
        Err(error) => {
            let message = format!("{error:#}");
            event_error(&events, None, &message);
            while !destroyed.load(Ordering::Acquire) {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(IpcCommand::Search { request, .. })
                    | Ok(IpcCommand::Activate { request, .. })
                    | Ok(IpcCommand::Refresh { request, .. }) => {
                        event_if_current(&events, &destroyed, &latest, request, message.clone())
                    }
                    Ok(IpcCommand::Details { search_request, .. }) => event_if_current(
                        &events,
                        &destroyed,
                        &latest,
                        search_request,
                        message.clone(),
                    ),
                    Ok(IpcCommand::Cancel | IpcCommand::Home { .. }) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            return;
        }
    };
    let has_icons = client
        .hello()
        .capabilities
        .iter()
        .any(|value| value == "iconAssetsBgra8");
    let has_details = client
        .hello()
        .capabilities
        .iter()
        .any(|value| value == "itemDetails");
    let has_theme = client
        .hello()
        .capabilities
        .iter()
        .any(|value| value == "themeState");
    let _ = events.try_send(
        json!({"type":"ready","shortcutError":"","itemDetails":has_details,"themeState":has_theme}),
    );
    let mut theme = ThemePoll::new(has_theme);
    let mut route = Route::Home;
    let mut view: Option<ViewSnapshot> = None;
    // Actor-owned: later input changes `latest`, but must not invalidate the
    // snapshot before an already-accepted activation is handled.
    let mut snapshot_request: Option<u64> = None;
    let mut pending_icons = VecDeque::<PendingIcon>::new();
    let mut deferred = None;
    let mut recovery_attempted = false;

    loop {
        if destroyed.load(Ordering::Acquire) {
            break;
        }
        theme.tick(&mut client, &events);
        let received = if let Some(command) = deferred.take() {
            Ok(command)
        } else {
            receiver.recv_timeout(if pending_icons.is_empty() {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(2)
            })
        };
        match received {
            Ok(command) => {
                let mut selected = command;
                // Collapse only adjacent queued searches. Never skip an activation or
                // cancellation that was already accepted by the C ABI.
                if matches!(&selected, IpcCommand::Search { .. }) {
                    loop {
                        match receiver.try_recv() {
                            Ok(next @ IpcCommand::Search { .. }) => selected = next,
                            Ok(next) => {
                                deferred = Some(next);
                                break;
                            }
                            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                        }
                    }
                }
                if !matches!(selected, IpcCommand::Details { .. }) {
                    pending_icons.clear();
                }
                if destroyed.load(Ordering::Acquire) {
                    break;
                }
                match selected {
                    IpcCommand::Search { request, query } => {
                        recovery_attempted = false;
                        // A queued search that was superseded before it reached the
                        // socket must not advance the service view revision and stale
                        // the snapshot currently displayed by the frontend.
                        if latest.load(Ordering::Acquire) != request {
                            continue;
                        }
                        let result = match &view {
                            Some(active) => client.query_view(&active.view_id, &query),
                            None => client.open_view(route.clone(), &query, RESULT_LIMIT),
                        };
                        match result {
                            Ok(snapshot) => {
                                route = snapshot.route.clone();
                                if latest.load(Ordering::Acquire) != request
                                    || destroyed.load(Ordering::Acquire)
                                {
                                    continue;
                                }
                                let (rows, icons) = project(&snapshot, request, has_icons);
                                let errors: Vec<String> = snapshot
                                    .errors
                                    .iter()
                                    .map(|error| {
                                        format!("{}: {}", error.provider_id, error.message)
                                    })
                                    .collect();
                                let event = json!({"type":"results","request":request,"items":rows,"complete":true,"errors":errors,"route":route_json(&route)});
                                if send_current_event(&events, &destroyed, &latest, request, event)
                                {
                                    // Only authorize activation from a snapshot that
                                    // was actually delivered to this frontend.
                                    snapshot_request = Some(request);
                                    view = Some(snapshot);
                                    pending_icons.extend(icons);
                                } else {
                                    view = None;
                                }
                            }
                            Err(error) => event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                format!("{error:#}"),
                            ),
                        }
                    }
                    IpcCommand::Details {
                        request,
                        search_request,
                        provider,
                        id,
                    } => {
                        if latest_details.load(Ordering::Acquire) != request
                            || latest.load(Ordering::Acquire) != search_request
                            || snapshot_request != Some(search_request)
                            || destroyed.load(Ordering::Acquire)
                        {
                            continue;
                        }
                        let Some(snapshot) = view.as_ref() else {
                            continue;
                        };
                        let item_available = snapshot
                            .sections
                            .iter()
                            .flat_map(|section| &section.items)
                            .any(|item| {
                                item.provider_id == provider
                                    && item.id == id
                                    && item.details_available
                            });
                        if !has_details || !item_available {
                            let _ = send_current_event(
                                &events,
                                &destroyed,
                                &latest,
                                search_request,
                                json!({"type":"details","request":search_request,"detailRequest":request,"provider":provider,"id":id,
                                    "content":[{"type":"text","text":"No preview available","truncated":false,"fallbackText":"No preview available"}]}),
                            );
                            continue;
                        }
                        let params = GetDetailsParams {
                            view_id: snapshot.view_id.clone(),
                            revision: snapshot.revision.clone(),
                            provider_id: provider.clone(),
                            item_id: id.clone(),
                        };
                        if let Ok(details) = client.get_details(&params) {
                            if latest_details.load(Ordering::Acquire) != request
                                || latest.load(Ordering::Acquire) != search_request
                                || snapshot_request != Some(search_request)
                                || destroyed.load(Ordering::Acquire)
                            {
                                continue;
                            }
                            let mut content: Vec<Value> = details.content.iter().map(|block| serde_json::to_value(block).unwrap_or_else(|_| json!({"type":"unknown","fallbackText":block.fallback_text()}))).collect();
                            let mut fetched = 0;
                            for (block, encoded) in details.content.iter().zip(content.iter_mut()) {
                                let ContentBlock::Image { asset, .. } = block else {
                                    continue;
                                };
                                if fetched >= 4
                                    || !has_icons
                                    || !asset.representations.iter().any(|r| r == "bgra8")
                                {
                                    continue;
                                }
                                fetched += 1;
                                match client.get_asset(asset) {
                                    Ok(image) => {
                                        if latest_details.load(Ordering::Acquire) != request
                                            || latest.load(Ordering::Acquire) != search_request
                                            || destroyed.load(Ordering::Acquire)
                                        {
                                            break;
                                        }
                                        let key = format!(
                                            "{}:{}",
                                            image.metadata.asset_id, image.metadata.revision
                                        );
                                        if let Some(token) = cache.insert(
                                            key,
                                            PixelIcon {
                                                bgra: image.bytes,
                                                width: image.metadata.width,
                                                height: image.metadata.height,
                                            },
                                        ) {
                                            encoded["source"] = Value::String(format!(
                                                "image://maccer-icons/{token}"
                                            ));
                                        }
                                    }
                                    Err(error) => {
                                        let code = error
                                            .downcast_ref::<crate::client::RemoteError>()
                                            .map(|remote| remote.0.data.code.as_str());
                                        if !recovery_attempted
                                            && matches!(code, Some("ASSET_EXPIRED" | "STALE_ASSET"))
                                        {
                                            recovery_attempted = true;
                                            let _ = send_current_event(
                                                &events,
                                                &destroyed,
                                                &latest,
                                                search_request,
                                                json!({"type":"assetExpired","request":search_request,"provider":provider,"id":id}),
                                            );
                                        }
                                    }
                                }
                            }
                            if latest_details.load(Ordering::Acquire) == request
                                && latest.load(Ordering::Acquire) == search_request
                                && snapshot_request == Some(search_request)
                                && !destroyed.load(Ordering::Acquire)
                            {
                                let _ = send_current_event(
                                    &events,
                                    &destroyed,
                                    &latest,
                                    search_request,
                                    json!({"type":"details","request":search_request,"detailRequest":request,"provider":provider,"id":id,"content":content}),
                                );
                            }
                        } else if latest_details.load(Ordering::Acquire) == request
                            && latest.load(Ordering::Acquire) == search_request
                            && !destroyed.load(Ordering::Acquire)
                        {
                            let _ = send_current_event(
                                &events,
                                &destroyed,
                                &latest,
                                search_request,
                                json!({"type":"details","request":search_request,"detailRequest":request,"provider":provider,"id":id,
                                    "content":[{"type":"text","text":"Preview unavailable","truncated":false,"fallbackText":"Preview unavailable"}]}),
                            );
                        }
                    }
                    IpcCommand::Refresh {
                        request,
                        search_request,
                    } => {
                        if latest.load(Ordering::Acquire) != request {
                            continue;
                        }
                        if snapshot_request != Some(search_request) {
                            event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                "asset refresh source snapshot is stale".into(),
                            );
                            continue;
                        }
                        let Some(active) = view.as_ref() else {
                            event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                "asset refresh has no active view".into(),
                            );
                            continue;
                        };
                        let result = client.query_view(&active.view_id, &active.query);
                        match result {
                            Ok(snapshot) => {
                                if latest.load(Ordering::Acquire) != request
                                    || destroyed.load(Ordering::Acquire)
                                {
                                    continue;
                                }
                                let (rows, icons) = project(&snapshot, request, has_icons);
                                let errors: Vec<String> = snapshot
                                    .errors
                                    .iter()
                                    .map(|error| {
                                        format!("{}: {}", error.provider_id, error.message)
                                    })
                                    .collect();
                                let event = json!({"type":"results","request":request,"items":rows,"complete":true,"errors":errors,"route":route_json(&snapshot.route),"recovery":true});
                                if send_current_event(&events, &destroyed, &latest, request, event)
                                {
                                    recovery_attempted = true;
                                    snapshot_request = Some(request);
                                    route = snapshot.route.clone();
                                    view = Some(snapshot);
                                    pending_icons.extend(icons);
                                }
                            }
                            Err(error) => event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                format!("{error:#}"),
                            ),
                        }
                    }
                    IpcCommand::Home { request } => {
                        route = Route::Home;
                        view = None;
                        snapshot_request = None;
                        let _ = send_current_event(
                            &events,
                            &destroyed,
                            &latest,
                            request,
                            json!({"type":"outcome","request":request,"kind":"setQuery","text":"","route":{"type":"home"}}),
                        );
                    }
                    IpcCommand::Cancel => {
                        view = None;
                        snapshot_request = None;
                        route = Route::Home;
                    }
                    IpcCommand::Activate {
                        request,
                        search_request,
                        provider,
                        id,
                        action,
                        confirmed,
                    } => {
                        // An accepted activation is a pending mutation. Newer input
                        // suppresses its UI outcome, not the already-selected action.
                        if destroyed.load(Ordering::Acquire) {
                            continue;
                        }
                        if snapshot_request != Some(search_request) {
                            event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                "selected results are stale".into(),
                            );
                            continue;
                        }
                        let Some(snapshot) = view.as_ref() else {
                            event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                "selected results are stale".into(),
                            );
                            continue;
                        };
                        let found = snapshot
                            .sections
                            .iter()
                            .flat_map(|section| &section.items)
                            .find(|item| item.provider_id == provider && item.id == id);
                        let Some(item) = found else {
                            event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                "selected result is stale".into(),
                            );
                            continue;
                        };
                        let Some(descriptor) = item.actions.iter().find(|entry| entry.id == action)
                        else {
                            event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                "unsupported action".into(),
                            );
                            continue;
                        };
                        let focus_transferred = provider == "apps"
                            && matches!(descriptor.id.as_str(), "open" | "reveal");
                        let params = InvokeActionParams {
                            view_id: snapshot.view_id.clone(),
                            revision: snapshot.revision.clone(),
                            action_ref: descriptor.action_ref.clone(),
                            operation_id: format!(
                                "agent-{}-{}",
                                client.hello().session_id,
                                request
                            ),
                            confirmed,
                        };
                        // Once sent, never retry this mutation. Disconnect/error can mean
                        // the service performed it but the response was lost.
                        match client.invoke(&params) {
                            Ok(operation) => {
                                if let Some(Presentation::Navigate { route: next }) =
                                    &operation.presentation
                                {
                                    // Navigation changes backend scope even when a newer
                                    // query superseded the notification. Process the next
                                    // queued query against this route.
                                    route = next.clone();
                                    view = None;
                                    snapshot_request = None;
                                }
                                if destroyed.load(Ordering::Acquire)
                                    || latest.load(Ordering::Acquire) != request
                                {
                                    continue;
                                }
                                if let Some(presentation) = operation.presentation {
                                    match presentation {
                                        Presentation::Navigate { route: next } => {
                                            let _ = send_current_event(
                                                &events,
                                                &destroyed,
                                                &latest,
                                                request,
                                                json!({"type":"outcome","request":request,"kind":"setQuery","text":"","route":route_json(&next)}),
                                            );
                                        }
                                        Presentation::Close => {
                                            let _ = send_current_event(
                                                &events,
                                                &destroyed,
                                                &latest,
                                                request,
                                                json!({"type":"outcome","request":request,"kind":"close","focusTransferred":focus_transferred}),
                                            );
                                        }
                                        Presentation::KeepOpen { message } => {
                                            let _ = send_current_event(
                                                &events,
                                                &destroyed,
                                                &latest,
                                                request,
                                                json!({"type":"outcome","request":request,"kind":"keepOpen","text":message}),
                                            );
                                        }
                                        Presentation::SetQuery { query } => {
                                            let _ = send_current_event(
                                                &events,
                                                &destroyed,
                                                &latest,
                                                request,
                                                json!({"type":"outcome","request":request,"kind":"setQuery","text":query}),
                                            );
                                        }
                                    }
                                } else {
                                    let text = operation
                                        .content
                                        .iter()
                                        .map(|block| block.fallback_text())
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    let _ = send_current_event(
                                        &events,
                                        &destroyed,
                                        &latest,
                                        request,
                                        json!({"type":"outcome","request":request,"kind":"keepOpen","text":text}),
                                    );
                                }
                            }
                            Err(error) => event_if_current(
                                &events,
                                &destroyed,
                                &latest,
                                request,
                                format!("{error:#}"),
                            ),
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !has_icons {
                    continue;
                }
                let Some(icon) = pending_icons.pop_front() else {
                    continue;
                };
                if latest.load(Ordering::Acquire) != icon.request
                    || destroyed.load(Ordering::Acquire)
                {
                    continue;
                }
                match client.get_asset(&icon.asset) {
                    Ok(asset)
                        if latest.load(Ordering::Acquire) == icon.request
                            && !destroyed.load(Ordering::Acquire) =>
                    {
                        let key =
                            format!("{}:{}", asset.metadata.asset_id, asset.metadata.revision);
                        let token = cache.insert(
                            key,
                            PixelIcon {
                                bgra: asset.bytes,
                                width: asset.metadata.width,
                                height: asset.metadata.height,
                            },
                        );
                        if let Some(token) = token {
                            let _ = events.try_send(json!({"type":"icon","request":icon.request,"provider":icon.provider,"id":icon.id,"source":format!("image://maccer-icons/{token}")}));
                        }
                    }
                    Err(error) => {
                        let code = error
                            .downcast_ref::<crate::client::RemoteError>()
                            .map(|remote| remote.0.data.code.as_str());
                        if !recovery_attempted
                            && matches!(code, Some("ASSET_EXPIRED" | "STALE_ASSET"))
                        {
                            recovery_attempted = true;
                            let _ = send_current_event(
                                &events,
                                &destroyed,
                                &latest,
                                icon.request,
                                json!({"type":"assetExpired","request":icon.request,"provider":icon.provider,"id":icon.id}),
                            );
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn project(
    snapshot: &ViewSnapshot,
    request: u64,
    fetch_icons: bool,
) -> (Vec<Value>, Vec<PendingIcon>) {
    let mut rows = Vec::new();
    let mut icons = Vec::new();
    for item in snapshot.sections.iter().flat_map(|section| &section.items) {
        let (icon_source, icon_text) = match &item.icon {
            Some(Icon::Glyph {
                name: _,
                fallback_text,
            }) => (String::new(), fallback_text.clone()),
            Some(Icon::Asset {
                asset,
                fallback_text,
            }) => {
                if fetch_icons
                    && icons.len() < MAX_ICONS_PER_VIEW
                    && asset.representations.iter().any(|r| r == "bgra8")
                {
                    icons.push(PendingIcon {
                        request,
                        provider: item.provider_id.clone(),
                        id: item.id.clone(),
                        asset: asset.clone(),
                    });
                }
                (String::new(), fallback_text.clone())
            }
            None => (
                String::new(),
                item.title.chars().next().unwrap_or('•').to_string(),
            ),
        };
        let actions: Vec<Value> = item
            .actions
            .iter()
            .map(|action| {
                let mut value = json!({"id":action.id,"title":action.title});
                if let Some(confirmation) = &action.confirmation {
                    value["confirmation"] = json!(confirmation);
                }
                value
            })
            .collect();
        let mut row = json!({"id":item.id,"provider":item.provider_id,"title":item.title,"subtitle":item.subtitle,"iconSource":icon_source,"iconText":icon_text,"actions":actions,"kind":item.kind,"content":item.content,"data":item.data,"revision":item.revision,"detailsAvailable":item.details_available});
        if let Some(object) = row.as_object_mut() {
            object.insert("route".into(), route_json(&snapshot.route));
        }
        rows.push(row);
    }
    (rows, icons)
}

fn route_json(route: &Route) -> Value {
    serde_json::to_value(route).unwrap_or_else(|_| json!({"type":"home"}))
}

// Results authorize actions only after delivery. Retry a full frontend queue for
// a bounded interval, while abandoning events that have become stale.
fn send_current_event(
    events: &SyncSender<Value>,
    destroyed: &AtomicBool,
    latest: &AtomicU64,
    request: u64,
    mut event: Value,
) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(250);
    while !destroyed.load(Ordering::Acquire)
        && latest.load(Ordering::Acquire) == request
        && std::time::Instant::now() < deadline
    {
        match events.try_send(event) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(next)) => {
                event = next;
                thread::sleep(Duration::from_millis(1));
            }
        }
    }
    false
}

fn event_if_current(
    events: &SyncSender<Value>,
    destroyed: &AtomicBool,
    latest: &AtomicU64,
    request: u64,
    message: String,
) {
    if !destroyed.load(Ordering::Acquire) && latest.load(Ordering::Acquire) == request {
        event_error(events, Some(request), message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_event_queue_does_not_authorize_an_unpublished_result_forever() {
        let (events, _receiver) = mpsc::sync_channel(0);
        let destroyed = AtomicBool::new(false);
        let latest = AtomicU64::new(7);
        let started = std::time::Instant::now();
        let sent = send_current_event(
            &events,
            &destroyed,
            &latest,
            7,
            json!({"type":"results","request":7}),
        );
        assert!(!sent);
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn theme_state_publication_waits_for_queue_room_before_advancing_revision() {
        let (events, receiver) = mpsc::sync_channel(0);
        let mut theme = ThemePoll::new(true);
        theme.pending = Some(("rev-1".into(), json!({"type":"themeState"})));
        // A rendezvous channel refuses the send, so the revision stays unpublished.
        theme.flush(&events);
        assert_eq!(theme.published, None);
        assert!(theme.pending.is_some());
        // A waiting receiver makes room; the retained update is delivered once.
        let received = thread::spawn(move || receiver.recv().unwrap());
        let deadline = Instant::now() + Duration::from_millis(500);
        while theme.published.is_none() && Instant::now() < deadline {
            theme.flush(&events);
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(theme.published.as_deref(), Some("rev-1"));
        assert!(theme.pending.is_none());
        assert_eq!(received.join().unwrap()["type"], "themeState");
    }
}
