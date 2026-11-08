use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::core::{
    Action, ActionId, ActionOutcome, CancellationToken, HistoryStore, Item, Preview,
    SearchCoordinator, UsageEvent,
};
use anyhow::{Context as _, Result};
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding, Render, ScrollHandle,
    ScrollStrategy, Subscription, UniformListScrollHandle, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, actions, div, prelude::*, px, size,
    uniform_list,
};

use crate::ui::{
    search_input::{self, SearchInput, SearchInputEvent},
    state::{
        EscapeResult, LauncherState, Screen, result_limit, selected_result_index,
        should_apply_activation, should_hide_after_deactivation, visible_result_count,
    },
    theme::{Theme, theme},
};

actions!(
    launcher,
    [
        MoveUp,
        MoveDown,
        Confirm,
        OpenActions,
        ProviderPicker,
        NextAction,
        CopyOutput,
        TogglePreview,
        Quit,
        Escape
    ]
);

const ROW_HEIGHT: f32 = 44.;
/// Rows and action buttons share one radius and icon slot so every screen
/// lines up on the same 8-point grid.
const ROW_RADIUS: f32 = 8.;
const ICON_SIZE: f32 = 24.;
/// Filmstrip geometry: exactly five frames — one focused, two shrinking on each
/// side — butt-joined with no gaps so the row reads as one continuous strip, and
/// as wide as the window so the last frame on each side ends at the edge. The
/// band keeps 16 of air above and below the tallest frame, so the focused frame
/// never touches the search rule or the footer.
struct StripGeometry {
    /// Height of the tallest (focused) frame.
    frame_h: f32,
    /// Frame width, height and centre offset by distance from the focused frame.
    /// Offset 0 is the focused frame; neighbouring frames start exactly where the
    /// previous one ends, which is what removes the gaps.
    slot_w: [f32; 3],
    slot_h: [f32; 3],
    slot_cx: [f32; 3],
    /// How much of a wallpaper survives at distance. Palette cards keep their
    /// dots legible at every distance, so they use 1.
    bright: [f32; 3],
}
const STRIP_INSET: f32 = 16.;
/// Window height = 2 border + 52 bar + band + 22 footer.
const STRIP_CHROME: f32 = 76.;
const WALLPAPER_STRIP: StripGeometry = StripGeometry {
    frame_h: 226.,
    slot_w: [320., 190., 142.],
    slot_h: [226., 160., 120.],
    slot_cx: [0., 255., 421.],
    bright: [1., 0.72, 0.52],
};
const THEME_STRIP: StripGeometry = StripGeometry {
    frame_h: 176.,
    slot_w: [248., 146., 110.],
    slot_h: [176., 124., 92.],
    slot_cx: [0., 197., 325.],
    bright: [1., 1., 1.],
};
/// Palette card metrics by distance. Kept here rather than inside the tile so
/// every frame lands its dots on the same line of the band.
const THEME_DOTS: [f32; 3] = [12., 8., 6.];
const THEME_DOT_GAP: [f32; 3] = [8., 4., 4.];
const THEME_DOTS_TOP: [f32; 3] = [72., 48., 32.];
const THEME_NAME: [f32; 3] = [13., 11., 11.];
const THEME_NAME_BOTTOM: [f32; 3] = [32., 24., 24.];
const THEME_NAME_PAD: [f32; 3] = [16., 12., 12.];

impl StripGeometry {
    /// The strip is exactly as wide as its frames: 984 for wallpapers, 760 for
    /// palette cards.
    fn width(&self) -> f32 {
        let last = self.slot_w.len() - 1;
        2. * self.slot_cx[last] + self.slot_w[last]
    }
    fn band(&self) -> f32 {
        self.frame_h + STRIP_INSET * 2.
    }
    fn window_height(&self) -> f32 {
        self.band() + STRIP_CHROME
    }
}
/// Only the two frames on each side of the focused one are drawn.
const FILMSTRIP_WINDOW: f32 = 2.;
/// Wallpaper thumbnails held as decoded images before the oldest is dropped.
const MAX_PREVIEW_CACHE: usize = 32;
/// Frame time constant for the strip chase. Same feel as the reference.
const FILMSTRIP_CHASE: f32 = 0.07;
const MAX_ICON_LOADS: usize = 4;
const MAX_SERVICE_ICON_CACHE: usize = 256;
const PREVIEW_WIDTH: f32 = 280.;
const FOOTER_HEIGHT: f32 = 22.;
const MAX_WINDOW_HEIGHT: f32 = 400.;
/// The launcher grows while the native web preview covers the body.
const WEB_PREVIEW_HEIGHT: f32 = 560.;
/// Delay between asking the paste target to activate and sending Command-V.
const PASTE_DELAY_MS: u64 = 100;

/// Native operations used to paste into another application. Tests replace
/// these so they never touch the system clipboard or synthesize keystrokes.
type FrontmostFn = Arc<dyn Fn() -> Option<i32> + Send + Sync>;
type PrepareFn = Arc<dyn Fn(&str, i32) -> Result<()> + Send + Sync>;
type SendFn = Arc<dyn Fn(i32) -> Result<()> + Send + Sync>;

struct PasteBackend {
    frontmost: FrontmostFn,
    prepare: PrepareFn,
    send: SendFn,
}

impl Default for PasteBackend {
    fn default() -> Self {
        Self {
            frontmost: Arc::new(crate::macos::clipboard::frontmost_application_pid),
            prepare: Arc::new(crate::macos::clipboard::prepare_paste),
            send: Arc::new(crate::macos::clipboard::send_paste),
        }
    }
}

pub struct LauncherOptions {
    pub width: f32,
    pub max_results: usize,
    pub web: WebPreviewOptions,
}

impl Default for LauncherOptions {
    fn default() -> Self {
        Self {
            width: 480.0,
            max_results: 50,
            web: WebPreviewOptions::default(),
        }
    }
}

/// How the launcher uses the native `WKWebView` panel.
#[derive(Clone, Debug)]
pub struct WebPreviewOptions {
    /// Open web results in the native panel instead of the default browser.
    pub inline: bool,
    /// Create the view and load `about:blank` while the selection is a web
    /// result. This removes WebKit process start-up from the frame where the
    /// user presses Enter and costs its resident memory only while a web
    /// result is selected. See `docs/web-preview-performance.md`.
    pub prewarm: bool,
    /// How long a hidden page is kept before its document is released. Zero
    /// keeps the document until the launcher exits.
    pub idle_release: std::time::Duration,
}

impl Default for WebPreviewOptions {
    fn default() -> Self {
        Self {
            inline: true,
            prewarm: true,
            idle_release: std::time::Duration::from_secs(300),
        }
    }
}

impl WebPreviewOptions {
    /// Prewarming earns its memory only when the selected row is the web
    /// result the user is about to open. App, file and clipboard selections
    /// never start WebKit.
    pub fn should_prewarm(&self, selected: Option<&Item>) -> bool {
        self.inline && self.prewarm && selected.is_some_and(|item| item.provider.0 == "web")
    }
}

/// A provider-published field waiting for the user before its action may run.
///
/// The item and action travel with the field so the collected value returns to
/// exactly the action that asked for it, and nothing is sent while the dialog
/// is open.
/// One row of the action panel: an action and the subject it runs against. A
/// scope command's subject is the provider's own identity for it, so choosing
/// one never runs against whatever happens to be selected.
#[derive(Clone, Debug)]
struct LauncherAction {
    item: Item,
    action: Action,
}

struct PendingInput {
    item: Item,
    action: Action,
    field: crate::ui::service_backend::ServiceActionInput,
    /// Chosen option for a select field; always 0 for text.
    index: usize,
}

pub struct Launcher {
    coordinator: Option<Arc<SearchCoordinator>>,
    history: Option<Arc<HistoryStore>>,
    service: Option<crate::ui::service_backend::ServiceBackend>,
    options: LauncherOptions,
    focus_handle: FocusHandle,
    search_input: Entity<SearchInput>,
    /// The text field a provider-published `text` input is typed into. It is
    /// only focused and painted while a dialog is pending.
    dialog_input: Entity<SearchInput>,
    /// The provider dialog currently waiting for the user, if any.
    pending_input: Option<PendingInput>,
    /// True while the dialog's field owned the keyboard, so closing it hands
    /// typing back to the search field exactly once.
    dialog_was_focused: bool,
    state: LauncherState,
    /// Last empty-query home snapshot. Reopening paints this immediately while
    /// a replacement is fetched, so the home list never flashes through the
    /// empty "Searching…" state.
    home_items: Option<Vec<Item>>,
    actions: Vec<LauncherAction>,
    action_item: Option<Item>,
    activation_serial: u64,
    activation_in_flight: Option<u64>,
    activation_cancellation: Option<CancellationToken>,
    search_cancellation: Option<CancellationToken>,
    error: Option<String>,
    output: Option<(String, String)>,
    output_scroll: ScrollHandle,
    list_scroll: UniformListScrollHandle,
    activation_subscription: Option<Subscription>,
    was_active: bool,
    preview_key: Option<(String, String, u64)>,
    preview: Option<Preview>,
    preview_image: Option<Arc<gpui::RenderImage>>,
    preview_images: crate::ui::preview_image::PreviewImageCache,
    preview_loading_visible: bool,
    preview_cancellation: Option<CancellationToken>,
    preview_started: Option<std::time::Instant>,
    preview_span: Option<tracing::Span>,
    preview_applied_at: Option<std::time::Instant>,
    preview_scroll: ScrollHandle,
    preview_enabled: bool,
    /// Active palette for this window. Starts as the embedded default and is
    /// replaced only by validated service state owned by this launcher.
    theme: Theme,
    /// A stale/missing service palette, kept apart from query and paste errors.
    theme_error: Option<String>,
    presentation_error: Option<String>,
    drag_start: Option<(Item, gpui::Point<gpui::Pixels>)>,
    icon_cache: HashMap<PathBuf, Arc<gpui::RenderImage>>,
    /// Decoded wallpaper thumbnails, keyed by `image://maccer-previews/` source.
    preview_cache: HashMap<PathBuf, Arc<gpui::RenderImage>>,
    /// Insertion order for `preview_cache`, so eviction drops the oldest
    /// thumbnail instead of an arbitrary one the strip may still be painting.
    preview_order: VecDeque<PathBuf>,
    /// Animated strip position chasing the selected index, and the pointer
    /// accumulators that turn wheel/trackpad deltas into single steps.
    filmstrip_pos: f32,
    /// Where the strip is heading, in continuous frames. Never wrapped, so
    /// stepping past either end keeps rotating the same way.
    filmstrip_target: f32,
    filmstrip_wheel: f32,
    icon_failed: HashSet<PathBuf>,
    /// Native calls are not interruptible. Keep at most four active across query
    /// changes, and never queue a second load for the same bundle.
    icon_loading: HashSet<PathBuf>,
    /// Actual viewport reported by the uniform list, tagged with its query.
    icon_viewport: Option<(u64, std::ops::Range<usize>)>,
    /// PID of the application that was frontmost before the launcher opened.
    paste_target: Option<i32>,
    /// A post-dismissal failure must survive ordinary search updates on reopen.
    paste_error: Option<String>,
    /// Pending delayed Command-V. Cancelled when the launcher reopens or the
    /// query changes so it can never paste into a later, unintended action.
    paste_cancellation: Option<CancellationToken>,
    paste_backend: Option<PasteBackend>,
    /// Ask the window platform to hide the window. gpui's test platform does
    /// not implement `Platform::hide`, so tests set this to false to exercise
    /// the rest of the hide path without the unsupported call.
    hide_window: bool,
    /// Native AppKit host captured from the GPUI window. The web view itself
    /// is created lazily so ordinary searches never load WebKit.
    web_host: Option<crate::macos::WebHost>,
    web_preview: Option<crate::macos::WebPreview>,
    /// The pending idle release; dropping it cancels the release.
    web_preview_idle: Option<gpui::Task<()>>,
}

pub fn open_launcher(
    coordinator: Arc<SearchCoordinator>,
    history: Arc<HistoryStore>,
    options: LauncherOptions,
    cx: &mut App,
) -> Result<WindowHandle<Launcher>> {
    open_launcher_backend(Some(coordinator), Some(history), None, options, cx)
}

/// Open a presentation-only client for an already-running service.
/// Connection failures arrive asynchronously and appear in the launcher.
/// No embedded backend, service startup, fallback or automatic retry is performed.
pub fn open_service_launcher(
    socket: &std::path::Path,
    options: LauncherOptions,
    cx: &mut App,
) -> Result<WindowHandle<Launcher>> {
    let service = crate::ui::service_backend::ServiceBackend::connect(socket)?;
    open_launcher_backend(None, None, Some(service), options, cx)
}

fn open_launcher_backend(
    coordinator: Option<Arc<SearchCoordinator>>,
    history: Option<Arc<HistoryStore>>,
    service: Option<crate::ui::service_backend::ServiceBackend>,
    options: LauncherOptions,
    cx: &mut App,
) -> Result<WindowHandle<Launcher>> {
    search_input::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("down", MoveDown, Some("Launcher")),
        KeyBinding::new("ctrl-n", MoveDown, Some("Launcher")),
        KeyBinding::new("up", MoveUp, Some("Launcher")),
        KeyBinding::new("ctrl-p", MoveUp, Some("Launcher")),
        // The search field owns plain left/right for the caret, so the
        // filmstrip steps with alt-arrows as well as up/down.
        KeyBinding::new("alt-left", MoveUp, Some("Launcher")),
        KeyBinding::new("alt-right", MoveDown, Some("Launcher")),
        // The wallpaper strip reads left/right as "previous/next wallpaper".
        // The input node swaps to this deeper context only in strip mode, so
        // the caret keeps the plain arrows everywhere else.
        KeyBinding::new("left", MoveUp, Some("Filmstrip")),
        KeyBinding::new("right", MoveDown, Some("Filmstrip")),
        KeyBinding::new("enter", Confirm, Some("Launcher")),
        KeyBinding::new("cmd-enter", OpenActions, Some("Launcher")),
        KeyBinding::new("cmd-k", OpenActions, Some("Launcher")),
        KeyBinding::new("cmd-p", ProviderPicker, Some("Launcher")),
        KeyBinding::new("tab", NextAction, Some("Launcher")),
        KeyBinding::new("escape", Escape, Some("Launcher")),
        KeyBinding::new("cmd-c", CopyOutput, Some("LauncherOutput")),
        KeyBinding::new("cmd-shift-p", TogglePreview, Some("Launcher")),
        KeyBinding::new("cmd-q", Quit, None),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
    let bounds = Bounds::centered(None, size(px(options.width), px(360.)), cx);
    let paste_backend = coordinator.as_ref().map(|_| PasteBackend::default());
    let service_mode = service.is_some();
    let (input_tx, input_rx) = async_channel::unbounded();
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                // Solid: the popup never lets the desktop through.
                window_background: WindowBackgroundAppearance::Opaque,
                focus: false,
                show: false,
                kind: WindowKind::PopUp,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                ..Default::default()
            },
            move |_, cx| {
                let search_input = cx.new(|cx| SearchInput::new(cx, input_tx));
                // The dialog's field has its own channel; its events are not
                // used, because a value is read once on submit.
                let (dialog_tx, _dialog_rx) = async_channel::unbounded();
                let dialog_input = cx.new(|cx| SearchInput::new(cx, dialog_tx));
                let launcher = cx.new(|cx| Launcher {
                    coordinator,
                    history,
                    service,
                    options,
                    focus_handle: cx.focus_handle(),
                    search_input,
                    dialog_input,
                    pending_input: None,
                    dialog_was_focused: false,
                    state: LauncherState::default(),
                    home_items: None,
                    actions: vec![],
                    action_item: None,
                    activation_serial: 0,
                    activation_in_flight: None,
                    activation_cancellation: None,
                    search_cancellation: None,
                    error: None,
                    output: None,
                    output_scroll: ScrollHandle::new(),
                    list_scroll: UniformListScrollHandle::new(),
                    activation_subscription: None,
                    was_active: false,
                    preview_key: None,
                    preview: None,
                    preview_image: None,
                    preview_images: Default::default(),
                    preview_loading_visible: false,
                    preview_cancellation: None,
                    preview_started: None,
                    preview_span: None,
                    preview_applied_at: None,
                    preview_scroll: ScrollHandle::new(),
                    preview_enabled: true,
                    theme: theme(),
                    theme_error: None,
                    presentation_error: None,
                    drag_start: None,
                    icon_cache: HashMap::new(),
                    preview_cache: HashMap::new(),
                    preview_order: VecDeque::new(),
                    filmstrip_pos: 0.,
                    filmstrip_target: 0.,
                    filmstrip_wheel: 0.,
                    icon_failed: HashSet::new(),
                    icon_loading: HashSet::new(),
                    icon_viewport: None,
                    paste_target: None,
                    paste_error: None,
                    paste_cancellation: None,
                    paste_backend,
                    hide_window: true,
                    web_host: None,
                    web_preview: None,
                    web_preview_idle: None,
                });
                let refresh = launcher.downgrade();
                let mut revisions: Vec<(String, u64)> = Vec::new();
                cx.spawn(async move |cx| {
                    loop {
                        gpui::Timer::after(std::time::Duration::from_millis(500)).await;
                        if refresh
                            .update(cx, |this, cx| {
                                let Some(coordinator) = &this.coordinator else {
                                    return;
                                };
                                let current: Vec<_> = coordinator
                                    .registry
                                    .descriptors()
                                    .iter()
                                    .filter_map(|d| {
                                        coordinator
                                            .registry
                                            .get(&d.id)
                                            .map(|p| (d.id.0.clone(), p.revision()))
                                    })
                                    .collect();
                                if current != revisions {
                                    revisions = current;
                                    if this.state.screen == Some(Screen::Search) {
                                        this.start_search(cx);
                                    }
                                }
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
                let weak = launcher.downgrade();
                cx.spawn(async move |cx| {
                    while let Ok(mut event) = input_rx.recv().await {
                        while let Ok(latest) = input_rx.try_recv() {
                            event = latest;
                        }
                        let result = weak.update(cx, |this, cx| {
                            match event {
                                SearchInputEvent::Changed(query) => {
                                    // The editor already holds this edit. Do not echo queued
                                    // text back and reset the caret or IME composition.
                                    this.query_changed(query, cx);
                                    this.start_search(cx);
                                }
                                // The strip is showing, so an arrow is a step through the
                                // wallpapers rather than a caret move.
                                SearchInputEvent::Navigate(delta) => {
                                    this.step_filmstrip(delta, cx);
                                }
                            }
                            cx.notify();
                        });
                        if result.is_err() {
                            break;
                        }
                    }
                })
                .detach();
                launcher
            },
        )
        .context("opening launcher window")?;
    window.update(cx, |launcher, window, app| {
        window.on_window_should_close(app, |_, _| false);
        launcher.activation_subscription = Some(app.observe_window_activation(
            window,
            |launcher, window, cx| {
                // Ignore initial inactive notifications. Once shown, the first deactivation
                // while logically open means the user switched to another window/app.
                let active = window.is_window_active();
                if active {
                    launcher.was_active = true;
                }
                if should_hide_after_deactivation(
                    launcher.was_active,
                    launcher.state.screen.is_some(),
                    active,
                ) {
                    launcher.hide(cx);
                }
            },
        ));
    })?;
    if service_mode {
        cx.spawn(async move |cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(16)).await;
                let result = window.update(cx, |this, window, cx| {
                    this.poll_service_window(window, cx);
                });
                if result.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
    Ok(window)
}

impl Launcher {
    fn poll_service_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_filmstrip_keys(cx);
        self.advance_filmstrip(cx);
        if !self.poll_service(cx) {
            return;
        }
        if self.state.screen.is_none() {
            self.show(window, cx);
        } else {
            // Re-present without discarding a query, staged consent or accepted action.
            cx.activate(true);
            window.activate_window();
            window.focus(&self.search_input.read(cx).focus_handle(cx));
        }
    }

    fn poll_service(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(service) = &self.service else {
            return false;
        };
        let mut present = false;
        let mut events = Vec::new();
        while let Some(event) = service.try_recv() {
            events.push(event);
        }
        for event in events {
            let request = event.get("request").and_then(serde_json::Value::as_u64);
            match event.get("type").and_then(serde_json::Value::as_str) {
                Some("present") => present = true,
                Some("presentationError") => {
                    self.presentation_error = event
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned);
                    cx.notify();
                }
                Some("ready") => {
                    if let Some(service) = &mut self.service {
                        service.has_details = event
                            .get("itemDetails")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                    }
                }
                // Shared active palette. No request ID: it is independent of
                // searches, routes, selections and actions. An invalid or
                // missing palette keeps the last good colors.
                Some("themeState") => {
                    let Some(value) = event.get("state") else {
                        continue;
                    };
                    let Ok(state) =
                        serde_json::from_value::<crate::protocol::ThemeState>(value.clone())
                    else {
                        continue;
                    };
                    if state.validate().is_err() {
                        continue;
                    }
                    self.theme_error = state.error.as_ref().map(|error| error.message.clone());
                    if let Some(palette) = &state.palette {
                        let theme = Theme::from_palette(palette);
                        if theme != self.theme {
                            self.theme = theme.clone();
                            // Update even while hidden so a later show is correct.
                            self.search_input
                                .update(cx, |input, cx| input.set_theme(theme.clone(), cx));
                            self.dialog_input
                                .update(cx, |input, cx| input.set_theme(theme, cx));
                        }
                    }
                    cx.notify();
                }
                Some("details")
                    if request == Some(self.state.generation)
                        && self.preview_enabled
                        && self.state.screen == Some(Screen::Search) =>
                {
                    if let Some((_, provider, id, content)) = self
                        .service
                        .as_mut()
                        .and_then(|service| service.apply_details(&event))
                    {
                        if !self
                            .state
                            .selection
                            .current(&self.state.items)
                            .is_some_and(|item| item.provider.0 == provider && item.id.0 == id)
                        {
                            continue;
                        }
                        let preview = crate::ui::service_preview::ServicePreview::from_event(
                            provider, id, &content,
                        );
                        for block in &preview.blocks {
                            if let crate::ui::service_preview::ServicePreviewBlock::Image {
                                source: Some(source),
                                ..
                            } = block
                                && let Some(pixels) = self.service.as_ref().and_then(|service| {
                                    service.icon(source.rsplit('/').next().unwrap_or_default())
                                })
                                && let Some(image) = crate::ui::preview_image::PreparedPreview::new(
                                    Preview::Pixels {
                                        bgra: pixels.bgra,
                                        width: pixels.width,
                                        height: pixels.height,
                                    },
                                )
                                .ok()
                                .and_then(|prepared| prepared.image)
                            {
                                self.cache_service_icon(PathBuf::from(source), image);
                            }
                        }
                        self.service.as_mut().unwrap().preview = Some(preview);
                        self.preview_loading_visible = false;
                        cx.notify();
                    }
                }
                Some("assetExpired")
                    if request == Some(self.state.generation)
                        && self.state.screen == Some(Screen::Search)
                        && self
                            .service
                            .as_ref()
                            .is_some_and(|service| service.search_request == request)
                        && self.activation_in_flight.is_none() =>
                {
                    self.clear_preview(cx);
                    let (next, result) = self.service.as_mut().unwrap().refresh(request.unwrap());
                    self.state.generation = next;
                    self.state.items.clear();
                    self.state.selection.reconcile(&self.state.items);
                    self.actions.clear();
                    self.state.searching = result.is_ok();
                    self.error = result.err();
                    cx.notify();
                }
                Some("results") if request == Some(self.state.generation) => {
                    let home_route = event
                        .get("route")
                        .and_then(|route| route.get("type"))
                        .and_then(serde_json::Value::as_str)
                        == Some("home");
                    if let Some((_, items)) = self
                        .service
                        .as_mut()
                        .and_then(|service| service.apply_results(&event))
                    {
                        let mut items = items;
                        let prepared_icons: Vec<_> = self
                            .service
                            .as_ref()
                            .map(|service| {
                                service
                                    .icon_sources
                                    .values()
                                    .filter_map(|source| {
                                        let pixels = service
                                            .icon(source.rsplit('/').next().unwrap_or_default())?;
                                        let image = crate::ui::preview_image::PreparedPreview::new(
                                            Preview::Pixels {
                                                bgra: pixels.bgra,
                                                width: pixels.width,
                                                height: pixels.height,
                                            },
                                        )
                                        .ok()?
                                        .image?;
                                        Some((PathBuf::from(source), image))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let prepared_previews: Vec<_> = self
                            .service
                            .as_ref()
                            .map(|service| {
                                service
                                    .preview_sources
                                    .values()
                                    // A source already painted is left alone. Rebuilding it
                                    // would copy every pixel buffer and allocate a fresh
                                    // render image on the UI thread for each keystroke.
                                    .filter(|source| {
                                        !self.preview_cache.contains_key(Path::new(source))
                                    })
                                    .filter_map(|source| {
                                        let pixels = service.preview(
                                            source.rsplit('/').next().unwrap_or_default(),
                                        )?;
                                        let image = crate::ui::preview_image::PreparedPreview::new(
                                            Preview::Pixels {
                                                bgra: pixels.bgra,
                                                width: pixels.width,
                                                height: pixels.height,
                                            },
                                        )
                                        .ok()?
                                        .image?;
                                        Some((PathBuf::from(source), image))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if let Some(service) = &mut self.service {
                            for (provider, id, source) in event
                                .get("items")
                                .and_then(serde_json::Value::as_array)
                                .into_iter()
                                .flatten()
                                .filter_map(|row| {
                                    Some((
                                        row.get("provider")?.as_str()?.to_owned(),
                                        row.get("id")?.as_str()?.to_owned(),
                                        row.get("iconSource")?.as_str()?.to_owned(),
                                    ))
                                })
                                .filter(|(_, _, source)| {
                                    source.starts_with("image://maccer-icons/")
                                })
                            {
                                service.icon_sources.insert((provider, id), source);
                            }
                        }
                        for (key, image) in prepared_icons {
                            self.cache_service_icon(key, image);
                        }
                        for (key, image) in prepared_previews {
                            self.cache_preview(key, image);
                        }
                        items.truncate(result_limit(self.options.max_results));
                        self.state.apply_results(self.state.generation, items);
                        self.state.searching = false;
                        if home_route && self.state.query.is_empty() {
                            self.home_items = Some(self.state.items.clone());
                        }
                        self.error = event
                            .get("errors")
                            .and_then(serde_json::Value::as_array)
                            .and_then(|errors| errors.first())
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                        self.activation_in_flight = None;
                        self.pending_input = None;
                        self.refresh_actions();
                        self.keep_selection_visible();
                        self.maybe_prewarm_web(cx);
                        cx.notify();
                    }
                }
                Some("error") => {
                    let is_action = request.is_some_and(|request| {
                        self.service
                            .as_mut()
                            .is_some_and(|service| service.finish_action(request))
                    });
                    if request.is_none()
                        || request == Some(self.state.generation)
                        || self.activation_in_flight == request
                    {
                        self.error = event
                            .get("message")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_owned);
                        self.state.searching = false;
                        cx.notify();
                    }
                    if is_action && self.activation_in_flight == request {
                        self.activation_in_flight = None;
                    }
                }
                Some("outcome") => {
                    let is_action = request.is_some_and(|request| {
                        self.service
                            .as_mut()
                            .is_some_and(|service| service.finish_action(request))
                    });
                    let action_is_current = request.is_some_and(|request| {
                        is_action && service_action_is_current(self.activation_in_flight, request)
                    });
                    let is_home = request == Some(self.state.generation)
                        && event.get("kind").and_then(serde_json::Value::as_str)
                            == Some("setQuery");
                    if is_action && self.activation_in_flight == request {
                        self.activation_in_flight = None;
                    }
                    if !action_is_current && !is_home {
                        continue;
                    }
                    match event.get("kind").and_then(serde_json::Value::as_str) {
                        Some("close") => self.hide(cx),
                        Some("setQuery") => {
                            if let Some(route) = event.get("route")
                                && let Some(service) = &mut self.service
                            {
                                service.provider_scope =
                                    route.get("type").and_then(serde_json::Value::as_str)
                                        == Some("provider");
                            }
                            let query = event
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            self.replace_query(query, cx);
                            self.start_search(cx);
                        }
                        Some("keepOpen") => {
                            self.error = event
                                .get("text")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_owned)
                        }
                        Some("output") | Some("refresh") => {
                            self.error = Some(
                                "This action outcome is not supported by service mode.".into(),
                            );
                        }
                        _ => {
                            self.error =
                                Some("The service returned an unsupported action outcome.".into());
                        }
                    }
                    cx.notify();
                }
                Some("preview") if request == Some(self.state.generation) => {
                    let source = event
                        .get("source")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let token = source.rsplit('/').next().unwrap_or_default();
                    let provider = event
                        .get("provider")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let id = event
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(service) = &mut self.service {
                        service.record_preview_source(&provider, &id, token);
                    }
                    if let Some(pixels) = self
                        .service
                        .as_ref()
                        .and_then(|service| service.preview(token))
                    {
                        let key = PathBuf::from(source);
                        if let Some(image) =
                            crate::ui::preview_image::PreparedPreview::new(Preview::Pixels {
                                bgra: pixels.bgra,
                                width: pixels.width,
                                height: pixels.height,
                            })
                            .ok()
                            .and_then(|prepared| prepared.image)
                        {
                            self.cache_preview(key, image);
                        }
                        cx.notify();
                    }
                }
                Some("icon") if request == Some(self.state.generation) => {
                    let source = event
                        .get("source")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default();
                    let token = source.rsplit('/').next().unwrap_or_default();
                    let provider = event
                        .get("provider")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let id = event
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if let Some(service) = &mut self.service {
                        service.record_icon_source(&provider, &id, token);
                    }
                    if let Some(pixels) = self
                        .service
                        .as_ref()
                        .and_then(|service| service.icon(token))
                    {
                        let key = PathBuf::from(source);
                        if let Some(image) =
                            crate::ui::preview_image::PreparedPreview::new(Preview::Pixels {
                                bgra: pixels.bgra,
                                width: pixels.width,
                                height: pixels.height,
                            })
                            .ok()
                            .and_then(|prepared| prepared.image)
                        {
                            self.cache_service_icon(key, image);
                        }
                        cx.notify();
                    }
                }
                _ => {}
            }
        }
        present
    }

    /// Keep the newest wallpaper thumbnails and drop the oldest source when the
    /// budget is reached. Sources are revision-scoped, so an evicted one is
    /// simply re-fetched if the strip scrolls back to it.
    fn cache_preview(&mut self, key: PathBuf, image: Arc<gpui::RenderImage>) {
        if !self.preview_cache.contains_key(&key) {
            while self.preview_cache.len() >= MAX_PREVIEW_CACHE {
                let Some(evicted) = self.preview_order.pop_front() else {
                    break;
                };
                self.preview_cache.remove(&evicted);
            }
            self.preview_order.push_back(key.clone());
        }
        self.preview_cache.insert(key, image);
    }

    fn preview_image(&self, item: &crate::core::Item) -> Option<Arc<gpui::RenderImage>> {
        let service = self.service.as_ref()?;
        let source = service
            .preview_sources
            .get(&(item.provider.0.clone(), item.id.0.clone()))?;
        self.preview_cache.get(&PathBuf::from(source)).cloned()
    }

    /// Move the strip by `delta` frames, wrapping at both ends. Returns false
    /// when the body is not a wallpaper strip, so the caller can fall through
    /// to ordinary list movement.
    fn step_filmstrip(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        if !self.is_filmstrip() || self.state.screen != Some(Screen::Search) {
            return false;
        }
        let count = self.state.items.len() as isize;
        if count == 0 {
            return true;
        }
        let current =
            selected_result_index(&self.state.items, &self.state.selection).unwrap_or(0) as isize;
        // A strip never ends: stepping past the last frame lands on the first
        // one and keeps rotating in the same direction.
        let next = (current + delta).rem_euclid(count) as usize;
        let item = self.state.items[next].clone();
        // Advance the running target by exactly one frame so the row keeps
        // travelling forward across the wrap instead of rewinding.
        self.filmstrip_target = self.filmstrip_target.round() + delta as f32;
        self.state.select_item(&item);
        self.refresh_actions();
        cx.notify();
        true
    }

    /// The strip owns the arrow keys while it is on screen; hand them back
    /// when the body is a list again.
    fn sync_filmstrip_keys(&mut self, cx: &mut Context<Self>) {
        let filmstrip = self.is_filmstrip();
        self.search_input.update(cx, |input, cx| {
            input.set_filmstrip(filmstrip, cx);
        });
    }

    /// Chase the selected index with the same exponential smoothing the
    /// reference strip uses. One value animates the whole row, so fast input
    /// stays bounded by a single chase instead of one animation per tile.
    fn advance_filmstrip(&mut self, cx: &mut Context<Self>) {
        if !self.is_filmstrip() {
            self.filmstrip_pos = 0.;
            self.filmstrip_target = 0.;
            return;
        }
        let Some(index) = selected_result_index(&self.state.items, &self.state.selection) else {
            return;
        };
        let count = self.state.items.len() as f32;
        let selected = index as f32;
        // The strip rotates forever, so a frame has many equivalent positions.
        // The running target is the authority while the strip itself is being
        // stepped; anything else that moves the selection (a click, a fresh
        // snapshot, the query changing) re-anchors to the nearest lap.
        if (self.filmstrip_target.rem_euclid(count) - selected).abs() > 0.5 {
            let mut target = (self.filmstrip_pos / count).round() * count + selected;
            if target - self.filmstrip_pos > count / 2. {
                target -= count;
            } else if target - self.filmstrip_pos < -count / 2. {
                target += count;
            }
            self.filmstrip_target = target;
        }
        let target = self.filmstrip_target;
        let delta = target - self.filmstrip_pos;
        if delta.abs() < 0.002 {
            if self.filmstrip_pos != target {
                self.filmstrip_pos = target;
                cx.notify();
            }
            return;
        }
        let k = 1. - (-(1. / 60.) / FILMSTRIP_CHASE).exp();
        self.filmstrip_pos += delta * k;
        cx.notify();
    }

    /// Which strip ladder this view uses. Palette cards are their own component:
    /// the same five-frame strip at a smaller size.
    fn strip_geometry(&self) -> &'static StripGeometry {
        if self.is_palette_strip() {
            &THEME_STRIP
        } else {
            &WALLPAPER_STRIP
        }
    }

    /// The strip: a focused frame with shrinking neighbours, clipped at both
    /// ends. Frames are absolutely placed so one animated value can move the
    /// whole row.
    fn filmstrip_body(&mut self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Div {
        let geometry = self.strip_geometry();
        let width = geometry.width();
        let band = geometry.band();
        // The outer frames' outer edges are the window edges, so the strip is
        // exactly as wide as the window at full scale.
        let scale = (self.effective_width() / width).clamp(0.35, 1.);
        let pos = self.filmstrip_pos;
        let selected = selected_result_index(&self.state.items, &self.state.selection);
        let view = cx.entity().downgrade();
        let mut strip = div()
            .debug_selector(|| "filmstrip".into())
            .relative()
            .w_full()
            .h(px(band))
            .overflow_hidden();
        // The strip is endless: the frames drawn are the slots nearest the
        // animation position, and every slot wraps onto an item, so stepping
        // past either end keeps rotating in the same direction.
        let count = self.state.items.len() as f32;
        let anchor = pos.round();
        let half = FILMSTRIP_WINDOW as i32;
        let mut slots: Vec<(f32, usize)> = (-half..=half)
            .map(|step| {
                let slot = anchor + step as f32;
                (slot - pos, slot.rem_euclid(count) as usize)
            })
            .collect();
        // Nearest first, and never the same wallpaper twice: a short strip has
        // fewer frames to show than there are slots.
        slots.sort_by(|a, b| a.0.abs().total_cmp(&b.0.abs()));
        let mut drawn: Vec<usize> = Vec::new();
        slots.retain(|(_, index)| {
            if drawn.contains(index) {
                return false;
            }
            drawn.push(*index);
            true
        });
        // Nearer tiles paint over farther ones, as in the reference: sort by
        // distance so the focused card is always on top of its neighbours.
        let mut tiles: Vec<(f32, gpui::Div)> = Vec::new();
        for (offset, index) in slots {
            let item = &self.state.items[index];
            // Slots past the last distinct one keep its size, so the outer
            // frames never pop while the row glides between them.
            let distance = offset.abs().min(geometry.slot_w.len() as f32 - 1.);
            let tile_w = filmstrip_slot(&geometry.slot_w, distance) * scale;
            let tile_h = filmstrip_slot(&geometry.slot_h, distance) * scale;
            let centre = filmstrip_slot(&geometry.slot_cx, distance) * scale;
            let x = width / 2. + if offset < 0. { -centre } else { centre } - tile_w / 2.;
            // Centred in the band, which keeps 16 of air above and below.
            let y = STRIP_INSET + (geometry.frame_h - tile_h) / 2.;
            let bright = filmstrip_slot(&geometry.bright, distance);
            let focused = selected == Some(index);
            // Small radii keep the joins tight; the strip reads as one band.
            let radius = (if focused { 8. } else { 6. }) * scale;
            let card = self.service.as_ref().and_then(|service| {
                service
                    .theme_cards
                    .get(&(item.provider.0.clone(), item.id.0.clone()))
                    .cloned()
            });
            let mut tile = div()
                .debug_selector(move || format!("filmstrip-tile-{index}"))
                .absolute()
                .left(px(x))
                .top(px(y))
                .w(px(tile_w))
                .h(px(tile_h))
                .rounded(px(radius))
                .overflow_hidden()
                .bg(theme.divider);
            let focus_color = if let Some(card) = &card {
                // The card is the theme itself while focused and a quiet panel
                // at distance, so a light theme can never outshine the frame
                // being looked at. The palette stays legible at every size.
                tile = tile.bg(if focused {
                    card.background
                } else {
                    theme.panel_fill()
                });
                let dots_top = filmstrip_slot(&THEME_DOTS_TOP, distance) * scale;
                let dot = filmstrip_slot(&THEME_DOTS, distance) * scale;
                let gap = filmstrip_slot(&THEME_DOT_GAP, distance) * scale;
                tile = tile.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(dots_top))
                        .flex()
                        .justify_center()
                        .gap(px(gap))
                        .children(
                            card.swatches
                                .iter()
                                .map(|color| div().size(px(dot)).rounded_full().bg(*color)),
                        ),
                );
                // The two frames at the edge are read as palettes, not names.
                if distance < 1.5 {
                    let bottom = filmstrip_slot(&THEME_NAME_BOTTOM, distance) * scale;
                    tile = tile.child(
                        div()
                            .absolute()
                            .left(px(0.))
                            .right(px(0.))
                            .bottom(px(bottom))
                            .px(px(filmstrip_slot(&THEME_NAME_PAD, distance) * scale))
                            .text_center()
                            .text_size(px(filmstrip_slot(&THEME_NAME, distance) * scale))
                            .text_color(if focused {
                                card.foreground
                            } else {
                                theme.muted
                            })
                            .truncate()
                            .child(item.title.clone()),
                    );
                }
                if card.applied {
                    // The theme the desktop is wearing, in its own accent.
                    tile = tile.child(
                        div()
                            .debug_selector(move || format!("filmstrip-live-{index}"))
                            .absolute()
                            .top(px(10. * scale))
                            .right(px(10. * scale))
                            .size(px(5. * scale))
                            .rounded_full()
                            .bg(card.accent),
                    );
                }
                card.accent
            } else {
                let image = self.preview_image(item);
                tile = match image {
                    Some(image) => tile.child(
                        gpui::img(image)
                            .size_full()
                            .object_fit(gpui::ObjectFit::Cover),
                    ),
                    None => tile.child(
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(theme.muted)
                            .text_size(px((13. * scale).max(8.)))
                            // A provider option frame carries its own glyph;
                            // a card-less theme frame falls back to its name.
                            .child(icon_text(item).unwrap_or_else(|| {
                                item.title.chars().next().unwrap_or('◧').to_string()
                            })),
                    ),
                };
                // Distance dims rather than desaturates: one overlay, no image
                // filter. Fading toward the surface keeps light themes light.
                tile = tile.child(div().absolute().inset_0().bg(gpui::Hsla {
                    a: 1. - bright,
                    ..theme.background
                }));
                theme.accent
            };
            tile = tile.child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded(px(radius))
                    .border_1()
                    .border_color(if focused { focus_color } else { theme.divider }),
            );
            let item_for_click = item.clone();
            let view = view.clone();
            tile = tile.on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if this.state.searching {
                        return;
                    }
                    this.state.select_item(&item_for_click);
                    this.refresh_actions();
                    window.focus(&this.search_input.read(cx).focus_handle(cx));
                    cx.notify();
                });
            });
            tiles.push((distance, tile));
        }
        tiles.sort_by(|a, b| b.0.total_cmp(&a.0));
        for (_, tile) in tiles {
            strip = strip.child(tile);
        }
        div()
            .debug_selector(|| "filmstrip-body".into())
            .w_full()
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_center()
            .child(strip)
    }

    fn cache_service_icon(&mut self, key: PathBuf, image: Arc<gpui::RenderImage>) {
        let is_new = !self.icon_cache.contains_key(&key);
        let count = self
            .icon_cache
            .keys()
            .filter(|path| path.to_string_lossy().starts_with("image://maccer-icons/"))
            .count();
        if is_new
            && count >= MAX_SERVICE_ICON_CACHE
            && let Some(evicted) = self
                .icon_cache
                .keys()
                .find(|path| path.to_string_lossy().starts_with("image://maccer-icons/"))
                .cloned()
        {
            self.icon_cache.remove(&evicted);
        }
        self.icon_cache.insert(key, image);
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.screen.is_some() {
            self.hide(cx);
        } else {
            self.show(window, cx);
        }
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Remember which application the user was in before the launcher takes
        // focus. Never replace a captured target while already shown.
        self.prepare_web_host(window);
        self.capture_paste_target();
        self.state.screen = Some(Screen::Search);
        self.was_active = false;
        self.clear_preview(cx);
        self.replace_query(String::new(), cx);
        let showing_cached_home = self.home_items.is_some();
        if let Some(items) = self.home_items.clone() {
            self.state.items = items;
            self.state.selection = Default::default();
            self.state.selection.reconcile(&self.state.items);
        } else {
            self.state.items.clear();
            self.state.selection.reconcile(&self.state.items);
        }
        cx.activate(true);
        window.activate_window();
        window.focus(&self.search_input.read(cx).focus_handle(cx));
        self.resize_window(window);
        self.start_home_search(cx);
        if showing_cached_home {
            // Keep the cached home list on screen while the replacement is in
            // flight; the result handler will apply it when it arrives.
            self.state.searching = false;
        }
        cx.notify();
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
        self.hide_web_preview(cx);
        self.state.screen = None;
        self.clear_preview(cx);
        // An in-flight icon load is left to finish and be cached; scheduling the
        // next one is gated on the launcher being visible again.
        self.drag_start = None;
        self.output = None;
        self.was_active = false;
        self.state.generation = self.state.generation.wrapping_add(1);
        if let Some(cancellation) = self.search_cancellation.take() {
            cancellation.cancel();
        }
        if let Some(service) = &mut self.service {
            let _ = service.cancel();
        }
        self.actions.clear();
        self.action_item = None;
        self.pending_input = None;
        self.invalidate_activation();
        if self.hide_window {
            cx.hide();
        }
        cx.notify();
    }

    fn replace_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.query_changed(query.clone(), cx);
        self.search_input
            .update(cx, |input, cx| input.set_text(query, cx));
    }

    fn query_changed(&mut self, query: String, cx: &mut Context<Self>) {
        self.hide_web_preview(cx);
        self.drag_start = None;
        self.output = None;
        self.state.replace_query(query);
        self.actions.clear();
        self.action_item = None;
        self.invalidate_activation();
    }

    /// Capture the AppKit views for the retained window. Unit tests use GPUI's
    /// test platform, which has no native handle.
    fn prepare_web_host(&mut self, window: &mut Window) {
        if self.web_host.is_some() || cfg!(test) {
            return;
        }
        match crate::macos::WebHost::from_window(window) {
            Ok(host) => self.web_host = Some(host),
            Err(error) => tracing::debug!(%error, "web preview is unavailable"),
        }
    }

    /// Create the native web view if it does not exist yet. The host is
    /// captured when the launcher window is shown, so a launcher that never
    /// shows cannot create one. Tests run against GPUI's test platform, which
    /// has no native handle, and keep the field empty.
    fn ensure_web_preview(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<&mut crate::macos::WebPreview> {
        if self.web_preview.is_none() {
            let host = self.web_host.as_ref()?;
            let (sender, receiver) = async_channel::unbounded();
            let started = std::time::Instant::now();
            let preview = match crate::macos::WebPreview::new(host, sender) {
                Ok(preview) => preview,
                Err(error) => {
                    tracing::debug!(%error, "web preview could not be created");
                    return None;
                }
            };
            tracing::debug!(
                stage = "web_preview_create",
                duration_ms = started.elapsed().as_secs_f64() * 1000.,
                "web preview created"
            );
            self.web_preview = Some(preview);
            cx.spawn(async move |this, cx| {
                while let Ok(event) = receiver.recv().await {
                    if matches!(event, crate::macos::WebPreviewEvent::Hidden) {
                        // The native key monitor hid the page before GPUI saw
                        // the key, so the release is scheduled from here.
                        let _ = this.update(cx, |this, cx| this.schedule_web_preview_release(cx));
                    }
                }
            })
            .detach();
        }
        self.web_preview.as_mut()
    }

    /// Pay WebKit's process start-up while the user is still reading results.
    /// Only a selected web result is worth it: a launcher search that will open
    /// an app or a file must not start WebKit. The first navigation is what
    /// spawns the WebContent process, so the warm load is an empty document
    /// rather than any search page.
    fn maybe_prewarm_web(&mut self, cx: &mut Context<Self>) {
        let selected = self.state.selection.current(&self.state.items);
        if !self.options.web.should_prewarm(selected) {
            return;
        }
        self.warm_web_preview(cx);
    }

    fn warm_web_preview(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.ensure_web_preview(cx) else {
            return;
        };
        let started = std::time::Instant::now();
        match preview.warm() {
            Ok(()) => tracing::debug!(
                stage = "web_preview_warm",
                duration_ms = started.elapsed().as_secs_f64() * 1000.,
                "web preview warmed"
            ),
            Err(error) => tracing::debug!(%error, "web preview could not be warmed"),
        }
    }

    /// Returns true when the page was placed in the launcher. A false result
    /// lets the caller fall back to the normal browser action.
    fn show_web_preview(&mut self, url: String, cx: &mut Context<Self>) -> bool {
        if !self.options.web.inline {
            return false;
        }
        // Showing the panel is an instant re-open, so a pending idle release
        // must not drop the document underneath it.
        self.web_preview_idle = None;
        let Some(preview) = self.ensure_web_preview(cx) else {
            return false;
        };
        match preview.show(&url) {
            Ok(()) => true,
            Err(error) => {
                tracing::debug!(%error, "web preview could not load the page");
                false
            }
        }
    }

    fn hide_web_preview(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = &mut self.web_preview else {
            return;
        };
        if !preview.is_visible() {
            return;
        }
        preview.hide();
        self.schedule_web_preview_release(cx);
    }

    /// Release a hidden page's document once an immediate re-open is unlikely.
    /// The WebKit processes stay warm, so the next search still skips start-up.
    fn schedule_web_preview_release(&mut self, cx: &mut Context<Self>) {
        let idle_release = self.options.web.idle_release;
        if idle_release.is_zero() {
            return;
        }
        let Some(preview) = &self.web_preview else {
            return;
        };
        if preview.is_visible() {
            return;
        }
        let this = cx.entity().downgrade();
        self.web_preview_idle = Some(cx.spawn(async move |_, cx| {
            gpui::Timer::after(idle_release).await;
            let _ = this.update(cx, |this, _| {
                let Some(preview) = &mut this.web_preview else {
                    return;
                };
                if preview.is_visible() {
                    return;
                }
                preview.release_page();
                tracing::debug!(
                    stage = "web_preview_release",
                    "web preview document released"
                );
            });
        }));
    }

    fn clear_preview(&mut self, _cx: &mut Context<Self>) {
        if let Some(service) = &mut self.service {
            service.invalidate_details();
        }
        if let Some(token) = self.preview_cancellation.take() {
            token.cancel();
        }
        // Keep decoded pixels and uploaded textures for synchronous revisits.
        self.preview_image = None;
        self.preview_loading_visible = false;
        self.preview_key = None;
        self.preview = None;
        self.preview_started = None;
        self.preview_span = None;
        self.preview_applied_at = None;
        self.preview_scroll.set_offset(Default::default());
    }
    fn has_preview(&self) -> bool {
        self.preview_enabled
            && self.state.screen == Some(Screen::Search)
            && self.preview_key.is_some()
    }
    fn effective_width(&self) -> f32 {
        if self.is_filmstrip() {
            return self.strip_geometry().width();
        }
        if self.has_preview() {
            (self.options.width + PREVIEW_WIDTH).min(1200.)
        } else {
            self.options.width
        }
    }

    /// A wallpaper browse is a wide strip, not a list, and never a detail view.
    fn is_filmstrip(&self) -> bool {
        self.service.as_ref().is_some_and(|service| {
            service.layout == crate::ui::service_backend::ServiceLayout::Filmstrip
        }) && self.state.screen == Some(Screen::Search)
            && !self.state.items.is_empty()
    }
    /// A strip whose frames are palette cards rather than wallpaper images.
    ///
    /// One card is enough to pick the ladder: the theme scope also publishes
    /// plain option frames (install, uninstall), and those must be drawn at the
    /// same scale as the palettes beside them rather than flipping the whole
    /// strip to the larger wallpaper geometry.
    fn is_palette_strip(&self) -> bool {
        self.is_filmstrip()
            && self
                .service
                .as_ref()
                .is_some_and(|service| !service.theme_cards.is_empty())
    }
    fn on_toggle_preview(&mut self, _: &TogglePreview, _: &mut Window, cx: &mut Context<Self>) {
        self.preview_enabled = !self.preview_enabled;
        self.clear_preview(cx);
        cx.notify();
    }
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        if self.service.is_some() {
            // Keep the prior result display stable while its replacement is
            // pending. ServiceBackend already revoked action/detail authority.
            if self.state.searching {
                return;
            }
            let selected = (self.preview_enabled
                && !self.state.searching
                && self.state.screen == Some(Screen::Search))
            .then(|| self.state.selection.current(&self.state.items).cloned())
            .flatten();
            let selected = selected.filter(|item| {
                // Application details repeat the row and force the window open
                // wider. The result row already carries the name and type.
                item.provider.0 != "apps"
                    && self.service.as_ref().is_some_and(|service| {
                        service.has_details
                            && service
                                .details_available
                                .get(&(item.provider.0.clone(), item.id.0.clone()))
                                .copied()
                                .unwrap_or(false)
                    })
            });
            let Some(item) = selected else {
                if self.preview_key.is_some() {
                    self.clear_preview(cx);
                }
                return;
            };
            let key = (
                item.provider.0.clone(),
                item.id.0.clone(),
                self.state.generation,
            );
            if self.preview_key.as_ref() == Some(&key) {
                return;
            }
            self.clear_preview(cx);
            self.preview_key = Some(key);
            self.preview_loading_visible = true;
            if let Err(error) = self.service.as_mut().unwrap().details(
                self.state.generation,
                &item.provider.0,
                &item.id.0,
            ) {
                self.preview_loading_visible = false;
                self.preview = Some(Preview::Info(error));
            }
            return;
        }
        if self.state.searching {
            return;
        }
        let selected = if self.preview_enabled && self.state.screen == Some(Screen::Search) {
            self.state.selection.current(&self.state.items).cloned()
        } else {
            None
        };
        let selected = selected.and_then(|item| {
            self.coordinator
                .as_ref()?
                .registry
                .get(&item.provider)
                .filter(|p| p.supports_preview(&item))
                .map(|p| (item, p))
        });
        let Some((item, provider)) = selected else {
            if self.preview_key.is_some() {
                self.clear_preview(cx);
            }
            return;
        };
        let key = (
            item.provider.0.clone(),
            item.id.0.clone(),
            provider.preview_revision(&item),
        );
        if self.preview_key.as_ref() == Some(&key) {
            return;
        }
        self.clear_preview(cx);
        self.preview_key = Some(key.clone());
        static REQUEST_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let request_id = REQUEST_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let span = tracing::debug_span!("preview", request_id, provider = %key.0, revision = key.2);
        self.preview_span = Some(span.clone());
        let cache_started = std::time::Instant::now();
        if let Some(image) = self.preview_images.get(&key) {
            self.preview_image = Some(image);
            self.preview = Some(Preview::Image { png: vec![] });
            span.in_scope(|| {
                tracing::debug!(
                    stage = "decoded_cache_lookup",
                    cache_hit = true,
                    duration_ms = cache_started.elapsed().as_secs_f64() * 1000.,
                    "preview stage"
                )
            });
            return;
        }
        span.in_scope(|| {
            tracing::debug!(
                stage = "decoded_cache_lookup",
                cache_hit = false,
                duration_ms = cache_started.elapsed().as_secs_f64() * 1000.,
                "preview stage"
            )
        });
        let token = CancellationToken::default();
        self.preview_cancellation = Some(token.clone());
        let started = std::time::Instant::now();
        self.preview_started = Some(started);
        span.in_scope(|| tracing::debug!("preview requested"));
        self.watch_preview_deadline(key.clone(), token.clone(), cx);
        self.watch_preview_loading(key.clone(), token.clone(), cx);
        let this = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            if token.is_cancelled() {
                return;
            }
            let worker_token = token.clone();
            let worker_span = span.clone();
            let (result, completed_at) = cx
                .background_executor()
                .spawn(async move {
                    worker_span.in_scope(|| {
                        tracing::debug!(
                            stage = "worker_queue_wait",
                            duration_ms = started.elapsed().as_secs_f64() * 1000.,
                            "preview stage"
                        );
                        let provider_started = std::time::Instant::now();
                        let preview = provider.preview(&item, &worker_token);
                        tracing::debug!(
                            stage = "provider_preview",
                            duration_ms = provider_started.elapsed().as_secs_f64() * 1000.,
                            success = preview.is_ok(),
                            "preview stage"
                        );
                        let result = preview.and_then(|preview| {
                            preview
                                .map(crate::ui::preview_image::PreparedPreview::new)
                                .transpose()
                        });
                        tracing::debug!(
                            elapsed_ms = started.elapsed().as_secs_f64() * 1000.,
                            success = result.is_ok(),
                            "preview worker completed"
                        );
                        (result, std::time::Instant::now())
                    })
                })
                .await;
            if token.is_cancelled() {
                return;
            }
            let _ = this.update(cx, |this, cx| {
                span.in_scope(|| {
                    tracing::debug!(
                        stage = "ui_dispatch_wait",
                        duration_ms = completed_at.elapsed().as_secs_f64() * 1000.,
                        "preview stage"
                    );
                    this.apply_prepared_preview(&key, result, cx);
                });
            });
        })
        .detach();
    }
    fn watch_preview_loading(
        &self,
        key: crate::ui::preview_image::PreviewKey,
        token: CancellationToken,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| {
                if !token.is_cancelled()
                    && this.preview_key.as_ref() == Some(&key)
                    && this.preview.is_none()
                {
                    this.preview_loading_visible = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn watch_preview_deadline(
        &self,
        key: (String, String, u64),
        token: CancellationToken,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            let _ = this.update(cx, |this, cx| {
                if token.is_cancelled()
                    || this.preview_key.as_ref() != Some(&key)
                    || this.preview.is_some()
                {
                    return;
                }
                // Explain the delay without cancelling valid slow work (PDFs
                // retain their backend deadline). A late result may still display
                // and populate the cache; selection changes still cancel it.
                tracing::warn!(provider = %key.0, "preview loading is slow");
                this.apply_preview(
                    &key,
                    Ok(Some(Preview::Info(
                        "Preview is taking too long. Use Quick Look.".into(),
                    ))),
                    cx,
                );
            });
        })
        .detach();
    }

    fn apply_preview(
        &mut self,
        key: &(String, String, u64),
        result: Result<Option<Preview>>,
        cx: &mut Context<Self>,
    ) {
        let prepared = result.and_then(|preview| {
            preview
                .map(crate::ui::preview_image::PreparedPreview::new)
                .transpose()
        });
        self.apply_prepared_preview(key, prepared, cx);
    }

    fn apply_prepared_preview(
        &mut self,
        key: &crate::ui::preview_image::PreviewKey,
        result: Result<Option<crate::ui::preview_image::PreparedPreview>>,
        cx: &mut Context<Self>,
    ) {
        if self.preview_key.as_ref() != Some(key) {
            return;
        }
        let applied_at = std::time::Instant::now();
        let span = self
            .preview_span
            .clone()
            .unwrap_or_else(tracing::Span::none);
        let _entered = span.enter();
        self.preview = match result {
            Ok(Some(prepared)) => {
                if let Some(image) = prepared.image {
                    self.preview_images.insert(key.clone(), image.clone());
                    self.preview_image = Some(image);
                }
                Some(prepared.preview)
            }
            Ok(None) => Some(Preview::Info("No preview available".into())),
            Err(error) => Some(Preview::Info(format!("Preview unavailable: {error}"))),
        };
        self.preview_applied_at = Some(std::time::Instant::now());
        tracing::debug!(
            stage = "ui_apply",
            duration_ms = applied_at.elapsed().as_secs_f64() * 1000.,
            elapsed_ms = self
                .preview_started
                .map(|start| start.elapsed().as_secs_f64() * 1000.),
            "preview stage"
        );
        cx.notify();
    }
    fn preview_panel(&self, height: f32) -> gpui::Stateful<gpui::Div> {
        let mut panel = div()
            .id("file-preview")
            .debug_selector(|| "file-preview".into())
            .w(px(PREVIEW_WIDTH))
            .h(px(height))
            .flex_shrink_0()
            .border_l_1()
            .border_color(self.theme.divider)
            .px_4()
            .py_3()
            .overflow_y_scroll()
            .track_scroll(&self.preview_scroll)
            .text_size(px(12.))
            .line_height(px(17.));
        if let Some(preview) = self
            .service
            .as_ref()
            .and_then(|service| service.preview.as_ref())
        {
            return panel.children(preview.elements(&self.icon_cache, &self.theme));
        }
        match &self.preview {
            Some(Preview::Text { text, truncated }) => {
                panel = panel.child(div().font_family("monospace").child(text.clone()));
                if *truncated {
                    panel = panel.child(
                        div()
                            .pt_2()
                            .text_color(self.theme.muted)
                            .child("First 64 KiB shown. Use Quick Look for the full file."),
                    );
                }
            }
            Some(Preview::Image { .. } | Preview::Pixels { .. }) => {
                if let Some(image) = &self.preview_image {
                    panel = panel.child(
                        gpui::img(image.clone())
                            .object_fit(gpui::ObjectFit::Contain)
                            .w_full()
                            .h(px(height - 24.)),
                    );
                }
            }
            Some(Preview::Info(message)) => {
                panel = panel.child(message.clone());
            }
            None if self.preview_loading_visible => {
                panel = panel.child("Loading preview…");
            }
            None => {}
        }
        panel
    }
    fn on_drag_move(
        &mut self,
        event: &gpui::MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !event.dragging()
            || self.state.screen != Some(Screen::Search)
            || self.activation_in_flight.is_some()
            || self.state.searching
        {
            self.drag_start = None;
            return;
        }
        let Some((_, start)) = &self.drag_start else {
            return;
        };
        if (event.position.x - start.x).abs() + (event.position.y - start.y).abs() < px(8.) {
            return;
        }
        let (item, _) = self.drag_start.take().unwrap();
        if let Some(provider) = self
            .coordinator
            .as_ref()
            .and_then(|c| c.registry.get(&item.provider))
            && provider.supports_drag(&item)
            && let Err(error) = provider.begin_drag(&item)
        {
            self.error = Some(format!("Could not drag file: {error}"));
            cx.notify();
        }
    }
    fn invalidate_activation(&mut self) {
        if let Some(cancellation) = self.activation_cancellation.take() {
            cancellation.cancel();
        }
        // A pending paste belongs to the activation that started it. Cancel it
        // on query changes, dismissal, or a newer activation.
        self.cancel_paste();
        self.activation_serial = self.activation_serial.wrapping_add(1);
        self.activation_in_flight = None;
    }

    /// Capture the frontmost application's pid as the paste target. Called when
    /// the launcher is shown, before it activates and steals focus. Our own pid
    /// and invalid pids are rejected so the launcher never pastes into itself.
    fn capture_paste_target(&mut self) {
        if self.service.is_some() || self.state.screen.is_some() {
            return;
        }
        let Some(backend) = self.paste_backend.as_ref() else {
            return;
        };
        // Ignore our own pid and empty readings so the last external application
        // is retained rather than clobbered by the launcher itself.
        if let Some(pid) =
            (backend.frontmost)().filter(|pid| *pid > 0 && *pid != std::process::id() as i32)
        {
            self.paste_target = Some(pid);
        }
    }

    fn visible_error(&self) -> Option<&String> {
        // A query or paste error wins; otherwise surface a stale/missing palette.
        self.error
            .as_ref()
            .or(self.paste_error.as_ref())
            .or(self.presentation_error.as_ref())
            .or(self.theme_error.as_ref())
    }

    fn cancel_paste(&mut self) {
        if let Some(token) = self.paste_cancellation.take() {
            token.cancel();
        }
    }

    /// Copy `text` to the pasteboard, return to the saved application, and send
    /// Command-V after a short delay so activation can finish. Failures before
    /// hiding keep the launcher open with a visible error. A failure after
    /// hiding is remembered for the next time the launcher opens rather than
    /// stealing focus. The keystroke re-checks the target, so it never pastes
    /// into the wrong application.
    fn paste_text(&mut self, text: String, cx: &mut Context<Self>) {
        self.paste_error = None;
        let Some(pid) = self.paste_target else {
            self.error = Some(
                "Nothing to paste into. Open the launcher while another app is frontmost.".into(),
            );
            cx.notify();
            return;
        };
        let Some(backend) = self.paste_backend.as_ref() else {
            self.error = Some("Paste is unavailable in service mode.".into());
            return;
        };
        let prepare = backend.prepare.clone();
        let send = backend.send.clone();
        if let Err(error) = prepare(&text, pid) {
            self.error = Some(format!("Could not paste: {error}"));
            cx.notify();
            return;
        }
        // The target now owns the clipboard text; hide and let it activate.
        self.hide(cx);
        self.cancel_paste();
        let token = CancellationToken::default();
        self.paste_cancellation = Some(token.clone());
        let this = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(PASTE_DELAY_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                if token.is_cancelled() {
                    return;
                }
                this.paste_cancellation = None;
                if let Err(error) = send(pid) {
                    this.paste_error = Some(format!("Could not paste: {error}. The text is still on the clipboard for manual pasting."));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn apply_action_outcome(&mut self, outcome: ActionOutcome, cx: &mut Context<Self>) {
        match outcome {
            ActionOutcome::Close => self.hide(cx),
            ActionOutcome::KeepOpen(message) => {
                self.error = Some(message);
                cx.notify();
            }
            ActionOutcome::Output { title, text } => {
                self.output = Some((title, text));
                self.output_scroll.set_offset(Default::default());
                self.state.screen = Some(Screen::Output);
                self.error = None;
                cx.notify();
            }
            ActionOutcome::SetQuery(query) => {
                self.replace_query(query, cx);
                self.start_search(cx);
            }
            ActionOutcome::RefreshSearch => {
                self.state.screen = Some(Screen::Search);
                self.output = None;
                self.action_item = None;
                self.error = None;
                self.clear_preview(cx);
                self.refresh_actions();
                self.start_search(cx);
                cx.notify();
            }
            ActionOutcome::PasteText(text) => self.paste_text(text, cx),
        }
    }

    fn start_search(&mut self, cx: &mut Context<Self>) {
        self.start_search_with(cx, false);
    }

    /// Refresh the home view without depending on the route left by the last
    /// provider scope. Callers that already display cached home rows use this
    /// so the replacement cannot re-query an empty provider view.
    fn start_home_search(&mut self, cx: &mut Context<Self>) {
        self.start_search_with(cx, true);
    }

    fn start_search_with(&mut self, cx: &mut Context<Self>, home: bool) {
        if self.service.is_some() {
            // The service icon cache is already bounded. Query changes must not
            // evict the images still used by the displayed previous snapshot.
            self.actions.clear();
            self.action_item = None;
            self.state.searching = true;
        }
        if let Some(service) = &mut self.service {
            let (request, result) = if home {
                service.search_home()
            } else {
                service.search(self.state.query.clone())
            };
            self.state.generation = request;
            self.error = result.err();
            self.state.searching = self.error.is_none();
            return;
        }
        let Some(coordinator) = &self.coordinator else {
            return;
        };
        if let Some(cancellation) = self.search_cancellation.take() {
            cancellation.cancel();
        }
        let session = coordinator.start(self.state.query.clone());
        self.search_cancellation = Some(session.cancellation.clone());
        let generation = session.generation;
        self.state.generation = generation;
        let receiver = session.receiver;
        let this = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            while let Ok(update) = receiver.recv().await {
                let generation = update.generation;
                let mut items = update.items;
                let errors = update.errors;
                let complete = update.complete;
                let result = this.update(cx, |this, cx| {
                    items.truncate(result_limit(this.options.max_results));
                    if this.state.apply_search_update(generation, items, complete) {
                        if complete && this.state.query.is_empty() {
                            this.home_items = Some(this.state.items.clone());
                        }
                        this.error = errors.first().map(|failure| failure.message.clone());
                        if this.state.screen != Some(Screen::Actions) {
                            this.refresh_actions();
                        }
                        this.schedule_icon_loads(cx);
                        this.keep_selection_visible();
                        this.maybe_prewarm_web(cx);
                        cx.notify();
                    }
                });
                if result.is_err() || complete {
                    break;
                }
            }
        })
        .detach();
    }

    /// Fill a bounded set of worker slots, selecting from the current viewport
    /// before warming offscreen results. No queued stale list survives a query
    /// change. Hidden/action/output screens stop new work but retain completions.
    fn schedule_icon_loads(&mut self, cx: &mut Context<Self>) {
        if self.state.screen != Some(Screen::Search) {
            return;
        }
        while let Some(path) = self.next_icon_to_load() {
            self.icon_loading.insert(path.clone());
            let this = cx.entity().downgrade();
            cx.spawn(async move |_, cx| {
                let load_path = path.clone();
                let image = cx
                    .background_executor()
                    .spawn(async move {
                        let started = std::time::Instant::now();
                        let image =
                            crate::macos::application_icon_pixels(&load_path).and_then(|pixels| {
                                crate::ui::preview_image::PreparedPreview::new(Preview::Pixels {
                                    bgra: pixels.bgra,
                                    width: pixels.width,
                                    height: pixels.height,
                                })
                                .ok()?
                                .image
                            });
                        tracing::debug!(
                            pipeline = "application_icon_pixels",
                            duration_ms = started.elapsed().as_secs_f64() * 1000.,
                            success = image.is_some(),
                            encoded_decode_required = false,
                            "application icon prepared"
                        );
                        image
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.icon_loading.remove(&path);
                    match image {
                        Some(image) => {
                            this.icon_cache.insert(path, image);
                        }
                        None => {
                            this.icon_failed.insert(path);
                        }
                    }
                    cx.notify();
                    this.schedule_icon_loads(cx);
                });
            })
            .detach();
        }
    }

    fn next_icon_to_load(&self) -> Option<PathBuf> {
        if self.icon_loading.len() >= MAX_ICON_LOADS {
            return None;
        }
        let items = &self.state.items;
        let viewport = self
            .icon_viewport
            .as_ref()
            .filter(|(generation, _)| *generation == self.state.generation)
            .map(|(_, range)| range.clone())
            .unwrap_or(0..visible_result_count(items.len()));
        let start = viewport.start.min(items.len());
        let end = viewport.end.min(items.len()).max(start);
        items[start..end]
            .iter()
            .chain(&items[..start])
            .chain(&items[end..])
            .find_map(|item| {
                let Some(crate::core::IconDescriptor::ApplicationBundle(path)) = &item.icon else {
                    return None;
                };
                if self.icon_cache.contains_key(path)
                    || self.icon_failed.contains(path)
                    || self.icon_loading.contains(path)
                {
                    return None;
                }
                Some(path.clone())
            })
    }

    /// The action panel: the selected row's own actions first, then whatever the
    /// provider published for the whole scope. Every entry carries its subject.
    fn refresh_actions(&mut self) {
        let mut actions: Vec<LauncherAction> = Vec::new();
        if let Some(item) = self.state.selection.current(&self.state.items).cloned() {
            if item.provider.0 == "picker" {
                actions.push(LauncherAction {
                    item,
                    action: Action {
                        id: ActionId("select-provider".into()),
                        title: "Select Provider".into(),
                    },
                });
            } else {
                let own = self
                    .service
                    .as_ref()
                    .and_then(|service| {
                        service
                            .authorized_actions
                            .get(&(item.provider.0.clone(), item.id.0.clone()))
                            .cloned()
                    })
                    .or_else(|| {
                        self.coordinator.as_ref().and_then(|coordinator| {
                            coordinator
                                .registry
                                .get(&item.provider)
                                .map(|p| p.actions(&item))
                        })
                    })
                    .unwrap_or_default();
                actions.extend(own.into_iter().map(|action| LauncherAction {
                    item: item.clone(),
                    action,
                }));
            }
        }
        if let Some(service) = &self.service {
            actions.extend(
                service
                    .scope_actions()
                    .iter()
                    .cloned()
                    .map(|entry| LauncherAction {
                        item: entry.item,
                        action: entry.action,
                    }),
            );
        }
        self.actions = actions;
        self.state.action_index = self
            .state
            .action_index
            .min(self.actions.len().saturating_sub(1));
    }

    /// The panel labels `state` shows: the action titles, in panel order.
    fn action_titles(&self) -> Vec<Action> {
        self.actions
            .iter()
            .map(|entry| entry.action.clone())
            .collect()
    }

    /// The actions of the selected row itself, ignoring scope commands. Tab and
    /// Return work on those, so a provider command can never take over the
    /// keyboard shortcut that runs the selection.
    fn selected_actions(&self) -> Vec<LauncherAction> {
        let Some(item) = self.state.selection.current(&self.state.items) else {
            return Vec::new();
        };
        self.actions
            .iter()
            .filter(|entry| entry.item.provider == item.provider && entry.item.id == item.id)
            .cloned()
            .collect()
    }

    fn keep_selection_visible(&self) {
        if self.state.screen == Some(Screen::Search)
            && let Some(index) = selected_result_index(&self.state.items, &self.state.selection)
        {
            self.list_scroll
                .scroll_to_item(index, ScrollStrategy::Center);
        }
    }

    fn resize_window(&self, window: &mut Window) {
        if self
            .web_preview
            .as_ref()
            .is_some_and(|preview| preview.is_visible())
        {
            let desired = size(px(self.effective_width()), px(WEB_PREVIEW_HEIGHT));
            let current = window.bounds().size;
            if ((current.width - desired.width).abs() > px(1.)
                || (current.height - desired.height).abs() > px(1.))
                && window.resize_centered(desired).is_err()
            {
                window.resize(desired);
            }
            if let Some(preview) = &self.web_preview {
                preview.update_frame();
            }
            return;
        }
        if self.is_filmstrip() {
            let status = if self.visible_error().is_some() {
                24.
            } else {
                0.
            };
            let desired = size(
                px(self.effective_width()),
                px(self.strip_geometry().window_height() + status),
            );
            if window.resize_centered(desired).is_err() {
                window.resize(desired);
            }
            return;
        }
        let mut row_count = if self.state.screen == Some(Screen::Actions) {
            visible_result_count(self.actions.len())
        } else {
            visible_result_count(self.state.items.len())
        };
        if self.has_preview() {
            row_count = row_count.max(5);
        }
        // Reserve room for the staged confirmation panel and its hint line.
        if self
            .service
            .as_ref()
            .is_some_and(|service| service.pending_confirmation.is_some())
        {
            row_count += 5;
        }
        // And for the provider dialog: a title, a label, the field and a hint.
        if let Some(pending) = &self.pending_input {
            row_count += if pending.field.is_select() {
                // The list scrolls past six choices.
                pending.field.options.len().min(6) + 4
            } else {
                5
            };
        }
        let error_height = if self.visible_error().is_some()
            && self.state.screen == Some(Screen::Search)
            && !self.state.items.is_empty()
        {
            24.
        } else {
            0.
        };
        let height = if self.state.screen == Some(Screen::Output) {
            364.
        } else if row_count == 0 {
            120.
        } else {
            // 2px outer border + 52px input + 8px list padding + 22px status.
            84. + row_count as f32 * ROW_HEIGHT + error_height
        }
        .clamp(120., MAX_WINDOW_HEIGHT);
        let desired = size(px(self.effective_width()), px(height));
        // Resizing alone keeps the old left edge, so the preview pushes the
        // entire popup to the right. Move and resize its frame together instead.
        if window.resize_centered(desired).is_err() {
            // Portable fallback for platforms where positioning is unavailable.
            let current = window.bounds().size;
            if (current.width - desired.width).abs() > px(1.)
                || (current.height - desired.height).abs() > px(1.)
            {
                window.resize(desired);
            }
        }
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        // A provider dialog is modal while it is open: arrows move its choice
        // list, never the results underneath it.
        if self.move_input(delta, cx) {
            return;
        }
        // A staged confirmation is modal: selection cannot move out from under
        // the consent the user is being asked for.
        if self
            .service
            .as_ref()
            .is_some_and(|service| service.pending_confirmation.is_some())
        {
            return;
        }
        if self.state.searching && self.state.screen == Some(Screen::Search) {
            return;
        }
        if self.state.screen == Some(Screen::Output) {
            let mut offset = self.output_scroll.offset();
            offset.y -= px(delta as f32 * 48.0);
            self.output_scroll.set_offset(offset);
            cx.notify();
            return;
        }
        if self.step_filmstrip(delta, cx) {
            return;
        }
        if self.state.screen == Some(Screen::Actions) {
            if !self.actions.is_empty() {
                let len = self.actions.len() as isize;
                self.state.action_index =
                    (self.state.action_index as isize + delta).rem_euclid(len) as usize;
            }
        } else {
            self.state.move_selection(delta);
            self.refresh_actions();
            self.keep_selection_visible();
            self.maybe_prewarm_web(cx);
        }
        self.resize_window(window);
        cx.notify();
    }

    fn on_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, window, cx);
    }
    /// One wheel notch steps one wallpaper. Trackpad pixel deltas and wheel
    /// lines both accumulate here so a flick never skips several tiles.
    fn on_scroll_wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.is_filmstrip() {
            return;
        }
        let (x, y) = match event.delta {
            gpui::ScrollDelta::Pixels(delta) => (f32::from(delta.x), f32::from(delta.y)),
            gpui::ScrollDelta::Lines(delta) => (delta.x * 18., delta.y * 18.),
        };
        // A horizontal gesture wins when it is the larger one: the strip is a
        // horizontal surface, but many mice only send vertical notches.
        let step = if x.abs() > y.abs() { x } else { y };
        self.filmstrip_wheel += step;
        let notches = -(self.filmstrip_wheel / 24.).trunc();
        if notches != 0. {
            self.filmstrip_wheel += notches * 24.;
            self.move_selection(notches as isize, window, cx);
        }
    }

    fn on_move_down(&mut self, _: &MoveDown, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, window, cx);
    }

    fn on_copy_output(&mut self, _: &CopyOutput, _: &mut Window, cx: &mut Context<Self>) {
        if let Some((_, text)) = &self.output {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        }
    }

    fn on_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        // The native key monitor normally hides the page first. This covers
        // Escape reaching GPUI after a click moved focus back to the input.
        if self
            .web_preview
            .as_ref()
            .is_some_and(|preview| preview.is_visible())
        {
            self.hide_web_preview(cx);
            window.focus(&self.search_input.read(cx).focus_handle(cx));
            self.resize_window(window);
            cx.notify();
            return;
        }
        // A provider dialog closes before anything else on screen.
        if self.cancel_input(cx) {
            window.focus(&self.search_input.read(cx).focus_handle(cx));
            self.resize_window(window);
            return;
        }
        // Escape dismisses a staged confirmation without sending an operation.
        if let Some(service) = &mut self.service
            && service.pending_confirmation.is_some()
        {
            service.cancel_confirmation();
            self.error = None;
            cx.notify();
            return;
        }
        if self.state.screen == Some(Screen::Search)
            && self
                .service
                .as_ref()
                .is_some_and(|service| service.provider_scope && self.state.query.is_empty())
        {
            if let Some(service) = &mut self.service {
                let (request, result) = service.home();
                self.state.generation = request;
                self.state.searching = result.is_ok();
                self.state.items.clear();
                self.state.screen = Some(Screen::Search);
                self.output = None;
                self.activation_in_flight = None;
                self.actions.clear();
                self.action_item = None;
                self.error = result.err();
            }
            self.resize_window(window);
            cx.notify();
            return;
        }
        if self.activation_in_flight.is_some() {
            let service_action = self.service.is_some();
            self.invalidate_activation();
            self.error = Some(if service_action {
                "Action continues in the background; its result will not change this view.".into()
            } else {
                "Action cancelled.".into()
            });
            cx.notify();
            return;
        }
        match self.state.escape() {
            EscapeResult::Back => {
                self.output = None;
                window.focus(&self.search_input.read(cx).focus_handle(cx));
                self.action_item = None;
                self.refresh_actions();
            }
            EscapeResult::Cleared => {
                self.replace_query(String::new(), cx);
                self.start_search(cx);
            }
            EscapeResult::Hide => self.hide(cx),
        }
        self.resize_window(window);
        cx.notify();
    }

    fn on_actions(&mut self, _: &OpenActions, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_input.is_some()
            || self.state.screen == Some(Screen::Output)
            || self.state.searching
        {
            return;
        }
        if self
            .service
            .as_ref()
            .is_some_and(|service| service.pending_confirmation.is_some())
        {
            return;
        }
        self.refresh_actions();
        self.action_item = self.state.selection.current(&self.state.items).cloned();
        self.state.enter_actions(&self.action_titles());
        self.resize_window(window);
        cx.notify();
    }

    fn on_next_action(&mut self, _: &NextAction, _: &mut Window, cx: &mut Context<Self>) {
        if self.pending_input.is_some()
            || self.state.screen == Some(Screen::Output)
            || self.state.searching
        {
            return;
        }
        if self
            .service
            .as_ref()
            .is_some_and(|service| service.pending_confirmation.is_some())
        {
            return;
        }
        if self.state.screen == Some(Screen::Actions) {
            if !self.actions.is_empty() {
                self.state.action_index = (self.state.action_index + 1) % self.actions.len();
                cx.notify();
            }
            return;
        }
        let Some(item) = self.state.selection.current(&self.state.items).cloned() else {
            return;
        };
        if item.provider.0 == "picker" {
            self.select_provider(&item, cx);
            return;
        }
        self.refresh_actions();
        if let Some(entry) = self.selected_actions().get(1).cloned() {
            self.activate(entry.item, entry.action, cx);
        }
    }

    fn select_provider(&mut self, item: &Item, cx: &mut Context<Self>) {
        if item.provider.0 == "picker"
            && let Some(prefix) = item.payload.get("prefix").and_then(|value| value.as_str())
        {
            self.replace_query(prefix.to_string(), cx);
            self.start_search(cx);
        }
    }

    fn on_provider_picker(&mut self, _: &ProviderPicker, _: &mut Window, cx: &mut Context<Self>) {
        if self.pending_input.is_some() {
            return;
        }
        if let Some(service) = &mut self.service {
            let (request, result) = service.home();
            self.state.generation = request;
            self.activation_in_flight = None;
            self.state.screen = Some(Screen::Search);
            self.output = None;
            self.state.query.clear();
            self.state.items.clear();
            self.state.searching = result.is_ok();
            self.actions.clear();
            self.action_item = None;
            self.error = result.err();
            self.search_input
                .update(cx, |input, cx| input.set_text(String::new(), cx));
            cx.notify();
            return;
        }
        self.replace_query(";".into(), cx);
        self.start_search(cx);
    }

    fn on_confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        // A provider dialog is the innermost thing on screen: Return submits it.
        if self.pending_input.is_some() {
            self.submit_input(cx);
            return;
        }
        // A staged confirmation takes priority over every other Return target.
        if let Some(service) = &mut self.service
            && service.pending_confirmation.is_some()
        {
            match service.confirm() {
                Ok(request) => {
                    self.activation_in_flight = Some(request);
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
            cx.notify();
            return;
        }
        if self.state.screen == Some(Screen::Output) || self.state.searching {
            return;
        }
        if self.state.screen == Some(Screen::Actions) {
            if let Some(entry) = self.actions.get(self.state.action_index).cloned() {
                if entry.item.provider.0 == "picker" {
                    self.select_provider(&entry.item, cx);
                } else {
                    self.activate(entry.item, entry.action, cx);
                }
            }
        } else if let Some(item) = self.state.selection.current(&self.state.items).cloned() {
            if item.provider.0 == "picker" {
                self.select_provider(&item, cx);
            } else {
                self.refresh_actions();
                // Return runs the selection's own first action, not a command
                // the scope happens to publish before it.
                if let Some(entry) = self.selected_actions().first().cloned() {
                    self.activate(entry.item, entry.action, cx);
                }
            }
        }
    }

    fn activate(&mut self, item: Item, action: Action, cx: &mut Context<Self>) {
        if self.activation_in_flight.is_some() {
            return;
        }
        // A provider that publishes a field decides it needs one collected
        // first. Nothing is sent until the dialog is submitted.
        if let Some(field) = self
            .service
            .as_ref()
            .and_then(|service| service.action_input(&item, &action))
        {
            self.open_input(item, action, field, cx);
            return;
        }
        self.activate_with_input(item, action, Default::default(), cx);
    }

    /// Open the provider's dialog. Every word in it is the provider's; the
    /// frontend only knows how to draw a text field and a choice list.
    fn open_input(
        &mut self,
        item: Item,
        action: Action,
        field: crate::ui::service_backend::ServiceActionInput,
        cx: &mut Context<Self>,
    ) {
        self.error = None;
        if !field.is_select() {
            self.dialog_input
                .update(cx, |input, cx| input.set_text(String::new(), cx));
        }
        self.pending_input = Some(PendingInput {
            item,
            action,
            field,
            index: 0,
        });
        cx.notify();
    }

    /// Submit the pending dialog: collect the value, then send the action the
    /// provider asked for. A published confirmation still stages first.
    fn submit_input(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_input.take() else {
            return;
        };
        let problem = |label: &str, empty: bool| {
            if empty {
                format!("{label} is required.")
            } else {
                format!("{label} has no choices.")
            }
        };
        let value = if pending.field.is_select() {
            match pending.field.options.get(pending.index) {
                Some(option) => option.value.clone(),
                None => {
                    self.error = Some(problem(&pending.field.label, false));
                    self.pending_input = Some(pending);
                    cx.notify();
                    return;
                }
            }
        } else {
            let text = self.dialog_input.read(cx).text().trim().to_owned();
            if text.is_empty() {
                self.error = Some(problem(&pending.field.label, true));
                self.pending_input = Some(pending);
                cx.notify();
                return;
            }
            text
        };
        let mut input = std::collections::BTreeMap::new();
        input.insert(pending.field.id.clone(), value);
        self.activate_with_input(pending.item, pending.action, input, cx);
    }

    /// Dismiss the dialog without sending anything.
    fn cancel_input(&mut self, cx: &mut Context<Self>) -> bool {
        if self.pending_input.take().is_none() {
            return false;
        }
        self.error = None;
        cx.notify();
        true
    }

    /// Move the highlight in a choice dialog. A text field keeps the caret keys.
    fn move_input(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let Some(pending) = self.pending_input.as_mut() else {
            return false;
        };
        let count = pending.field.options.len();
        if !pending.field.is_select() || count == 0 {
            return true;
        }
        let len = count as isize;
        pending.index = (pending.index as isize + delta).rem_euclid(len) as usize;
        cx.notify();
        true
    }

    fn activate_with_input(
        &mut self,
        item: Item,
        action: Action,
        input: std::collections::BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        if self.activation_in_flight.is_some() {
            return;
        }
        // A web search opens its page in the native panel instead of the
        // browser. If WebKit or the AppKit host is unavailable, fall through
        // to the provider's ordinary Open action.
        if let Some(url) = web_search_url(&item, &action)
            && self.show_web_preview(url, cx)
        {
            self.error = None;
            cx.notify();
            return;
        }
        if let Some(service) = &mut self.service {
            match service.activate(&item, &action, input) {
                Ok(Some(request)) => {
                    self.activation_in_flight = Some(request);
                    self.error = None;
                }
                // A provider-published prompt was staged. No operation is sent
                // until the user confirms; `on_confirm` performs the invoke.
                Ok(None) => self.error = None,
                Err(error) => self.error = Some(error),
            }
            cx.notify();
            return;
        }
        let Some(provider) = self
            .coordinator
            .as_ref()
            .and_then(|c| c.registry.get(&item.provider))
        else {
            return;
        };
        self.activation_serial = self.activation_serial.wrapping_add(1);
        let activation_serial = self.activation_serial;
        self.activation_in_flight = Some(activation_serial);
        let cancellation = CancellationToken::default();
        self.activation_cancellation = Some(cancellation.clone());
        self.error = None;
        self.paste_error = None;
        cx.notify();
        let Some(history) = self.history.clone() else {
            return;
        };
        let query = self.state.query.clone();
        let this = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    provider
                        .activate_with_cancellation(&item, &action, &cancellation)
                        .inspect(|_| {
                            if !provider.records_usage() {
                                return;
                            }
                            let _ = history.record(&UsageEvent {
                                query,
                                provider: item.provider.clone(),
                                item: item.id.clone(),
                                action: action.id.clone(),
                                timestamp: std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs() as i64,
                            });
                        })
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                if !should_apply_activation(
                    activation_serial,
                    this.activation_in_flight.unwrap_or_default(),
                ) {
                    return;
                }
                this.activation_in_flight = None;
                this.activation_cancellation = None;
                match result {
                    Ok(outcome) => this.apply_action_outcome(outcome, cx),
                    Err(error) => {
                        this.error = Some(error.to_string());
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

impl Focusable for Launcher {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Launcher {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.refresh_preview(cx);
        for image in self.preview_images.take_evicted() {
            cx.drop_image(image, Some(window));
        }
        if self.preview.is_some()
            && let Some(started) = self.preview_started.take()
        {
            let span = self
                .preview_span
                .clone()
                .unwrap_or_else(tracing::Span::none);
            span.in_scope(|| {
                tracing::debug!(
                    stage = "ui_render_wait",
                    duration_ms = self
                        .preview_applied_at
                        .take()
                        .map(|applied| applied.elapsed().as_secs_f64() * 1000.),
                    elapsed_ms = started.elapsed().as_secs_f64() * 1000.,
                    "preview render callback reached"
                )
            });
        }
        self.resize_window(window);
        let theme = self.theme.clone();
        let state = self.state.screen.clone();
        let max_height = if self
            .web_preview
            .as_ref()
            .is_some_and(|preview| preview.is_visible())
        {
            WEB_PREVIEW_HEIGHT
        } else {
            MAX_WINDOW_HEIGHT
        };
        // A provider-published prompt staged by the last action selection. The
        // backend owns consent, so render only its plain text, never a slug.
        let confirmation = self
            .service
            .as_ref()
            .and_then(|service| service.pending_confirmation.clone());
        if state == Some(Screen::Output) {
            window.focus(&self.focus_handle);
        }
        // The provider dialog is the focus while it is open: typing belongs in
        // its field, and Return/Escape still reach the launcher's key context.
        if self.pending_input.is_some() {
            window.focus(&self.dialog_input.read(cx).focus_handle(cx));
            self.dialog_was_focused = true;
        } else if self.dialog_was_focused {
            self.dialog_was_focused = false;
            window.focus(&self.search_input.read(cx).focus_handle(cx));
        }
        let selected = self
            .state
            .selection
            .selected()
            .map(|(p, i)| (p.to_string(), i.to_string()));
        let items = self.state.items.clone();
        let actions = self.actions.clone();
        let action_index = self.state.action_index;
        let mut root = div()
            .key_context(if state == Some(Screen::Output) {
                "Launcher LauncherOutput"
            } else {
                "Launcher"
            })
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_actions))
            .on_action(cx.listener(Self::on_provider_picker))
            .on_action(cx.listener(Self::on_next_action))
            .on_action(cx.listener(Self::on_escape))
            .on_action(cx.listener(Self::on_copy_output))
            .on_action(cx.listener(Self::on_toggle_preview))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_mouse_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, _| {
                    this.drag_start = None;
                }),
            )
            .debug_selector(|| "launcher-container".into())
            .flex()
            .flex_col()
            .w(px(self.effective_width()))
            .h_full()
            .max_h(px(max_height))
            .overflow_hidden()
            .rounded(px(12.))
            .font_family("monospace")
            .text_size(px(14.))
            .line_height(px(18.))
            .border_1()
            .border_color(theme.divider)
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .h(px(52.))
                    .flex_shrink_0()
                    .px_4()
                    .py(px(12.))
                    .border_b_1()
                    .border_color(theme.divider)
                    .child(self.search_input.clone()),
            );

        if state.is_some()
            && let Some(pending) = &confirmation
        {
            // Mouse affordances mirror the Return/Escape keyboard paths. Both
            // route through the same backend consent, so neither can send the
            // action without the prompt having been shown first.
            let confirm_view = cx.entity().downgrade();
            let cancel_view = cx.entity().downgrade();
            root = root.child(
                div()
                    .debug_selector(|| "service-confirmation".into())
                    .mx_2()
                    .mt_2()
                    .px_2()
                    .py_2()
                    .rounded(px(ROW_RADIUS))
                    .border_1()
                    .border_color(theme.divider)
                    .bg(theme.selection_fill())
                    .flex_shrink_0()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .id("service-confirmation-message")
                            .debug_selector(|| "service-confirmation-prompt".into())
                            .max_h(px(128.))
                            .overflow_y_scroll()
                            .text_size(px(12.))
                            .line_height(px(17.))
                            .child(pending.prompt.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(12.))
                            .text_size(px(10.))
                            .line_height(px(14.))
                            .text_color(theme.muted)
                            .child(div().flex_1().child(pending.action.title.clone()))
                            .child(
                                div()
                                    .id("service-confirmation-apply")
                                    .debug_selector(|| "service-confirmation-apply".into())
                                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                                        let _ = confirm_view.update(cx, |this, cx| {
                                            this.on_confirm(&Confirm, window, cx)
                                        });
                                    })
                                    .child("↵ Apply"),
                            )
                            .child(
                                div()
                                    .id("service-confirmation-cancel")
                                    .debug_selector(|| "service-confirmation-cancel".into())
                                    .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                                        let _ = cancel_view.update(cx, |this, cx| {
                                            this.on_escape(&Escape, window, cx)
                                        });
                                    })
                                    .child("Esc Cancel"),
                            ),
                    ),
            );
        }

        // The provider's field, if it asked for one. Every word here comes
        // from the provider; this only knows a text box from a choice list.
        if state.is_some()
            && let Some(pending) = self.pending_input.as_ref()
        {
            let field = pending.field.clone();
            let chosen = pending.index;
            let dialog_input = self.dialog_input.clone();
            let empty = dialog_input.read(cx).text().trim().is_empty();
            let view = cx.entity().downgrade();
            let mut panel = div()
                .debug_selector(|| "service-input-dialog".into())
                .mx_2()
                .mt_2()
                .px_2()
                .py_2()
                .flex_shrink_0()
                .flex()
                .flex_col()
                .gap(px(6.))
                .rounded(px(ROW_RADIUS))
                .border_1()
                .border_color(theme.divider)
                .bg(theme.selection_fill())
                .child(
                    div()
                        .id("service-input-title")
                        .debug_selector(|| "service-input-title".into())
                        .text_size(px(12.))
                        .line_height(px(17.))
                        .child(field.title.clone()),
                )
                .child(
                    div()
                        .debug_selector(|| "service-input-label".into())
                        .text_size(px(11.))
                        .line_height(px(15.))
                        .text_color(theme.muted)
                        .child(field.label.clone()),
                );
            if field.is_select() {
                for (index, option) in field.options.iter().enumerate().take(6) {
                    let selected = index == chosen;
                    let view = view.clone();
                    panel = panel.child(
                        div()
                            .debug_selector(move || format!("service-input-option-{index}"))
                            .flex()
                            .items_center()
                            .h(px(26.))
                            .px_2()
                            .rounded(px(6.))
                            .bg(if selected {
                                theme.panel_fill()
                            } else {
                                gpui::transparent_black()
                            })
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted
                            })
                            .on_mouse_down(gpui::MouseButton::Left, move |_, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    if let Some(pending) = this.pending_input.as_mut() {
                                        pending.index = index;
                                    }
                                    cx.notify();
                                });
                            })
                            .child(option.label.clone()),
                    );
                }
                if field.options.len() > 6 {
                    panel = panel.child(
                        div()
                            .text_size(px(10.))
                            .text_color(theme.muted)
                            .child(format!("{} more…", field.options.len() - 6)),
                    );
                }
            } else {
                panel = panel.child(
                    div()
                        .rounded(px(6.))
                        .border_1()
                        .border_color(theme.divider)
                        .bg(theme.panel_fill())
                        .px_2()
                        .py(px(2.))
                        .child(dialog_input),
                );
                if empty && let Some(placeholder) = &field.placeholder {
                    panel = panel.child(
                        div()
                            .debug_selector(|| "service-input-placeholder".into())
                            .text_size(px(10.))
                            .line_height(px(14.))
                            .text_color(theme.muted)
                            .child(placeholder.clone()),
                    );
                }
            }
            let cancel_view = cx.entity().downgrade();
            root = root.child(
                panel.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(12.))
                        .text_size(px(10.))
                        .line_height(px(14.))
                        .text_color(theme.muted)
                        .child(
                            div()
                                .debug_selector(|| "service-input-submit".into())
                                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                                    let _ = cancel_view.update(cx, |this, cx| {
                                        this.on_confirm(&Confirm, window, cx)
                                    });
                                })
                                .child(format!("↵ {}", field.submit_label)),
                        )
                        .child(
                            div()
                                .debug_selector(|| "service-input-cancel".into())
                                .on_mouse_down(gpui::MouseButton::Left, move |_, window, cx| {
                                    let _ = view
                                        .update(cx, |this, cx| this.on_escape(&Escape, window, cx));
                                })
                                .child("Esc Cancel"),
                        ),
                ),
            );
        }

        if state.is_some() {
            if state == Some(Screen::Output) {
                if let Some((title, text)) = &self.output {
                    root = root
                        .child(
                            div()
                                .h(px(24.))
                                .px_4()
                                .flex()
                                .items_center()
                                .text_size(px(11.))
                                .text_color(theme.muted)
                                .child(title.clone()),
                        )
                        .child(
                            div()
                                .id("command-output")
                                .mx_4()
                                .mb_3()
                                .h(px(240.))
                                .rounded(px(ROW_RADIUS))
                                .border_1()
                                .border_color(theme.divider)
                                .bg(theme.panel_fill())
                                .p_3()
                                .overflow_y_scroll()
                                .track_scroll(&self.output_scroll)
                                .font_family("monospace")
                                .text_size(px(12.))
                                .line_height(px(18.))
                                .child(text.clone()),
                        );
                }
            } else if state == Some(Screen::Actions) {
                for (index, entry) in actions
                    .iter()
                    .enumerate()
                    .skip(action_index.saturating_sub(crate::ui::state::MAX_VISIBLE_RESULTS - 1))
                    .take(crate::ui::state::MAX_VISIBLE_RESULTS)
                {
                    let selected = index == action_index;
                    root = root.child(
                        div()
                            .debug_selector(|| format!("action-{index}"))
                            .relative()
                            .rounded(px(ROW_RADIUS))
                            .mx_2()
                            .h(px(ROW_HEIGHT))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .px_2()
                            .bg(if selected {
                                theme.selection_fill()
                            } else {
                                gpui::transparent_black()
                            })
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted
                            })
                            .children(selected.then(|| {
                                div()
                                    .absolute()
                                    .left(px(0.))
                                    .top(px(12.))
                                    .bottom(px(12.))
                                    .w(px(2.))
                                    .rounded_full()
                                    .bg(theme.accent)
                            }))
                            .child(entry.action.title.clone()),
                    );
                }
            } else if items.is_empty() {
                root = root.child(
                    div()
                        .h(px(44.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(12.))
                        .text_color(theme.muted)
                        .child(self.visible_error().cloned().unwrap_or_else(|| {
                            if self.state.searching {
                                "Searching…"
                            } else {
                                "No results"
                            }
                            .into()
                        })),
                );
            } else if self.is_filmstrip() {
                // A strip has no result list to carry a status line, and a
                // failed install or removal must not look like nothing happened.
                if let Some(error) = self.visible_error() {
                    root = root.child(
                        div()
                            .debug_selector(|| "launcher-error".into())
                            .px_4()
                            .h(px(24.))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .text_size(px(11.))
                            .line_height(px(14.))
                            .text_color(theme.muted)
                            .truncate()
                            .child(error.clone()),
                    );
                }
                root = root.child(self.filmstrip_body(&theme, cx));
            } else {
                if let Some(error) = self.visible_error() {
                    root = root.child(
                        div()
                            .px_4()
                            .h(px(24.))
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .line_height(px(14.))
                            .text_color(theme.muted)
                            .truncate()
                            .child(error.clone()),
                    );
                }
                let visible_count =
                    visible_result_count(items.len()).max(if self.has_preview() { 5 } else { 1 });
                let row_theme = theme.clone();
                let selected_for_rows = selected.clone();
                let rows_view = cx.entity().downgrade();
                // Render-ready BGRA images bypass GPUI's encoded-image decoder.
                let generation = self.state.generation;
                let mut icon_images: HashMap<PathBuf, Arc<gpui::RenderImage>> = HashMap::new();
                let service_icon_sources = self
                    .service
                    .as_ref()
                    .map(|service| service.icon_sources.clone())
                    .unwrap_or_default();
                for item in &items {
                    let key = service_icon_sources
                        .get(&(item.provider.0.clone(), item.id.0.clone()))
                        .map(PathBuf::from)
                        .or_else(|| match &item.icon {
                            Some(crate::core::IconDescriptor::ApplicationBundle(path)) => {
                                Some(path.clone())
                            }
                            _ => None,
                        });
                    if let Some(key) = key
                        && let Some(image) = self.icon_cache.get(&key)
                    {
                        icon_images.insert(key, image.clone());
                    }
                }
                let list = uniform_list("launcher-results", items.len(), move |range, _, cx| {
                    // Layout reports the real range after mouse/keyboard scroll.
                    // Scheduling only spawns bounded work; no native calls here.
                    let _ = rows_view.update(cx, |this, cx| {
                        if this.state.generation == generation
                            && this.state.screen == Some(Screen::Search)
                        {
                            this.icon_viewport = Some((generation, range.clone()));
                            this.schedule_icon_loads(cx);
                        }
                    });
                    range
                        .map(|index| {
                            let item = &items[index];
                            let is_selected = selected_for_rows
                                .as_ref()
                                .is_some_and(|(p, id)| p == &item.provider.0 && id == &item.id.0);
                            let view = rows_view.clone();
                            let item_for_click = item.clone();
                            let image = service_icon_sources
                                .get(&(item.provider.0.clone(), item.id.0.clone()))
                                .map(PathBuf::from)
                                .or_else(|| match &item.icon {
                                    Some(crate::core::IconDescriptor::ApplicationBundle(path)) => {
                                        Some(path.clone())
                                    }
                                    _ => None,
                                })
                                .and_then(|key| icon_images.get(&key).cloned());
                            div()
                                .w_full()
                                .px_2()
                                .child(result_row(item, is_selected, &row_theme, image).h_full())
                                .h(px(ROW_HEIGHT))
                                .id(("result-row", index))
                                .on_mouse_down(gpui::MouseButton::Left, move |event, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        if this.state.searching {
                                            return;
                                        }
                                        // While a confirmation is staged the
                                        // prompt is modal; row clicks are inert.
                                        if this.service.as_ref().is_some_and(|service| {
                                            service.pending_confirmation.is_some()
                                        }) {
                                            return;
                                        }
                                        this.state.select_item(&item_for_click);
                                        this.refresh_actions();
                                        this.drag_start =
                                            Some((item_for_click.clone(), event.position));
                                        window.focus(&this.search_input.read(cx).focus_handle(cx));
                                        if event.click_count == 2 {
                                            this.on_confirm(&Confirm, window, cx);
                                        }
                                        cx.notify();
                                    });
                                })
                        })
                        .collect::<Vec<_>>()
                })
                .h(px(visible_count as f32 * ROW_HEIGHT))
                .flex_shrink_0()
                .track_scroll(self.list_scroll.clone())
                .flex_1();
                let mut body = div().flex().w_full().py_1().flex_shrink_0().child(list);
                if self.has_preview() {
                    body = body.child(self.preview_panel(visible_count as f32 * ROW_HEIGHT));
                }
                root = root.child(body);
            }
            let hint = if confirmation.is_some() {
                "↵ Apply · Esc Cancel".to_string()
            } else if state == Some(Screen::Output) {
                "⌘C Copy · Esc Back".to_string()
            } else if self.state.searching {
                "Searching…".to_string()
            } else if self.activation_in_flight.is_some() {
                "Running… · Esc Cancel".to_string()
            } else if state == Some(Screen::Actions) {
                "↵ Run · Esc Back".to_string()
            } else if self.is_palette_strip() {
                // The scope's frames are actions, not only "apply": name the
                // one Return would run, in the provider's own words.
                match self.selected_actions().first() {
                    Some(entry) => format!(
                        "←→ Browse · ↵ {} · ⌘K Actions · Esc Close",
                        entry.action.title.trim_end_matches('…')
                    ),
                    None => "←→ Browse · ⌘K Actions · Esc Close".to_string(),
                }
            } else if self.is_filmstrip() {
                "←→ Browse · ↵ Set · Esc Close".to_string()
            } else {
                "↵ Open · ⌘K Actions · Esc Close".to_string()
            };
            let position = selected_result_index(&self.state.items, &self.state.selection)
                .map(|index| format!("{}/{}", index + 1, self.state.items.len()))
                .unwrap_or_default();
            root = root.child(
                div()
                    .h(px(FOOTER_HEIGHT))
                    .flex_shrink_0()
                    .mt_auto()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap(px(12.))
                    .px_4()
                    .text_size(px(10.))
                    .line_height(px(14.))
                    .text_color(theme.muted)
                    .child(hint)
                    .child(
                        div()
                            .flex_shrink_0()
                            .child(if state == Some(Screen::Search) {
                                position
                            } else {
                                String::new()
                            }),
                    ),
            );
        }
        root
    }
}

fn service_action_is_current(in_flight: Option<u64>, request: u64) -> bool {
    in_flight == Some(request)
}

/// The configured web provider's primary action carries the encoded search
/// URL in its item payload. Service rows receive the same URL as `webLink`
/// domain data, which `ServiceBackend` copies into this payload.
fn web_search_url(item: &Item, action: &Action) -> Option<String> {
    if item.provider.0 != "web" || action.id.0 != "search" {
        return None;
    }
    let url = item
        .payload
        .get("url")
        .and_then(serde_json::Value::as_str)?;
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(url.to_owned())
}

/// Piecewise-linear lookup across the three strip slots, clamped at the last.
/// A provider's own short glyph for an item, when it published one. Used where
/// a row has no image to draw and its name would be the wrong label.
fn icon_text(item: &Item) -> Option<String> {
    match &item.icon {
        Some(crate::core::IconDescriptor::Text(text)) if !text.starts_with("image://") => {
            let text = text.trim();
            (!text.is_empty() && text != "•").then(|| text.to_owned())
        }
        _ => None,
    }
}

fn filmstrip_slot(values: &[f32; 3], distance: f32) -> f32 {
    let last = values.len() - 1;
    if distance >= last as f32 {
        return values[last];
    }
    let index = distance.floor().max(0.) as usize;
    let fraction = distance - index as f32;
    values[index] + (values[index + 1] - values[index]) * fraction
}

/// Rows without a subtitle show their source instead. Applications speak for
/// themselves, so they stay single-line.
fn provider_label(provider: &str) -> Option<String> {
    if provider == "apps" || provider.is_empty() {
        return None;
    }
    let mut chars = provider.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}

fn result_row(
    item: &Item,
    selected: bool,
    theme: &crate::ui::theme::Theme,
    image: Option<Arc<gpui::RenderImage>>,
) -> gpui::Div {
    // The provider menu is navigation, not results: one name per row, no icon
    // and no subtitle, so six providers read as six words.
    if item.provider.0 == "menu" {
        return div()
            .debug_selector(|| format!("suggestion-{}", item.id.0))
            .relative()
            .w_full()
            .min_w_0()
            .px_2()
            .rounded(px(ROW_RADIUS))
            .text_color(theme.foreground)
            .bg(if selected {
                theme.selection_fill()
            } else {
                gpui::transparent_black()
            })
            .flex()
            .items_center()
            .children(selected.then(|| {
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(12.))
                    .bottom(px(12.))
                    .w(px(2.))
                    .rounded_full()
                    .bg(theme.accent)
            }))
            .child(
                div()
                    .debug_selector(|| format!("suggestion-title-{}", item.id.0))
                    .flex_1()
                    .min_w_0()
                    .text_size(px(14.))
                    .line_height(px(18.))
                    .truncate()
                    .child(item.title.clone()),
            );
    }
    let secondary = item
        .subtitle
        .clone()
        .or_else(|| provider_label(&item.provider.0));
    let icon = if let Some(image) = image {
        Some(
            gpui::img(image)
                .size(px(ICON_SIZE))
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
        )
    } else {
        match &item.icon {
            Some(crate::core::IconDescriptor::Png(png)) => Some(
                gpui::img(Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    png.to_vec(),
                )))
                .size(px(ICON_SIZE))
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            ),
            Some(crate::core::IconDescriptor::Text(text)) if !text.starts_with("image://") => {
                let text = text.trim();
                // The service sends a bullet for glyph icons it cannot project.
                // The title's initial keeps the slot informative.
                let glyph = if text.is_empty() || text == "•" {
                    item.title
                        .chars()
                        .next()
                        .map(|c| c.to_uppercase().to_string())
                        .unwrap_or_default()
                } else {
                    text.to_owned()
                };
                Some(
                    div()
                        .size(px(ICON_SIZE))
                        .rounded(px(6.))
                        .bg(theme.panel_fill())
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_size(px(11.))
                        .text_color(theme.muted)
                        .truncate()
                        .child(glyph)
                        .into_any_element(),
                )
            }
            _ => Some(
                div()
                    .size(px(ICON_SIZE))
                    .rounded(px(6.))
                    .bg(theme.panel_fill())
                    .into_any_element(),
            ),
        }
    };
    let icon_slot = icon.map(|icon| {
        div()
            .debug_selector(|| format!("suggestion-icon-{}", item.id.0))
            .size(px(ICON_SIZE))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .child(icon)
    });
    div()
        .debug_selector(|| format!("suggestion-{}", item.id.0))
        .relative()
        .w_full()
        .min_w_0()
        .px_2()
        .rounded(px(ROW_RADIUS))
        .text_color(theme.foreground)
        .bg(if selected {
            theme.selection_fill()
        } else {
            gpui::transparent_black()
        })
        .flex()
        .items_center()
        .gap(px(10.))
        .children(selected.then(|| {
            div()
                .absolute()
                .left(px(0.))
                .top(px(12.))
                .bottom(px(12.))
                .w(px(2.))
                .rounded_full()
                .bg(theme.accent)
        }))
        .children(icon_slot)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(2.))
                .child(
                    div()
                        .debug_selector(|| format!("suggestion-title-{}", item.id.0))
                        .text_size(px(14.))
                        .line_height(px(18.))
                        .flex_shrink_0()
                        .truncate()
                        .child(item.title.clone()),
                )
                .children(secondary.map(|secondary| {
                    div()
                        .debug_selector(|| format!("suggestion-type-{}", item.id.0))
                        .text_size(px(11.))
                        .line_height(px(14.))
                        .flex_shrink_0()
                        .truncate()
                        .text_color(theme.muted)
                        .child(secondary)
                })),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        HistoryStore, ItemId, Provider, ProviderId, ProviderRegistry, SearchContext, SearchQuery,
    };
    use crate::macos::{Application, ApplicationIconPixels, Platform};
    use crate::service::{IconResolver, application_service};
    use gpui::{
        AvailableSpace, ScrollDelta, ScrollWheelEvent, TestAppContext, VisualTestContext, point,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn provider_labels_skip_applications_and_capitalize_sources() {
        assert_eq!(provider_label("apps"), None);
        assert_eq!(provider_label(""), None);
        assert_eq!(provider_label("web").as_deref(), Some("Web"));
        assert_eq!(provider_label("clipboard").as_deref(), Some("Clipboard"));
    }

    #[test]
    fn prewarming_waits_for_a_selected_web_result_and_respects_both_opt_outs() {
        let item = |provider: &str| Item {
            id: ItemId(format!("{provider}:one")),
            provider: ProviderId(provider.into()),
            title: "Result".into(),
            subtitle: None,
            keywords: Vec::new(),
            icon: None,
            score: 0.0,
            payload: serde_json::Value::Null,
        };
        let options = WebPreviewOptions::default();
        // Nothing selected, or an app/file row: WebKit must stay unloaded.
        assert!(!options.should_prewarm(None));
        assert!(!options.should_prewarm(Some(&item("apps"))));
        assert!(!options.should_prewarm(Some(&item("files"))));
        assert!(options.should_prewarm(Some(&item("web"))));

        let cold = WebPreviewOptions {
            prewarm: false,
            ..options.clone()
        };
        assert!(!cold.should_prewarm(Some(&item("web"))));
        // Opening results in the browser makes a warm panel pure waste.
        let external = WebPreviewOptions {
            inline: false,
            ..options
        };
        assert!(!external.should_prewarm(Some(&item("web"))));
    }

    #[test]
    fn web_search_url_is_bound_to_the_web_provider_and_http_schemes() {
        let item = |provider: &str, url: serde_json::Value| Item {
            id: ItemId("web:google".into()),
            provider: ProviderId(provider.into()),
            title: "Search with Google".into(),
            subtitle: Some("Web".into()),
            keywords: Vec::new(),
            icon: None,
            score: 0.0,
            payload: serde_json::json!({"url": url}),
        };
        let search = Action {
            id: ActionId("search".into()),
            title: "Search".into(),
        };
        assert_eq!(
            web_search_url(
                &item(
                    "web",
                    serde_json::json!("https://www.google.com/search?q=rust")
                ),
                &search
            )
            .as_deref(),
            Some("https://www.google.com/search?q=rust")
        );
        assert!(
            web_search_url(
                &item("apps", serde_json::json!("https://example.com")),
                &search
            )
            .is_none()
        );
        assert!(
            web_search_url(
                &item("web", serde_json::json!("file:///etc/passwd")),
                &search
            )
            .is_none()
        );
        let copy = Action {
            id: ActionId("copy-url".into()),
            title: "Copy URL".into(),
        };
        assert!(
            web_search_url(
                &item("web", serde_json::json!("https://example.com")),
                &copy
            )
            .is_none()
        );
    }

    struct FixtureService {
        dir: tempfile::TempDir,
        stop: Arc<std::sync::atomic::AtomicBool>,
        worker: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
    }
    impl FixtureService {
        fn start() -> Self {
            Self::with_service(crate::service::Service::fixture().unwrap())
        }

        fn with_service(service: Arc<crate::service::Service>) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::Builder::new()
                .prefix("maccer-gpui-")
                .tempdir_in("/tmp")
                .unwrap();
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let server = crate::service::Server::bind(dir.path().join("rpc"), service).unwrap();
            let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_stop = stop.clone();
            let worker = std::thread::spawn(move || server.serve_until(&worker_stop));
            Self {
                dir,
                stop,
                worker: Some(worker),
            }
        }
        fn socket(&self) -> PathBuf {
            self.dir.path().join("rpc")
        }
    }
    impl Drop for FixtureService {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            if let Some(worker) = self.worker.take() {
                worker.join().unwrap().unwrap();
            }
        }
    }

    fn test_window(cx: &TestAppContext, registry: ProviderRegistry) -> WindowHandle<Launcher> {
        let history = Arc::new(HistoryStore::in_memory().unwrap());
        let coordinator = Arc::new(SearchCoordinator::new(
            Arc::new(registry),
            history.clone(),
            50,
        ));
        let window = cx.update(|cx| {
            open_launcher(
                coordinator,
                history,
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        // gpui's test platform does not implement `Platform::hide`. Turn off
        // the window-platform call so tests can exercise the hide path.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| launcher.hide_window = false)
                .unwrap()
        });
        window
    }

    #[gpui::test]
    fn backend_shortcut_opens_hidden_service_frontend_without_toggling(cx: &mut TestAppContext) {
        use std::os::unix::fs::PermissionsExt;
        let service = crate::service::Service::fixture().unwrap();
        let server = FixtureService::with_service(service.clone());
        let executable = server.dir.path().join("owned-frontend");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let router = crate::service::FrontendRouter::new(
            crate::protocol::FrontendKind::Gpui,
            executable,
            server.socket(),
        )
        .unwrap();
        service.attach_frontend(router.clone()).unwrap();
        let window = open_service(cx, &server.socket());
        assert!(cx.read(|cx| window.read(cx).unwrap().state.screen.is_none()));
        router.present().unwrap();
        poll_service_until(
            cx,
            &window,
            "backend presentation while hidden",
            |launcher| launcher.state.screen == Some(Screen::Search) && !launcher.state.searching,
        );
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.replace_query("Catppuccin".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "explicit query", |launcher| {
            !launcher.state.searching
        });
        router.present().unwrap();
        // Give the independent presentation connection time to deliver again.
        for _ in 0..40 {
            pump_service(cx, &window);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(launcher.state.screen, Some(Screen::Search));
            assert_eq!(launcher.state.query, "Catppuccin");
            assert!(launcher.coordinator.is_none());
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| launcher.hide(cx))
                .unwrap()
        });
        router.present().unwrap();
        poll_service_until(
            cx,
            &window,
            "backend presentation after cancel",
            |launcher| launcher.state.screen == Some(Screen::Search),
        );
    }

    #[gpui::test]
    fn shared_web_and_clipboard_work_through_gpui_without_embedded_providers(
        cx: &mut TestAppContext,
    ) {
        use std::sync::Mutex;
        #[derive(Default)]
        struct Desktop(Mutex<Vec<String>>);
        impl crate::macos::Platform for Desktop {
            fn launch_application(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn reveal_in_finder(&self, _: &std::path::Path) -> Result<()> {
                unreachable!()
            }
            fn open_url(&self, url: &str) -> Result<()> {
                self.0.lock().unwrap().push(format!("open:{url}"));
                Ok(())
            }
            fn copy_text(&self, text: &str) -> Result<()> {
                self.0.lock().unwrap().push(format!("copy:{text}"));
                Ok(())
            }
        }
        let desktop = Arc::new(Desktop::default());
        let clipboard = Arc::new(crate::providers::clipboard::ClipboardProvider::new(
            desktop.clone(),
            20,
        ));
        clipboard.capture(1, None);
        clipboard.capture(2, Some("Owned clipboard sample\nPreview body"));
        let web = crate::providers::web::WebProvider::new(
            desktop.clone(),
            crate::providers::web::WebProvider::default_engines(),
            "google".into(),
        )
        .unwrap();
        let service = crate::service::Service::new(
            vec![
                (
                    Arc::new(crate::service::WebServiceProvider::new(web)),
                    crate::core::ProviderConfig {
                        prefix: Some("?".into()),
                        ..Default::default()
                    },
                ),
                (
                    Arc::new(crate::service::ClipboardServiceProvider::new(clipboard)),
                    crate::core::ProviderConfig {
                        prefix: Some(":".into()),
                        ..Default::default()
                    },
                ),
            ],
            Arc::new(HistoryStore::in_memory().unwrap()),
        )
        .unwrap();
        let server = FixtureService::with_service(service);
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.show(window, cx);
                    launcher.replace_query("café & tea".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "Google result", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.title == "Search with Google")
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    assert!(launcher.coordinator.is_none());
                    assert!(
                        !launcher
                            .state
                            .items
                            .iter()
                            .any(|item| item.provider.0 == "clipboard")
                    );
                    launcher.on_confirm(&Confirm, window, cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "web action closes", |launcher| {
            launcher.state.screen.is_none()
        });
        assert_eq!(
            *desktop.0.lock().unwrap(),
            ["open:https://www.google.com/search?q=caf%C3%A9+%26+tea"]
        );
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.show(window, cx);
                    launcher.replace_query(":".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "scoped clipboard result", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.provider.0 == "clipboard")
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    assert!(
                        !launcher
                            .actions
                            .iter()
                            .any(|entry| entry.action.id.0 == "paste")
                    );
                    launcher.refresh_preview(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "clipboard text preview", |launcher| {
            launcher
                .service
                .as_ref()
                .and_then(|service| service.preview.as_ref())
                .is_some()
        });
        cx.update(|cx| window.update(cx, |launcher, _, cx| {
            let preview = launcher.service.as_ref().unwrap().preview.as_ref().unwrap();
            assert!(preview.blocks.iter().any(|block| matches!(block, crate::ui::service_preview::ServicePreviewBlock::Text { text, .. } if text.contains("Preview body"))));
            let item = launcher.state.items[0].clone();
            let pin = launcher
                .actions
                .iter()
                .find(|entry| entry.action.id.0 == "pin")
                .unwrap()
                .action
                .clone();
            launcher.activate(item, pin, cx);
        }).unwrap());
        poll_service_until(
            cx,
            &window,
            "pin refresh stays in clipboard prefix",
            |launcher| {
                !launcher.state.searching
                    && launcher.activation_in_flight.is_none()
                    && launcher
                        .state
                        .items
                        .iter()
                        .any(|item| item.subtitle.as_deref() == Some("Pinned"))
            },
        );
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    assert_eq!(launcher.state.query, ":");
                    launcher.on_confirm(&Confirm, window, cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "clipboard copy closes", |launcher| {
            launcher.state.screen.is_none()
        });
        assert_eq!(
            desktop.0.lock().unwrap().last().unwrap(),
            "copy:Owned clipboard sample\nPreview body"
        );
    }

    #[gpui::test]
    fn typing_keeps_service_visuals_but_revokes_old_action_and_detail_authority(
        cx: &mut TestAppContext,
    ) {
        let server = FixtureService::start();
        let window = open_service(cx, &server.socket());
        cx.update(|cx| window.update(cx, |launcher, _, cx| {
            launcher.state.screen = Some(Screen::Search);
            let source = "image://maccer-icons/retained";
            let event = serde_json::json!({"request":1,"route":{"type":"home"},"items":[{
                "id":"old","provider":"clipboard","title":"Old result","iconSource":source,
                "detailsAvailable":true,"actions":[{"id":"copy","title":"Copy"}]
            }]});
            let service = launcher.service.as_mut().unwrap();
            let (_, items) = service.apply_results(&event).unwrap();
            launcher.state.generation = 1;
            launcher.state.apply_results(1, items);
            service.preview = Some(crate::ui::service_preview::ServicePreview::from_event("clipboard".into(), "old".into(), &[serde_json::json!({"type":"text","text":"Old preview","fallbackText":"Old preview"})]));
            launcher.preview_key = Some(("clipboard".into(), "old".into(), 1));
            let image = crate::ui::preview_image::PreparedPreview::new(Preview::Pixels { bgra: vec![1,2,3,255], width:1, height:1 }).unwrap().image.unwrap();
            launcher.cache_service_icon(PathBuf::from(source), image);
            let width = launcher.effective_width();
            let old_item = launcher.state.items[0].clone();
            launcher.replace_query("new query".into(), cx);
            launcher.start_search(cx);
            launcher.refresh_preview(cx);
            assert!(launcher.state.searching);
            assert_eq!(launcher.state.items[0].id.0, "old");
            assert_eq!(launcher.effective_width(), width, "pending search must not collapse the preview panel");
            assert!(launcher.icon_cache.contains_key(&PathBuf::from(source)), "typing must not evict displayed icons");
            let service = launcher.service.as_mut().unwrap();
            assert_eq!(service.icon_sources.get(&("clipboard".into(), "old".into())).map(String::as_str), Some(source));
            assert!(service.preview.is_some());
            assert!(service.authorized_actions.is_empty());
            assert!(service.search_request.is_none());
            assert!(service.detail_selection.is_none());
            assert!(service.details_available.is_empty());
            assert!(service
                .activate(
                    &old_item,
                    &Action { id: ActionId("copy".into()), title: "Copy".into() },
                    Default::default(),
                )
                .is_err());
            assert!(service.apply_details(&serde_json::json!({"request":1,"detailRequest":1,"provider":"clipboard","id":"old","content":[]})).is_none());
            // When a replacement is delivered, old preview content must go away.
            let request = launcher.state.generation;
            let (_, items) = service.apply_results(&serde_json::json!({"request":request,"route":{"type":"home"},"items":[]})).unwrap();
            launcher.state.apply_results(request, items);
            launcher.refresh_preview(cx);
            assert!(!launcher.has_preview());
            assert!(launcher.service.as_ref().unwrap().preview.is_none());
        }).unwrap());
    }

    #[gpui::test]
    fn service_details_palette_and_expiry_refresh_keep_fresh_authority(cx: &mut TestAppContext) {
        let server = FixtureService::start();
        let window = cx.update(|cx| {
            open_service_launcher(
                &server.socket(),
                LauncherOptions {
                    width: 480.,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.hide_window = false;
                    launcher.show(window, cx);
                    launcher.replace_query("Catppuccin".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        let wait = |cx: &mut TestAppContext, details: bool| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                cx.update(|cx| {
                    window
                        .update(cx, |launcher, _, cx| {
                            launcher.poll_service(cx);
                            if details {
                                launcher.refresh_preview(cx);
                            }
                        })
                        .unwrap()
                });
                if cx.read(|cx| {
                    let launcher = window.read(cx).unwrap();
                    !launcher.state.searching
                        && (!details || launcher.service.as_ref().unwrap().preview.is_some())
                }) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "service details timeout"
                );
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        };
        wait(cx, false);
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.provider.0 == "themes")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                })
                .unwrap()
        });
        wait(cx, true);
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            let preview = launcher.service.as_ref().unwrap().preview.as_ref().unwrap();
            assert_eq!(preview.provider, "themes");
            assert!(preview.blocks.iter().any(|block| matches!(block, crate::ui::service_preview::ServicePreviewBlock::Palette { colors, .. } if !colors.is_empty())));
            assert!(!launcher.service.as_ref().unwrap().authorized_actions.is_empty());
        });
        let old = cx.read(|cx| window.read(cx).unwrap().state.generation);
        cx.update(|cx| window.update(cx, |launcher, _, cx| {
            launcher.clear_preview(cx);
            let service = launcher.service.as_mut().unwrap();
            let (next, sent) = service.refresh(old);
            sent.unwrap();
            launcher.state.generation = next;
            launcher.state.searching = true;
            assert!(service.authorized_actions.is_empty());
            assert!(service.apply_details(&serde_json::json!({"request":old,"detailRequest":service.details_request,"provider":"themes","id":"themes:fixture","content":[]})).is_none());
        }).unwrap());
        wait(cx, false);
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(launcher.state.generation > old);
            assert!(
                !launcher
                    .service
                    .as_ref()
                    .unwrap()
                    .authorized_actions
                    .is_empty()
            );
        });
    }

    #[gpui::test]
    fn service_catalog_browse_renders_read_only_palette_details(cx: &mut TestAppContext) {
        // Two resolved catalog entries with colors distinct from the active theme.
        let mut dark = watched_palette("#010203", "#0d0e0f", "#101112");
        dark.slug = "night-blue".into();
        dark.name = "Night Blue".into();
        let preview_dark = dark.clone();
        let mut light = watched_palette("#fafafa", "#004488", "#dddddd");
        light.slug = "day-white".into();
        light.name = "Day White".into();
        light.variant = "light".into();
        let catalog = crate::service::ThemeCatalog::from_palettes(vec![dark, light]).unwrap();
        let active = crate::service::ThemerState::fixture();
        let active_before = active.snapshot();
        let server = catalog_service(catalog, active.clone());
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });

        // The home view lists the Themes provider entry next to any results.
        poll_service_until(cx, &window, "home menu", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "themes")
        });

        // Activating the entry navigates to the read-only provider route.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.id.0 == "themes")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "theme provider scope", |launcher| {
            !launcher.state.searching
                && launcher.service.as_ref().is_some_and(|s| s.provider_scope)
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "themes:night-blue")
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "themes:day-white")
        });

        // Selecting a theme requests lazy details through the generic preview path.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.id.0 == "themes:night-blue")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    assert!(
                        launcher.actions.is_empty(),
                        "read-only themes expose no actions"
                    );
                    launcher.refresh_preview(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "theme palette details", |launcher| {
            launcher
                .service
                .as_ref()
                .and_then(|service| service.preview.as_ref())
                .is_some()
        });
        let theme_before = cx.read(|cx| window.read(cx).unwrap().theme.clone());
        assert_ne!(
            theme_before,
            Theme::from_palette(&preview_dark),
            "the active window theme is not the previewed palette"
        );
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            let service = launcher.service.as_ref().unwrap();
            let preview = service.preview.as_ref().unwrap();
            assert_eq!(preview.provider, "themes");
            assert_eq!(preview.id, "themes:night-blue");
            let swatches: Vec<_> = preview
                .blocks
                .iter()
                .filter_map(|block| match block {
                    crate::ui::service_preview::ServicePreviewBlock::Palette { colors, .. } => {
                        Some(colors)
                    }
                    _ => None,
                })
                .collect();
            assert!(!swatches.is_empty(), "resolved palette blocks render");
            assert!(
                swatches.iter().all(|colors| colors.len() <= 16),
                "each palette block stays within the adapter bound"
            );
            assert!(preview.blocks.iter().any(|block| matches!(
                block,
                crate::ui::service_preview::ServicePreviewBlock::Metadata { .. }
            )));
            assert!(
                service.authorized_actions.values().all(Vec::is_empty),
                "theme results authorize no actions"
            );
        });

        // Return on a theme result neither applies a theme nor closes the window.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_confirm(&Confirm, window, cx)
                })
                .unwrap()
        });
        // Give any wrongly authorized action time to run before asserting.
        pump_for(cx, &window, 400);
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(launcher.theme, theme_before);
            assert!(launcher.activation_in_flight.is_none());
            assert_eq!(launcher.state.screen, Some(Screen::Search));
            assert!(
                launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "themes:night-blue"),
                "the confirmed view stays open"
            );
            assert!(launcher.service.as_ref().unwrap().preview.is_some());
        });
        // The shared active palette is unchanged by browsing and details.
        assert_eq!(active.snapshot(), active_before);
    }

    fn pump_for(cx: &mut TestAppContext, window: &WindowHandle<Launcher>, millis: u64) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(millis);
        while std::time::Instant::now() < deadline {
            pump_service(cx, window);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[gpui::test]
    fn service_frontend_searches_fixture_and_cmd_p_returns_home(cx: &mut TestAppContext) {
        let server = FixtureService::start();
        let window = cx.update(|cx| {
            open_service_launcher(
                &server.socket(),
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.hide_window = false;
                    launcher.show(window, cx);
                })
                .unwrap()
        });
        let wait_for_results = |cx: &mut TestAppContext, window: &WindowHandle<Launcher>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                cx.update(|cx| {
                    window
                        .update(cx, |launcher, _, cx| launcher.poll_service(cx))
                        .unwrap()
                });
                if cx.read(|cx| {
                    let launcher = window.read(cx).unwrap();
                    !launcher.state.searching && launcher.activation_in_flight.is_none()
                }) {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "service result timeout"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        wait_for_results(cx, &window);
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(launcher.coordinator.is_none());
            assert!(launcher.history.is_none());
            assert!(launcher.state.items.iter().any(|item| item.id.0 == "apps"));
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.id.0 == "apps")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    window.focus(&launcher.search_input.read(cx).focus_handle(cx));
                })
                .unwrap()
        });
        wait_for_results(cx, &window);
        cx.read(|cx| {
            assert!(
                window
                    .read(cx)
                    .unwrap()
                    .service
                    .as_ref()
                    .unwrap()
                    .provider_scope
            )
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_provider_picker(&ProviderPicker, window, cx)
                })
                .unwrap()
        });
        wait_for_results(cx, &window);
        cx.read(|cx| {
            assert!(
                !window
                    .read(cx)
                    .unwrap()
                    .service
                    .as_ref()
                    .unwrap()
                    .provider_scope
            )
        });
    }

    /// A complete, schema-1 mac-themer palette with the requested colors, so a
    /// watched file always passes the service's strict validation.
    fn watched_palette(
        surface: &str,
        accent: &str,
        selection: &str,
    ) -> crate::protocol::ThemePalette {
        fn colors(
            keys: &[&str],
            base: &str,
            overrides: &[(&str, &str)],
        ) -> std::collections::BTreeMap<String, String> {
            keys.iter()
                .map(|key| {
                    let value = overrides
                        .iter()
                        .find(|(name, _)| name == key)
                        .map(|(_, value)| *value)
                        .unwrap_or(base);
                    ((*key).to_owned(), value.to_owned())
                })
                .collect()
        }
        crate::protocol::ThemePalette {
            schema: 1,
            slug: "watched".into(),
            name: "Watched".into(),
            variant: "dark".into(),
            alpha_byte: 200,
            palette: colors(
                &crate::protocol::THEME_PALETTE_KEYS,
                "#000000",
                &[
                    ("accent", accent),
                    ("selection_background", selection),
                    ("selection_foreground", "#ffffff"),
                ],
            ),
            tokens: colors(
                &crate::protocol::THEME_TOKEN_KEYS,
                "#111111",
                &[("surface", surface), ("outline_variant", "#222222")],
            ),
            border: colors(
                &crate::protocol::THEME_BORDER_KEYS,
                "#333333",
                &[("active", "#444444")],
            ),
            bar: colors(
                &crate::protocol::THEME_BAR_KEYS,
                "#555555",
                &[("label", "#aaaaaa"), ("muted", "#777777")],
            ),
        }
    }

    fn write_theme(dir: &std::path::Path, palette: &crate::protocol::ThemePalette) {
        std::fs::write(
            dir.join("current.json"),
            serde_json::to_vec(palette).unwrap(),
        )
        .unwrap();
    }

    /// A service whose only shared state is a watched mac-themer directory and
    /// which has no providers, so theme delivery cannot depend on a search.
    fn watched_service(dir: &std::path::Path) -> FixtureService {
        let service = crate::service::Service::with_state(
            vec![],
            Arc::new(HistoryStore::in_memory().unwrap()),
            None,
            crate::service::ThemerState::watch(dir.to_path_buf()),
        )
        .unwrap();
        FixtureService::with_service(service)
    }

    /// A service whose only provider is the read-only theme catalog, with an
    /// independent active palette so previewing a theme cannot be confused
    /// with applying one.
    fn catalog_service(
        catalog: Arc<crate::service::ThemeCatalog>,
        theme: Arc<crate::service::ThemerState>,
    ) -> FixtureService {
        catalog_service_with(catalog, theme, false)
    }

    /// The production shape of a theme scope: a card that can Apply and the two
    /// page commands behind it.
    fn catalog_service_with_apply(
        catalog: Arc<crate::service::ThemeCatalog>,
        theme: Arc<crate::service::ThemerState>,
        install: bool,
    ) -> FixtureService {
        let provider = crate::service::ThemeServiceProvider::with_apply(
            catalog,
            crate::service::ThemeApply::new(std::path::PathBuf::from("/nonexistent/mac-themer")),
        )
        .with_theme(theme.clone());
        let provider: crate::service::ThemeServiceProvider = if install {
            provider.with_install(crate::service::ThemeInstall::new(std::path::PathBuf::from(
                "/nonexistent/mac-themer",
            )))
        } else {
            provider
        };
        let service = crate::service::Service::with_state(
            vec![(
                Arc::new(provider),
                crate::core::ProviderConfig {
                    enabled: true,
                    max_results: 50,
                    ..Default::default()
                },
            )],
            Arc::new(HistoryStore::in_memory().unwrap()),
            None,
            theme.clone(),
        )
        .unwrap();
        FixtureService::with_service(service)
    }

    /// A themes scope that also publishes install and remove, so a strip of
    /// palette cards has to share its geometry with the page's commands.
    fn catalog_service_with(
        catalog: Arc<crate::service::ThemeCatalog>,
        theme: Arc<crate::service::ThemerState>,
        install: bool,
    ) -> FixtureService {
        let provider = crate::service::ThemeServiceProvider::new(catalog).with_theme(theme.clone());
        let provider: crate::service::ThemeServiceProvider = if install {
            provider.with_install(crate::service::ThemeInstall::new(std::path::PathBuf::from(
                "/nonexistent/mac-themer",
            )))
        } else {
            provider
        };
        let provider: Arc<dyn crate::service::ServiceProvider> = Arc::new(provider);
        let service = crate::service::Service::with_state(
            vec![(provider, crate::core::ProviderConfig::default())],
            Arc::new(HistoryStore::in_memory().unwrap()),
            None,
            theme,
        )
        .unwrap();
        FixtureService::with_service(service)
    }

    /// A provider whose only action carries a plain confirmation prompt, so the
    /// frontend can prove it stages consent before any invoke reaches the backend.
    struct ConfirmationProvider {
        activations: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Provider for ConfirmationProvider {
        fn id(&self) -> ProviderId {
            ProviderId("confirm".into())
        }
        fn name(&self) -> &str {
            "Confirm"
        }
        fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
            Ok(["one", "two"]
                .into_iter()
                .map(|suffix| Item {
                    id: ItemId(format!("confirm:{suffix}")),
                    provider: ProviderId("confirm".into()),
                    title: format!("Apply {suffix}"),
                    subtitle: None,
                    keywords: vec![],
                    icon: None,
                    score: 0.0,
                    payload: serde_json::Value::Null,
                })
                .collect())
        }
        fn actions(&self, _: &Item) -> Vec<Action> {
            vec![Action {
                id: ActionId("apply".into()),
                title: "Apply".into(),
            }]
        }
        fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
            self.activations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ActionOutcome::KeepOpen("applied".into()))
        }
    }
    impl crate::service::ServiceProvider for ConfirmationProvider {
        fn description(&self) -> &str {
            "Confirmable demo provider"
        }
        fn kind(&self) -> &str {
            "confirm"
        }
        fn presentation(&self, _: &Item) -> crate::service::ItemPresentation {
            crate::service::ItemPresentation {
                kind: "confirm".into(),
                icon: None,
                content: vec![],
                data: None,
            }
        }
        fn action_confirmation(&self, _: &Item, _: &Action) -> Option<String> {
            Some("Apply thing? It changes shared state.".into())
        }
    }

    /// A browse provider whose `kind` is `wallpaper`, so the service publishes
    /// a filmstrip for its results.
    struct FilmstripProvider {
        activations: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Provider for FilmstripProvider {
        fn id(&self) -> ProviderId {
            ProviderId("walls".into())
        }
        fn name(&self) -> &str {
            "Walls"
        }
        fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
            Ok(["Wall A", "Wall B", "Wall C", "Wall D"]
                .into_iter()
                .enumerate()
                .map(|(index, title)| Item {
                    id: ItemId(format!("walls:{index}")),
                    provider: ProviderId("walls".into()),
                    title: title.into(),
                    subtitle: Some(format!("pack · {index}.webp")),
                    keywords: vec![],
                    icon: None,
                    score: -(index as f64),
                    payload: serde_json::Value::Null,
                })
                .collect())
        }
        fn actions(&self, _: &Item) -> Vec<Action> {
            vec![Action {
                id: ActionId("set".into()),
                title: "Set wallpaper".into(),
            }]
        }
        fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
            self.activations
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ActionOutcome::KeepOpen("Wallpaper set.".into()))
        }
    }
    impl crate::service::ServiceProvider for FilmstripProvider {
        fn description(&self) -> &str {
            "Browse every installed wallpaper."
        }
        fn kind(&self) -> &str {
            "wallpaper"
        }
        fn presentation(&self, item: &Item) -> crate::service::ItemPresentation {
            crate::service::ItemPresentation {
                kind: "wallpaper".into(),
                icon: Some(crate::protocol::Icon::Glyph {
                    name: "wallpaper".into(),
                    fallback_text: item.title.chars().next().unwrap_or('◧').to_string(),
                }),
                content: vec![],
                data: Some(crate::protocol::DomainData::WallpaperSummary {
                    schema_version: 1,
                    theme_slug: "pack".into(),
                    wallpaper_id: format!("{}.webp", item.title.replace(' ', "-").to_lowercase()),
                }),
            }
        }
    }

    fn filmstrip_service(activations: Arc<std::sync::atomic::AtomicUsize>) -> FixtureService {
        let provider: Arc<dyn crate::service::ServiceProvider> =
            Arc::new(FilmstripProvider { activations });
        let service = crate::service::Service::new(
            vec![(
                provider,
                crate::core::ProviderConfig {
                    // A picker is browsed by prefix, exactly like `/walls`.
                    prefix: Some("/walls".into()),
                    default_search: false,
                    ..Default::default()
                },
            )],
            Arc::new(HistoryStore::in_memory().unwrap()),
        )
        .unwrap();
        FixtureService::with_service(service)
    }

    /// A theme scope is the same strip at its own size, drawing palette cards:
    /// the theme's surface, its six accents, its name and a dot when the desktop
    /// is wearing it.
    #[gpui::test]
    fn the_provider_menu_is_a_list_of_names_only(cx: &mut TestAppContext) {
        let server = FixtureService::start();
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });
        poll_service_until(cx, &window, "home menu", |launcher| {
            !launcher.state.searching && !launcher.state.items.is_empty()
        });
        let first = cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(
                launcher
                    .state
                    .items
                    .iter()
                    .all(|item| item.provider.0 == "menu"),
                "the home view is the provider menu"
            );
            launcher.state.items[0].id.0.clone()
        });
        let view = window.root(cx).unwrap();
        let mut visual = VisualTestContext::from_window(*window, cx);
        let bounds = cx.update(|cx| window.update(cx, |_, window, _| window.bounds()).unwrap());
        visual.simulate_resize(bounds.size);
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(bounds.size.height),
            ),
            |_, _| view.clone().into_any_element(),
        );
        // A name, on the 16px gutter: no icon slot, no subtitle.
        let row = visual
            .debug_bounds(Box::leak(format!("suggestion-{first}").into_boxed_str()))
            .expect("the provider row is painted");
        let title = visual
            .debug_bounds(Box::leak(
                format!("suggestion-title-{first}").into_boxed_str(),
            ))
            .expect("the provider name is painted");
        assert!(
            visual
                .debug_bounds(Box::leak(
                    format!("suggestion-icon-{first}").into_boxed_str()
                ))
                .is_none(),
            "the menu has no icon slot"
        );
        assert!(
            (f32::from(title.left()) - (f32::from(row.left()) + 8.)).abs() < 1.,
            "the name starts on the row's 8px padding: {:?}",
            title
        );
        assert!(
            (f32::from(title.size.height) - 18.).abs() < 1.,
            "one line, no subtitle: {:?}",
            title.size
        );
    }

    #[gpui::test]
    fn reopening_service_home_paints_cached_menu_before_the_refresh(cx: &mut TestAppContext) {
        let server = FixtureService::start();
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });
        poll_service_until(cx, &window, "initial home menu", |launcher| {
            !launcher.state.searching
                && !launcher.state.items.is_empty()
                && launcher
                    .state
                    .items
                    .iter()
                    .all(|item| item.provider.0 == "menu")
        });
        let home_ids: Vec<_> = cx.read(|cx| {
            window
                .read(cx)
                .unwrap()
                .state
                .items
                .iter()
                .map(|item| item.id.0.clone())
                .collect()
        });

        // Enter a provider scope so reopening must not reuse that route.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.id.0 == "apps")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "provider scope", |launcher| {
            !launcher.state.searching
                && launcher
                    .service
                    .as_ref()
                    .is_some_and(|service| service.provider_scope)
        });

        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| launcher.hide(cx))
                .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });

        // The first frame after show must already be the home menu: no poll or
        // IPC result has been processed yet, and no loading state is visible.
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(launcher.state.screen == Some(Screen::Search));
            assert!(!launcher.state.searching);
            assert!(!launcher.state.items.is_empty());
            assert!(
                launcher
                    .state
                    .items
                    .iter()
                    .all(|item| item.provider.0 == "menu"),
                "reopening must paint the cached home menu, not provider rows"
            );
            assert_eq!(
                launcher
                    .state
                    .items
                    .iter()
                    .map(|item| item.id.0.clone())
                    .collect::<Vec<_>>(),
                home_ids
            );
        });
        let height = cx.update(|cx| {
            window
                .update(cx, |_, window, _| window.bounds().size.height)
                .unwrap()
        });
        assert!(
            height > px(120.),
            "the cached home menu must paint at its list height immediately, got {height:?}"
        );

        // The background home search replaces the cache without changing the
        // visible shape of the screen.
        poll_service_until(cx, &window, "refreshed home menu", |launcher| {
            !launcher.state.searching
                && !launcher.state.items.is_empty()
                && launcher
                    .state
                    .items
                    .iter()
                    .all(|item| item.provider.0 == "menu")
        });
    }

    #[gpui::test]
    fn a_theme_picker_paints_palette_cards_on_the_smaller_strip(cx: &mut TestAppContext) {
        let mut night = watched_palette("#010203", "#0d0e0f", "#101112");
        night.slug = "night-blue".into();
        night.name = "Night Blue".into();
        let catalog = crate::service::ThemeCatalog::from_palettes(vec![night]).unwrap();
        // The desktop already wears this one, so its card carries the marker.
        let active = crate::service::ThemerState::fixture_named("night-blue");
        let server = catalog_service(catalog, active);
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.options.width = 480.;
                    launcher.show(window, cx);
                    launcher.replace_query("night".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "theme cards", |launcher| {
            launcher.state.items.len() == 1
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(launcher.is_filmstrip(), "a theme scope is a filmstrip");
            assert!(
                launcher.is_palette_strip(),
                "every frame carries a palette card"
            );
            assert_eq!(launcher.effective_width(), THEME_STRIP.width());
            assert_eq!(THEME_STRIP.width(), 760.);
            assert_eq!(
                launcher.strip_geometry().window_height(),
                THEME_STRIP.window_height()
            );
            // 2 border + 52 bar + 16 + 176 frame + 16 + 22 footer.
            assert_eq!(THEME_STRIP.window_height(), 284.);
        });

        let view = window.root(cx).unwrap();
        let mut visual = VisualTestContext::from_window(*window, cx);
        let width = THEME_STRIP.width();
        let height = THEME_STRIP.window_height();
        visual.simulate_resize(size(px(width), px(height)));
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(width)),
                AvailableSpace::Definite(px(height)),
            ),
            |_, _| view.clone().into_any_element(),
        );
        let focused = visual
            .debug_bounds("filmstrip-tile-0")
            .expect("the palette card is painted");
        assert!(
            (f32::from(focused.size.width) - 248.).abs() < 1.,
            "the focused card is 248 wide: {:?}",
            focused.size
        );
        assert!(
            (f32::from(focused.size.height) - 176.).abs() < 1.,
            "the focused card is 176 tall: {:?}",
            focused.size
        );
        // 1 border + 52 bar + 16 of air: the card never touches the search rule.
        assert!(
            (f32::from(focused.origin.y) - 69.).abs() < 1.,
            "the card sits 16 below the bar rule: {:?}",
            focused.origin
        );
        assert!(
            visual.debug_bounds("filmstrip-live-0").is_some(),
            "the applied theme is marked"
        );
    }

    #[gpui::test]
    fn a_wallpaper_picker_paints_a_filmstrip_and_steps_between_tiles(cx: &mut TestAppContext) {
        let activations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = filmstrip_service(activations.clone());
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.options.width = 480.;
                    launcher.show(window, cx);
                    launcher.replace_query("/walls".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "wallpaper tiles", |launcher| {
            launcher.state.items.len() == 4
        });
        // The provider owns the body shape, and the strip is wider than a list.
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(launcher.is_filmstrip(), "a wallpaper scope is a filmstrip");
            assert_eq!(launcher.effective_width(), WALLPAPER_STRIP.width());
            assert_eq!(
                launcher.service.as_ref().unwrap().layout,
                crate::ui::service_backend::ServiceLayout::Filmstrip
            );
            assert_eq!(
                selected_result_index(&launcher.state.items, &launcher.state.selection),
                Some(0)
            );
        });

        let body = |cx: &mut TestAppContext| {
            let view = window.root(cx).unwrap();
            let mut visual = VisualTestContext::from_window(*window, cx);
            let height = WALLPAPER_STRIP.window_height();
            visual.simulate_resize(size(px(WALLPAPER_STRIP.width()), px(height)));
            visual.draw(
                point(px(0.), px(0.)),
                size(
                    AvailableSpace::Definite(px(WALLPAPER_STRIP.width())),
                    AvailableSpace::Definite(px(height)),
                ),
                |_, _| view.clone().into_any_element(),
            );
            visual
        };
        let mut visual = body(cx);
        assert!(
            visual.debug_bounds("filmstrip").is_some(),
            "the strip is painted"
        );
        assert!(
            visual.debug_bounds("filmstrip-tile-0").is_some(),
            "the focused tile is painted"
        );
        assert!(
            visual.debug_bounds("filmstrip-tile-1").is_some(),
            "its neighbour is painted"
        );
        let focused = visual.debug_bounds("filmstrip-tile-0").unwrap();
        let neighbour = visual.debug_bounds("filmstrip-tile-1").unwrap();
        assert!(
            focused.size.width > neighbour.size.width,
            "the focused frame is the largest: {:?} vs {:?}",
            focused.size,
            neighbour.size
        );
        // The strip wraps: the last wallpaper sits to the left of the first,
        // so a five-frame window is always full.
        assert!(
            visual.debug_bounds("filmstrip-tile-2").is_some(),
            "the second frame to the right is painted"
        );
        assert!(
            visual.debug_bounds("filmstrip-tile-3").is_some(),
            "the strip rotates: the last wallpaper is already on the left"
        );
        // The frames butt-join: the neighbour starts exactly where the focused
        // frame ends, which is what makes the row read as one continuous strip.
        let gap: f32 = (neighbour.origin.x - (focused.origin.x + focused.size.width)).into();
        assert!(gap.abs() < 1., "frames are continuous, gap was {gap}");

        // Stepping right moves the selection; the chase settles on it.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_move_down(&MoveDown, window, cx)
                })
                .unwrap()
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                selected_result_index(&launcher.state.items, &launcher.state.selection),
                Some(1)
            );
        });
        for _ in 0..90 {
            pump_service(cx, &window);
        }
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(
                (launcher.filmstrip_pos - 1.).abs() < 0.05,
                "the strip settles on the selection, got {}",
                launcher.filmstrip_pos
            );
        });
        // Plain arrows belong to the strip, not to the caret, while it shows.
        let mut visual = body(cx);
        visual.simulate_keystrokes("right");
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                selected_result_index(&launcher.state.items, &launcher.state.selection),
                Some(2),
                "the right arrow steps the strip"
            );
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_move_up(&MoveUp, window, cx)
                })
                .unwrap()
        });

        // Stepping past the last frame wraps to the first and keeps the strip
        // travelling forward: an endless rotation, never a rewind.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    for _ in 0..3 {
                        launcher.on_move_down(&MoveDown, window, cx);
                    }
                })
                .unwrap()
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                selected_result_index(&launcher.state.items, &launcher.state.selection),
                Some(0),
                "the last frame wraps to the first"
            );
        });
        for _ in 0..140 {
            pump_service(cx, &window);
        }
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(
                launcher.filmstrip_pos > 3.9,
                "the strip rotated forward past the end, got {}",
                launcher.filmstrip_pos
            );
        });
        // And the other way: from the first frame, left lands on the last one.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_move_up(&MoveUp, window, cx)
                })
                .unwrap()
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                selected_result_index(&launcher.state.items, &launcher.state.selection),
                Some(3)
            );
        });

        // Enter runs the wallpaper action, not a row confirmation.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_confirm(&Confirm, window, cx)
                })
                .unwrap()
        });
        pump_for(cx, &window, 300);
        assert_eq!(
            activations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "confirming a tile sets that wallpaper exactly once"
        );
    }

    fn confirmation_service(activations: Arc<std::sync::atomic::AtomicUsize>) -> FixtureService {
        let provider: Arc<dyn crate::service::ServiceProvider> =
            Arc::new(ConfirmationProvider { activations });
        let service = crate::service::Service::new(
            vec![(provider, crate::core::ProviderConfig::default())],
            Arc::new(HistoryStore::in_memory().unwrap()),
        )
        .unwrap();
        FixtureService::with_service(service)
    }

    fn open_service(cx: &mut TestAppContext, socket: &std::path::Path) -> WindowHandle<Launcher> {
        let window = cx.update(|cx| {
            open_service_launcher(
                socket,
                LauncherOptions {
                    width: 480.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        // gpui's test platform does not implement `Platform::hide`.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| launcher.hide_window = false)
                .unwrap()
        });
        window
    }

    fn pump_service(cx: &mut TestAppContext, window: &WindowHandle<Launcher>) {
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.poll_service_window(window, cx)
                })
                .unwrap()
        });
    }

    fn poll_service_until(
        cx: &mut TestAppContext,
        window: &WindowHandle<Launcher>,
        what: &str,
        mut done: impl FnMut(&Launcher) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            pump_service(cx, window);
            if cx.read(|cx| done(window.read(cx).unwrap())) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    fn open_confirmable(
        cx: &mut TestAppContext,
        server: &FixtureService,
    ) -> WindowHandle<Launcher> {
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.show(window, cx);
                    launcher.replace_query("apply".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "confirmable result", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "confirm:one")
        });
        window
    }

    fn select_confirmable(launcher: &mut Launcher, cx: &mut Context<Launcher>) {
        let item = launcher
            .state
            .items
            .iter()
            .find(|item| item.id.0 == "confirm:one")
            .unwrap()
            .clone();
        launcher
            .state
            .selection
            .reconcile(std::slice::from_ref(&item));
        launcher.refresh_actions();
        launcher.activate(
            launcher.actions[0].item.clone(),
            launcher.actions[0].action.clone(),
            cx,
        );
    }

    #[gpui::test]
    fn service_action_confirmation_stages_then_sends_once(cx: &mut TestAppContext) {
        let activations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = confirmation_service(activations.clone());
        let window = open_confirmable(cx, &server);

        // Selecting the action stages the prompt; nothing is sent to the backend.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    select_confirmable(launcher, cx);
                    let service = launcher.service.as_ref().unwrap();
                    assert!(service.pending_confirmation.is_some());
                    assert_eq!(
                        service.pending_confirmation.as_ref().unwrap().prompt,
                        "Apply thing? It changes shared state."
                    );
                    assert!(service.pending_action.is_none());
                    assert!(launcher.activation_in_flight.is_none());
                })
                .unwrap()
        });
        // The prompt is modal: navigation cannot move selection out from under it.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    let before = launcher
                        .state
                        .selection
                        .selected()
                        .map(|(p, i)| (p.to_owned(), i.to_owned()));
                    launcher.on_move_down(&MoveDown, window, cx);
                    assert_eq!(
                        launcher
                            .state
                            .selection
                            .selected()
                            .map(|(p, i)| (p.to_owned(), i.to_owned())),
                        before,
                        "selection is frozen while a confirmation is staged"
                    );
                    assert!(
                        launcher
                            .service
                            .as_ref()
                            .unwrap()
                            .pending_confirmation
                            .is_some()
                    );
                })
                .unwrap()
        });
        assert_eq!(
            activations.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no operation is sent before consent"
        );

        // Escape dismisses the prompt without sending anything.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_escape(&Escape, window, cx);
                    assert!(
                        launcher
                            .service
                            .as_ref()
                            .unwrap()
                            .pending_confirmation
                            .is_none()
                    );
                })
                .unwrap()
        });
        assert_eq!(activations.load(std::sync::atomic::Ordering::SeqCst), 0);

        // Re-stage and confirm; exactly one invoke reaches the provider.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_confirmable(launcher, cx);
                    assert!(
                        launcher
                            .service
                            .as_ref()
                            .unwrap()
                            .pending_confirmation
                            .is_some()
                    );
                    launcher.on_confirm(&Confirm, window, cx);
                    assert!(
                        launcher
                            .service
                            .as_ref()
                            .unwrap()
                            .pending_confirmation
                            .is_none()
                    );
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "confirmed activation", |_| {
            activations.load(std::sync::atomic::Ordering::SeqCst) == 1
        });
        cx.run_until_parked();
        assert_eq!(activations.load(std::sync::atomic::Ordering::SeqCst), 1);
        cx.read(|cx| {
            assert_eq!(window.read(cx).unwrap().error.as_deref(), Some("applied"));
        });
    }

    /// Values the option provider received, in the order it received them.
    type Recorded =
        Arc<std::sync::Mutex<Vec<(String, std::collections::BTreeMap<String, String>)>>>;

    /// A provider whose two rows are options: one asks for a text value, one
    /// asks which of two choices to run and then wants consent. Every word the
    /// frontend draws comes from here, so a test that passes proves the dialog
    /// is provider-driven rather than the launcher's own popup.
    struct OptionProvider {
        activated: Recorded,
    }
    impl Provider for OptionProvider {
        fn id(&self) -> ProviderId {
            ProviderId("options".into())
        }
        fn name(&self) -> &str {
            "Options"
        }
        fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
            Ok([("ins", "Install an option"), ("rem", "Remove an option")]
                .into_iter()
                .enumerate()
                .map(|(index, (suffix, title))| Item {
                    id: ItemId(format!("options:{suffix}")),
                    provider: ProviderId("options".into()),
                    title: title.into(),
                    subtitle: Some("option".into()),
                    keywords: vec![],
                    icon: None,
                    score: -(index as f64),
                    payload: serde_json::Value::Null,
                })
                .collect())
        }
        fn actions(&self, item: &Item) -> Vec<Action> {
            if item.id.0 == "options:new" {
                return vec![Action {
                    id: ActionId("add".into()),
                    title: "Add an option from a link".into(),
                }];
            }
            vec![Action {
                id: ActionId("run".into()),
                title: if item.id.0.ends_with("ins") {
                    "Install from GitHub".into()
                } else {
                    "Remove it".into()
                },
            }]
        }
        fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
            Ok(ActionOutcome::KeepOpen("activated without input".into()))
        }
        fn activate_with_input(
            &self,
            item: &Item,
            _: &Action,
            input: &std::collections::BTreeMap<String, String>,
        ) -> Result<ActionOutcome> {
            self.activated
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((item.id.0.clone(), input.clone()));
            Ok(ActionOutcome::KeepOpen("done".into()))
        }
    }
    impl crate::service::ServiceProvider for OptionProvider {
        fn scope_actions(&self) -> Vec<Item> {
            vec![Item {
                id: ItemId("options:new".into()),
                provider: ProviderId("options".into()),
                title: "Add an option from a link".into(),
                subtitle: Some("paste a link".into()),
                keywords: vec![],
                icon: None,
                score: 0.,
                payload: serde_json::Value::Null,
            }]
        }
        fn description(&self) -> &str {
            "Options that ask for a value"
        }
        fn kind(&self) -> &str {
            "option"
        }
        fn presentation(&self, _: &Item) -> crate::service::ItemPresentation {
            crate::service::ItemPresentation {
                kind: "option".into(),
                icon: None,
                content: vec![],
                data: None,
            }
        }
        fn action_input(
            &self,
            item: &Item,
            action: &Action,
        ) -> Option<crate::protocol::ActionInput> {
            if item.id.0 == "options:new" && action.id.0 == "add" {
                return Some(crate::protocol::ActionInput {
                    id: "link".into(),
                    kind: crate::protocol::ACTION_INPUT_TEXT.into(),
                    title: "Add an option".into(),
                    label: "Option link".into(),
                    placeholder: Some("https://example.com/<name>".into()),
                    submit_label: "Add".into(),
                    options: vec![],
                });
            }
            if item.id.0.ends_with("ins") {
                Some(crate::protocol::ActionInput {
                    id: "url".into(),
                    kind: crate::protocol::ACTION_INPUT_TEXT.into(),
                    title: "Install an option".into(),
                    label: "Repository URL".into(),
                    placeholder: Some("https://github.com/<owner>/<repo>".into()),
                    submit_label: "Install".into(),
                    options: vec![],
                })
            } else {
                Some(crate::protocol::ActionInput {
                    id: "slug".into(),
                    kind: crate::protocol::ACTION_INPUT_SELECT.into(),
                    title: "Remove an option".into(),
                    label: "Choose what to remove".into(),
                    placeholder: None,
                    submit_label: "Continue".into(),
                    options: ["aura", "jade"]
                        .into_iter()
                        .map(|value| crate::protocol::ActionInputOption {
                            value: value.into(),
                            label: value.to_uppercase(),
                        })
                        .collect(),
                })
            }
        }
        fn action_confirmation(&self, item: &Item, _: &Action) -> Option<String> {
            (!item.id.0.ends_with("ins"))
                .then(|| "Are you sure you want to remove this option?".into())
        }
    }

    fn option_service(activated: Recorded) -> FixtureService {
        let provider: Arc<dyn crate::service::ServiceProvider> =
            Arc::new(OptionProvider { activated });
        let service = crate::service::Service::new(
            vec![(provider, crate::core::ProviderConfig::default())],
            Arc::new(HistoryStore::in_memory().unwrap()),
        )
        .unwrap();
        FixtureService::with_service(service)
    }

    fn open_options(cx: &mut TestAppContext, server: &FixtureService) -> WindowHandle<Launcher> {
        let window = open_service(cx, &server.socket());
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.show(window, cx);
                    // "an" matches both option titles without naming the
                    // provider, so the home view lists results rather than
                    // narrowing to the provider row.
                    launcher.replace_query("an".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "option rows", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .filter(|item| item.provider.0 == "options")
                    .count()
                    == 2
        });
        window
    }

    /// Navigates from the home view into the options provider's own scope, which
    /// is the only place its page commands are published.
    fn enter_options_scope(cx: &mut TestAppContext, window: &WindowHandle<Launcher>) {
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.show(window, cx);
                    launcher.replace_query(String::new(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        poll_service_until(cx, window, "provider menu", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "options")
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.id.0 == "options")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    launcher.resize_window(window);
                })
                .unwrap()
        });
        poll_service_until(cx, window, "options scope", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .filter(|item| item.provider.0 == "options")
                    .count()
                    == 2
        });
    }

    fn select_option(launcher: &mut Launcher, id: &str) {
        let item = launcher
            .state
            .items
            .iter()
            .find(|item| item.id.0 == id)
            .unwrap()
            .clone();
        launcher
            .state
            .selection
            .reconcile(std::slice::from_ref(&item));
        launcher.refresh_actions();
    }

    #[gpui::test]
    fn the_theme_scope_keeps_its_commands_out_of_the_strip(cx: &mut TestAppContext) {
        let palette = watched_palette("#010203", "#7aa2f7", "#101112");
        let catalog = crate::service::ThemeCatalog::from_palettes(vec![palette]).unwrap();
        let active = crate::service::ThemerState::fixture();
        let server = catalog_service_with_apply(catalog, active, true);
        let window = open_service(cx, &server.socket());
        let view = window.root(cx).unwrap();
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });
        poll_service_until(cx, &window, "themes home row", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "themes")
        });
        // Entering the scope lists the pack and only the pack: the two commands
        // belong to the page, not to its strip.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.id.0 == "themes")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    launcher.resize_window(window);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "themes scope", |launcher| {
            !launcher.state.searching
                && launcher
                    .service
                    .as_ref()
                    .is_some_and(|service| !service.scope_actions().is_empty())
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            let ids: Vec<&str> = launcher
                .state
                .items
                .iter()
                .map(|item| item.id.0.as_str())
                .collect();
            assert_eq!(ids, ["themes:watched"], "the strip is palettes only");
            assert!(launcher.is_filmstrip());
            assert!(launcher.is_palette_strip());
            assert_eq!(launcher.strip_geometry().width(), THEME_STRIP.width());

            // The commands are the page's, in the provider's own words, and the
            // selected card's Apply comes first so Return still applies.
            let panel: Vec<String> = launcher
                .actions
                .iter()
                .map(|entry| entry.action.title.clone())
                .collect();
            assert_eq!(
                panel,
                [
                    "Apply theme…",
                    "Install a theme from GitHub…",
                    "Uninstall a theme…"
                ]
            );
            assert_eq!(launcher.actions[1].item.id.0, "themes:install");
            assert_eq!(launcher.actions[2].item.id.0, "themes:uninstall");
        });
        let bounds = cx.update(|cx| window.update(cx, |_, window, _| window.bounds()).unwrap());
        let mut visual = VisualTestContext::from_window(*window, cx);
        visual.simulate_resize(bounds.size);
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(bounds.size.height),
            ),
            |_, _| view.clone().into_any_element(),
        );
        assert!(
            visual
                .debug_bounds(Box::leak("filmstrip-tile-0".to_string().into_boxed_str()))
                .is_some(),
            "the pack frame is painted"
        );
        assert!(
            visual
                .debug_bounds(Box::leak("filmstrip-tile-1".to_string().into_boxed_str()))
                .is_none(),
            "no command frame follows the pack"
        );
    }

    #[gpui::test]
    fn the_theme_scope_paints_a_failure_above_the_strip(cx: &mut TestAppContext) {
        let palette = watched_palette("#010203", "#7aa2f7", "#101112");
        let catalog = crate::service::ThemeCatalog::from_palettes(vec![palette]).unwrap();
        let active = crate::service::ThemerState::fixture();
        let server = catalog_service_with(catalog, active, true);
        let window = open_service(cx, &server.socket());
        let view = window.root(cx).unwrap();
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });
        poll_service_until(cx, &window, "themes home row", |launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.id.0 == "themes")
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.id.0 == "themes")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    launcher.resize_window(window);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "themes scope", |launcher| {
            !launcher.state.searching && launcher.state.items.len() == 1
        });
        // A strip has no list to hang a status line on, so a failed command still
        // gets its own line and the window grows to fit it.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, _| {
                    launcher.error = Some("The theme helper failed or did not finish.".into());
                    launcher.resize_window(window);
                })
                .unwrap()
        });
        let bounds = cx.update(|cx| window.update(cx, |_, window, _| window.bounds()).unwrap());
        let mut visual = VisualTestContext::from_window(*window, cx);
        visual.simulate_resize(bounds.size);
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(bounds.size.height),
            ),
            |_, _| view.clone().into_any_element(),
        );
        visual
            .debug_bounds("launcher-error")
            .expect("the failure is painted above the strip");
        cx.read(|cx| {
            assert_eq!(
                window.read(cx).unwrap().strip_geometry().window_height() + 24.,
                f32::from(bounds.size.height)
            );
        });
    }

    #[gpui::test]
    fn a_scope_command_lives_in_the_actions_panel_and_opens_its_dialog(cx: &mut TestAppContext) {
        let activated: Recorded = Default::default();
        let server = option_service(activated.clone());
        let window = open_service(cx, &server.socket());
        enter_options_scope(cx, &window);
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:ins");
                    // The panel is the row's own actions first, then the page's.
                    launcher.on_actions(&OpenActions, window, cx);
                    let titles: Vec<&str> = launcher
                        .actions
                        .iter()
                        .map(|entry| entry.action.title.as_str())
                        .collect();
                    assert_eq!(titles, ["Install from GitHub", "Add an option from a link"]);
                    assert_eq!(launcher.state.screen, Some(Screen::Actions));

                    // Choosing the command opens the field the provider published
                    // for it, and the command — not the selected row — is its
                    // subject.
                    launcher.on_move_down(&MoveDown, window, cx);
                    launcher.on_confirm(&Confirm, window, cx);
                    let pending = launcher.pending_input.as_ref().expect("command dialog");
                    assert_eq!(pending.item.id.0, "options:new");
                    assert_eq!(pending.action.id.0, "add");
                    assert_eq!(pending.field.id, "link");
                    assert_eq!(
                        launcher
                            .state
                            .selection
                            .selected()
                            .map(|(_, id)| id.to_owned()),
                        Some("options:ins".to_string()),
                        "a command runs without disturbing the selection"
                    );
                })
                .unwrap()
        });
        assert!(activated.lock().unwrap().is_empty());
    }

    #[gpui::test]
    fn tab_and_return_still_belong_to_the_selected_row(cx: &mut TestAppContext) {
        let activated: Recorded = Default::default();
        let server = option_service(activated.clone());
        let window = open_service(cx, &server.socket());
        enter_options_scope(cx, &window);
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:ins");
                    assert_eq!(launcher.actions.len(), 2);
                    // Tab is the shortcut for the selection's own action: with a
                    // single one it does nothing, and it never opens a command.
                    launcher.on_next_action(&NextAction, window, cx);
                    assert!(launcher.pending_input.is_none());
                    assert!(launcher.activation_in_flight.is_none());

                    // Return runs the selection's first action, not the command
                    // the page publishes behind it.
                    launcher.on_confirm(&Confirm, window, cx);
                    assert!(
                        launcher.pending_input.is_some(),
                        "own action asks for a value"
                    );
                    assert_eq!(
                        launcher.pending_input.as_ref().unwrap().item.id.0,
                        "options:ins"
                    );
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn service_action_input_dialog_collects_a_text_value_before_invoking(cx: &mut TestAppContext) {
        let activated: Recorded = Default::default();
        let server = option_service(activated.clone());
        let window = open_options(cx, &server);

        // The option is reachable through the actions panel, and choosing it
        // opens the provider's dialog instead of sending anything.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:ins");
                    launcher.on_actions(&OpenActions, window, cx);
                    assert_eq!(launcher.state.screen, Some(Screen::Actions));
                    assert_eq!(launcher.actions[0].action.title, "Install from GitHub");
                    launcher.on_confirm(&Confirm, window, cx);
                    let pending = launcher.pending_input.as_ref().expect("dialog opened");
                    assert_eq!(pending.field.title, "Install an option");
                    assert_eq!(pending.field.label, "Repository URL");
                    assert!(launcher.service.as_ref().unwrap().pending_action.is_none());
                })
                .unwrap()
        });
        assert!(activated.lock().unwrap().is_empty(), "nothing is sent yet");

        // Return with an empty field keeps the dialog up and says what is missing.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_confirm(&Confirm, window, cx);
                    assert!(launcher.pending_input.is_some());
                    assert_eq!(
                        launcher.error.as_deref(),
                        Some("Repository URL is required.")
                    );
                })
                .unwrap()
        });
        assert!(activated.lock().unwrap().is_empty());

        // Typing a value and pressing Return sends exactly it, once.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.dialog_input.update(cx, |input, cx| {
                        input.set_text("https://github.com/owner/aura".into(), cx)
                    });
                    launcher.on_confirm(&Confirm, window, cx);
                    assert!(launcher.pending_input.is_none());
                    assert!(launcher.error.is_none());
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "text activation", |_| {
            activated.lock().unwrap().len() == 1
        });
        cx.run_until_parked();
        let recorded = activated.lock().unwrap().clone();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "options:ins");
        assert_eq!(
            recorded[0].1,
            [("url".to_owned(), "https://github.com/owner/aura".to_owned())]
                .into_iter()
                .collect()
        );
    }

    #[gpui::test]
    fn service_choice_dialog_moves_then_consent_carries_the_choice(cx: &mut TestAppContext) {
        let activated: Recorded = Default::default();
        let server = option_service(activated.clone());
        let window = open_options(cx, &server);

        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:rem");
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    let pending = launcher.pending_input.as_ref().expect("choice dialog");
                    assert!(pending.field.is_select());
                    assert_eq!(pending.field.options.len(), 2);
                    // The choice list is modal: arrows move it, not the results.
                    launcher.on_move_down(&MoveDown, window, cx);
                    assert_eq!(launcher.pending_input.as_ref().unwrap().index, 1);
                    launcher.on_move_down(&MoveDown, window, cx);
                    assert_eq!(launcher.pending_input.as_ref().unwrap().index, 0);
                    launcher.on_move_up(&MoveUp, window, cx);
                    assert_eq!(launcher.pending_input.as_ref().unwrap().index, 1);
                })
                .unwrap()
        });
        assert!(activated.lock().unwrap().is_empty());

        // Submitting the choice asks for consent, and the collected value is
        // held with the staged action rather than sent early.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_confirm(&Confirm, window, cx);
                    assert!(launcher.pending_input.is_none());
                    let staged = launcher
                        .service
                        .as_ref()
                        .unwrap()
                        .pending_confirmation
                        .clone()
                        .expect("consent staged");
                    assert_eq!(
                        staged.prompt,
                        "Are you sure you want to remove this option?"
                    );
                    assert_eq!(staged.input.get("slug").map(String::as_str), Some("jade"));
                })
                .unwrap()
        });
        assert!(
            activated.lock().unwrap().is_empty(),
            "consent is not implied"
        );
        // Escape drops the staged action; the value is gone with it.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.on_escape(&Escape, window, cx);
                    assert!(
                        launcher
                            .service
                            .as_ref()
                            .unwrap()
                            .pending_confirmation
                            .is_none()
                    );
                })
                .unwrap()
        });
        assert!(activated.lock().unwrap().is_empty());

        // A second run confirms, and the invocation carries the choice.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:rem");
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    launcher.on_confirm(&Confirm, window, cx);
                    launcher.on_confirm(&Confirm, window, cx);
                })
                .unwrap()
        });
        poll_service_until(cx, &window, "choice activation", |_| {
            activated.lock().unwrap().len() == 1
        });
        cx.run_until_parked();
        let recorded = activated.lock().unwrap().clone();
        assert_eq!(recorded[0].0, "options:rem");
        assert_eq!(
            recorded[0].1,
            [("slug".to_owned(), "aura".to_owned())]
                .into_iter()
                .collect()
        );
    }

    #[gpui::test]
    fn service_action_input_dialog_paints_the_providers_words(cx: &mut TestAppContext) {
        let activated: Recorded = Default::default();
        let server = option_service(activated.clone());
        let window = open_options(cx, &server);
        let view = window.root(cx).unwrap();

        // The choice dialog: the provider's title, label, choices and submit text.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:rem");
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    launcher.resize_window(window);
                })
                .unwrap()
        });
        let bounds = cx.update(|cx| window.update(cx, |_, window, _| window.bounds()).unwrap());
        let mut visual = VisualTestContext::from_window(*window, cx);
        visual.simulate_resize(bounds.size);
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(bounds.size.height),
            ),
            |_, _| view.clone().into_any_element(),
        );
        let dialog = visual
            .debug_bounds("service-input-dialog")
            .expect("the provider dialog is painted");
        let title = visual.debug_bounds("service-input-title").unwrap();
        let label = visual.debug_bounds("service-input-label").unwrap();
        let first = visual.debug_bounds("service-input-option-0").unwrap();
        let second = visual.debug_bounds("service-input-option-1").unwrap();
        let submit = visual.debug_bounds("service-input-submit").unwrap();
        assert!(title.top() >= dialog.top());
        assert!(label.top() >= title.bottom() - px(1.));
        assert!(first.top() >= label.bottom() - px(1.));
        assert!(second.top() >= first.bottom() - px(1.));
        assert!(submit.top() >= second.bottom() - px(1.));
        assert!(submit.bottom() <= dialog.bottom() + px(1.));
        // Both choices are on screen, so a picker never has to guess.
        assert_eq!(first.size.height, second.size.height);
    }

    #[gpui::test]
    fn service_text_dialog_paints_the_providers_placeholder(cx: &mut TestAppContext) {
        let activated: Recorded = Default::default();
        let server = option_service(activated.clone());
        let window = open_options(cx, &server);
        let view = window.root(cx).unwrap();
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:ins");
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    launcher.resize_window(window);
                })
                .unwrap()
        });
        let bounds = cx.update(|cx| window.update(cx, |_, window, _| window.bounds()).unwrap());
        let mut visual = VisualTestContext::from_window(*window, cx);
        visual.simulate_resize(bounds.size);
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(bounds.size.width),
                AvailableSpace::Definite(bounds.size.height),
            ),
            |_, _| view.clone().into_any_element(),
        );
        let dialog = visual
            .debug_bounds("service-input-dialog")
            .expect("the text dialog is painted");
        // The dialog's own field owns the keyboard, so typing lands in the
        // value that is sent back rather than in the search query.
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    assert!(
                        launcher
                            .dialog_input
                            .read(cx)
                            .focus_handle(cx)
                            .is_focused(window),
                        "the dialog field takes focus"
                    );
                })
                .unwrap()
        });
        let placeholder = visual
            .debug_bounds("service-input-placeholder")
            .expect("the provider placeholder is painted");
        assert!(placeholder.top() >= dialog.top());
        // A text field carries no choices and no chooseable rows.
        assert!(visual.debug_bounds("service-input-option-0").is_none());
        assert!(visual.debug_bounds("service-input-submit").is_some());
        assert!(visual.debug_bounds("service-input-cancel").is_some());
        assert!(activated.lock().unwrap().is_empty());
    }

    #[gpui::test]
    fn service_action_input_dialog_is_cancelled_by_escape_and_blocks_navigation(
        cx: &mut TestAppContext,
    ) {
        let activated: Recorded = Default::default();
        let server = option_service(activated.clone());
        let window = open_options(cx, &server);

        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    select_option(launcher, "options:ins");
                    launcher.activate(
                        launcher.actions[0].item.clone(),
                        launcher.actions[0].action.clone(),
                        cx,
                    );
                    assert!(launcher.pending_input.is_some());
                    let selected = launcher
                        .state
                        .selection
                        .selected()
                        .map(|(provider, id)| (provider.to_owned(), id.to_owned()));
                    launcher.on_move_down(&MoveDown, window, cx);
                    assert_eq!(
                        launcher
                            .state
                            .selection
                            .selected()
                            .map(|(provider, id)| (provider.to_owned(), id.to_owned())),
                        selected,
                        "the dialog freezes selection"
                    );
                    launcher.on_escape(&Escape, window, cx);
                    assert!(launcher.pending_input.is_none());
                })
                .unwrap()
        });
        assert!(activated.lock().unwrap().is_empty());
    }

    #[gpui::test]
    fn service_staged_confirmation_is_dropped_when_the_view_changes(cx: &mut TestAppContext) {
        let activations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = confirmation_service(activations.clone());
        let window = open_confirmable(cx, &server);
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    select_confirmable(launcher, cx);
                    assert!(
                        launcher
                            .service
                            .as_ref()
                            .unwrap()
                            .pending_confirmation
                            .is_some()
                    );
                    // A new query starts a new view; old consent must not survive it.
                    launcher.replace_query("apply".into(), cx);
                    launcher.start_search(cx);
                    assert!(
                        launcher
                            .service
                            .as_ref()
                            .unwrap()
                            .pending_confirmation
                            .is_none()
                    );
                })
                .unwrap()
        });
        assert_eq!(activations.load(std::sync::atomic::Ordering::SeqCst), 0);
        poll_service_until(cx, &window, "reloaded results", |launcher| {
            !launcher.state.searching && !launcher.state.items.is_empty()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert!(launcher.service.as_mut().unwrap().confirm().is_err());
                })
                .unwrap()
        });
        assert_eq!(activations.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[gpui::test]
    fn service_confirmation_prompt_is_painted_as_plain_text(cx: &mut TestAppContext) {
        let activations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let server = confirmation_service(activations.clone());
        let window = open_confirmable(cx, &server);
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    select_confirmable(launcher, cx);
                    launcher.options.width = 480.;
                })
                .unwrap()
        });
        let view = window.root(cx).unwrap();
        let mut visual = VisualTestContext::from_window(*window, cx);
        visual.simulate_resize(size(px(480.), px(360.)));
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(480.)),
                AvailableSpace::Definite(px(360.)),
            ),
            |_, _| view.clone().into_any_element(),
        );
        assert!(
            visual.debug_bounds("service-confirmation").is_some(),
            "a confirmation panel is painted"
        );
        assert!(
            visual.debug_bounds("service-confirmation-prompt").is_some(),
            "the provider prompt is painted as plain text"
        );
        assert!(
            visual.debug_bounds("service-confirmation-apply").is_some(),
            "the explicit apply affordance is painted"
        );
        assert!(
            visual.debug_bounds("service-confirmation-cancel").is_some(),
            "the cancel affordance is painted"
        );
    }

    #[gpui::test]
    fn service_theme_state_is_delivered_and_repaints_the_input(cx: &mut TestAppContext) {
        let server = FixtureService::start();
        let window = open_service(cx, &server.socket());
        poll_service_until(cx, &window, "fixture theme", |launcher| {
            launcher.theme != theme()
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(
                launcher
                    .search_input
                    .read(cx)
                    .theme_matches(&launcher.theme),
                "the search input paints with the window palette"
            );
            assert!(
                launcher.theme_error.is_none(),
                "the fixture palette is valid"
            );
            // The window is solid: every role is opaque, including a palette
            // that still carries an alpha byte.
            assert_eq!(launcher.theme.background.a, 1.0);
            assert_eq!(launcher.theme.foreground.a, 1.0);
        });
    }

    #[gpui::test]
    fn service_theme_state_applies_without_search_and_keeps_last_good(cx: &mut TestAppContext) {
        let themes = tempfile::tempdir().unwrap();
        let first = watched_palette("#010203", "#040506", "#070809");
        write_theme(themes.path(), &first);
        let server = watched_service(themes.path());
        let window = open_service(cx, &server.socket());
        let first_theme = Theme::from_palette(&first);
        poll_service_until(cx, &window, "first palette", |launcher| {
            launcher.theme == first_theme
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                launcher.state.generation, 0,
                "theme delivery needs no search"
            );
            assert!(launcher.service.as_ref().unwrap().search_request.is_none());
            assert!(launcher.search_input.read(cx).theme_matches(&first_theme));
        });

        // Cancellation must not undo a delivered palette, and a later event still applies.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| launcher.hide(cx))
                .unwrap()
        });
        let second = watched_palette("#0a0b0c", "#0d0e0f", "#101112");
        write_theme(themes.path(), &second);
        let second_theme = Theme::from_palette(&second);
        poll_service_until(cx, &window, "second palette", |launcher| {
            launcher.theme == second_theme
        });

        // A malformed replacement keeps the last good palette and reports why.
        std::fs::write(themes.path().join("current.json"), b"{\"schema\":1}").unwrap();
        poll_service_until(cx, &window, "stale theme error", |launcher| {
            launcher.theme_error.is_some()
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                launcher.theme, second_theme,
                "last good palette is retained"
            );
            assert!(
                launcher.error.is_none(),
                "a theme problem must not overwrite the query error surface"
            );
        });

        // A fresh valid palette clears the error again.
        let third = watched_palette("#131415", "#161718", "#191a1b");
        write_theme(themes.path(), &third);
        let third_theme = Theme::from_palette(&third);
        poll_service_until(cx, &window, "third palette", |launcher| {
            launcher.theme == third_theme && launcher.theme_error.is_none()
        });
    }

    #[gpui::test]
    fn missing_palette_keeps_the_embedded_default_and_records_the_error(cx: &mut TestAppContext) {
        let themes = tempfile::tempdir().unwrap();
        std::fs::write(themes.path().join("current.json"), b"not json").unwrap();
        let server = watched_service(themes.path());
        let window = open_service(cx, &server.socket());
        poll_service_until(cx, &window, "theme error", |launcher| {
            launcher.theme_error.is_some()
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                launcher.theme,
                theme(),
                "no palette means the embedded default"
            );
            assert!(launcher.service.as_ref().unwrap().search_request.is_none());
        });
    }

    #[gpui::test]
    fn service_theme_state_is_independent_per_window(cx: &mut TestAppContext) {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let palette_a = watched_palette("#010203", "#040506", "#070809");
        let palette_b = watched_palette("#a0b0c0", "#d0e0f0", "#ffffff");
        write_theme(dir_a.path(), &palette_a);
        write_theme(dir_b.path(), &palette_b);
        let server_a = watched_service(dir_a.path());
        let server_b = watched_service(dir_b.path());
        let window_a = open_service(cx, &server_a.socket());
        let window_b = open_service(cx, &server_b.socket());
        let theme_a = Theme::from_palette(&palette_a);
        let theme_b = Theme::from_palette(&palette_b);
        poll_service_until(cx, &window_a, "window A palette", |launcher| {
            launcher.theme == theme_a
        });
        poll_service_until(cx, &window_b, "window B palette", |launcher| {
            launcher.theme == theme_b
        });
        assert_ne!(theme_a, theme_b);

        // Changing one window's source must not recolor the other window.
        let palette_a2 = watched_palette("#111213", "#141516", "#171819");
        write_theme(dir_a.path(), &palette_a2);
        let theme_a2 = Theme::from_palette(&palette_a2);
        poll_service_until(cx, &window_a, "window A palette update", |launcher| {
            launcher.theme == theme_a2
        });
        cx.read(|cx| {
            assert_eq!(
                window_b.read(cx).unwrap().theme,
                theme_b,
                "a second window keeps its own palette"
            );
        });
    }

    #[gpui::test]
    fn service_icons_render_on_cold_and_warm_views_without_embedded_backend(
        cx: &mut TestAppContext,
    ) {
        #[derive(Default)]
        struct FakePlatform;
        impl Platform for FakePlatform {
            fn launch_application(&self, _: &std::path::Path) -> anyhow::Result<()> {
                unreachable!()
            }
            fn reveal_in_finder(&self, _: &std::path::Path) -> anyhow::Result<()> {
                unreachable!()
            }
            fn open_url(&self, _: &str) -> anyhow::Result<()> {
                unreachable!()
            }
            fn copy_text(&self, _: &str) -> anyhow::Result<()> {
                unreachable!()
            }
        }
        struct FakeResolver(AtomicUsize);
        impl IconResolver for FakeResolver {
            fn resolve(
                &self,
                _: &std::path::Path,
            ) -> anyhow::Result<Option<ApplicationIconPixels>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(Some(ApplicationIconPixels {
                    bgra: vec![1, 2, 3, 255],
                    width: 1,
                    height: 1,
                }))
            }
        }

        let resolver = Arc::new(FakeResolver(AtomicUsize::new(0)));
        let app = Application {
            path: "/Applications/Fixture App.app".into(),
            bundle_id: Some("test.fixture-app".into()),
            display_name: "Fixture App".into(),
            unlocalized_name: None,
            executable_name: Some("Fixture App".into()),
            icon_path: None,
        };
        let provider =
            crate::providers::apps::ApplicationProvider::new(vec![app], Arc::new(FakePlatform));
        let service = application_service(
            provider,
            crate::core::ProviderConfig::default(),
            Arc::new(HistoryStore::in_memory().unwrap()),
            resolver.clone(),
        )
        .unwrap();
        let server = FixtureService::with_service(service);
        let window = cx.update(|cx| {
            open_service_launcher(
                &server.socket(),
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.hide_window = false;
                    launcher.show(window, cx);
                    launcher.replace_query("Fixture App".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });

        let wait_for_icon = |cx: &mut TestAppContext, window: &WindowHandle<Launcher>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                cx.update(|cx| {
                    window
                        .update(cx, |launcher, _, cx| launcher.poll_service(cx))
                        .unwrap()
                });
                let ready = cx.read(|cx| {
                    let launcher = window.read(cx).unwrap();
                    !launcher.state.searching
                        && launcher
                            .state
                            .items
                            .iter()
                            .any(|item| item.title == "Fixture App")
                        && launcher
                            .icon_cache
                            .keys()
                            .any(|key| key.to_string_lossy().starts_with("image://maccer-icons/"))
                });
                if ready {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "fixture app icon did not reach GPUI cache"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let assert_icon_state = |cx: &TestAppContext| {
            cx.read(|cx| {
                let launcher = window.read(cx).unwrap();
                let item = launcher.state.items.iter().find(|item| item.title == "Fixture App").unwrap();
                assert!(launcher.coordinator.is_none());
                assert!(launcher.history.is_none());
                assert!(launcher.paste_backend.is_none());
                assert!(launcher.icon_loading.is_empty());
                assert!(matches!(item.icon, Some(crate::core::IconDescriptor::Text(ref text)) if !text.starts_with("image://maccer-icons/")));
                let source = launcher.service.as_ref().unwrap().icon_sources
                    .get(&(item.provider.0.clone(), item.id.0.clone())).unwrap();
                assert!(source.starts_with("image://maccer-icons/"));
                assert!(launcher.icon_cache.contains_key(&PathBuf::from(source)));
                let token = source.rsplit('/').next().unwrap();
                let pixels = launcher.service.as_ref().unwrap().icon(token).unwrap();
                assert_eq!((pixels.width, pixels.height), (1, 1));
                assert_eq!(pixels.bgra, [1, 2, 3, 255]);
            });
        };

        wait_for_icon(cx, &window);
        assert_icon_state(cx);
        assert_eq!(resolver.0.load(Ordering::SeqCst), 1);

        cx.update(|cx| {
            window
                .update(cx, |launcher, cx, gpui_cx| {
                    launcher.icon_cache.clear();
                    launcher.replace_query("Fixture App".into(), gpui_cx);
                    launcher.start_search(gpui_cx);
                    let _ = cx;
                })
                .unwrap()
        });
        wait_for_icon(cx, &window);
        assert_icon_state(cx);
        assert_eq!(
            resolver.0.load(Ordering::SeqCst),
            1,
            "warm asset must come from service cache"
        );
    }

    #[gpui::test]
    fn queue_full_cancel_keeps_reopened_service_search_current(cx: &mut TestAppContext) {
        use std::sync::{Condvar, Mutex};

        struct BlockingPlatform {
            entered: AtomicUsize,
            released: Mutex<bool>,
            changed: Condvar,
        }
        impl Platform for BlockingPlatform {
            fn launch_application(&self, _: &std::path::Path) -> anyhow::Result<()> {
                self.entered.fetch_add(1, Ordering::SeqCst);
                self.changed.notify_all();
                let mut released = self.released.lock().unwrap();
                while !*released {
                    released = self.changed.wait(released).unwrap();
                }
                Ok(())
            }
            fn reveal_in_finder(&self, _: &std::path::Path) -> anyhow::Result<()> {
                unreachable!()
            }
            fn open_url(&self, _: &str) -> anyhow::Result<()> {
                unreachable!()
            }
            fn copy_text(&self, _: &str) -> anyhow::Result<()> {
                unreachable!()
            }
        }

        let platform = Arc::new(BlockingPlatform {
            entered: AtomicUsize::new(0),
            released: Mutex::new(false),
            changed: Condvar::new(),
        });
        struct ReleaseOnDrop(Arc<BlockingPlatform>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) {
                *self.0.released.lock().unwrap() = true;
                self.0.changed.notify_all();
            }
        }
        let _release_on_drop = ReleaseOnDrop(platform.clone());
        let app = Application {
            path: "/Applications/Fixture App.app".into(),
            bundle_id: Some("test.fixture-app".into()),
            display_name: "Fixture App".into(),
            unlocalized_name: None,
            executable_name: Some("Fixture App".into()),
            icon_path: None,
        };
        let provider =
            crate::providers::apps::ApplicationProvider::new(vec![app], platform.clone());
        struct NoIcons;
        impl IconResolver for NoIcons {
            fn resolve(
                &self,
                _: &std::path::Path,
            ) -> anyhow::Result<Option<ApplicationIconPixels>> {
                Ok(None)
            }
        }
        let service = application_service(
            provider,
            crate::core::ProviderConfig::default(),
            Arc::new(HistoryStore::in_memory().unwrap()),
            Arc::new(NoIcons),
        )
        .unwrap();
        let server = FixtureService::with_service(service.clone());
        let executable = server.dir.path().join("owned-frontend");
        std::fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let router = crate::service::FrontendRouter::new(
            crate::protocol::FrontendKind::Gpui,
            executable,
            server.socket(),
        )
        .unwrap();
        service.attach_frontend(router.clone()).unwrap();
        let window = cx.update(|cx| {
            open_service_launcher(
                &server.socket(),
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.hide_window = false;
                    launcher.show(window, cx);
                    launcher.replace_query("Fixture App".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });

        macro_rules! wait_for {
            ($condition:expr) => {{
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    cx.update(|cx| {
                        window
                            .update(cx, |launcher, _, cx| launcher.poll_service(cx))
                            .unwrap()
                    });
                    if cx.read(|cx| {
                        let launcher = window.read(cx).unwrap();
                        ($condition)(launcher)
                    }) {
                        break;
                    }
                    assert!(
                        std::time::Instant::now() < deadline,
                        "GPUI service/action event timed out"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            }};
        }
        wait_for!(|launcher: &Launcher| {
            !launcher.state.searching
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.title == "Fixture App")
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.title == "Fixture App")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    let open = launcher
                        .actions
                        .iter()
                        .find(|entry| entry.action.id.0 == "open")
                        .unwrap()
                        .action
                        .clone();
                    launcher.activate(item, open, cx);
                })
                .unwrap()
        });

        let entered_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut released = platform.released.lock().unwrap();
        while platform.entered.load(Ordering::SeqCst) < 1 {
            let remaining = entered_deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "first activation did not reach fake platform"
            );
            let (next, timeout) = platform.changed.wait_timeout(released, remaining).unwrap();
            released = next;
            assert!(
                !timeout.timed_out(),
                "first activation remained blocked before entry"
            );
        }
        drop(released);

        // Presentation uses its own connection and must work while the action
        // RPC is blocked. Hiding/canceling the view does not cancel the mutation.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| launcher.hide(cx))
                .unwrap()
        });
        router.present().unwrap();
        poll_service_until(
            cx,
            &window,
            "presentation during a blocked mutation",
            |launcher| launcher.state.screen == Some(Screen::Search),
        );
        assert!(!*platform.released.lock().unwrap());
        assert_eq!(platform.entered.load(Ordering::SeqCst), 1);

        // The mutation is accepted and blocked in the fake platform. Do not poll its outcome.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.replace_query("Fixture".into(), cx);
                    launcher.start_search(cx);
                })
                .unwrap()
        });
        {
            let mut released = platform.released.lock().unwrap();
            *released = true;
            platform.changed.notify_all();
        }
        wait_for!(|launcher: &Launcher| {
            !launcher.state.searching
                && launcher.state.query == "Fixture"
                && launcher
                    .state
                    .items
                    .iter()
                    .any(|item| item.title == "Fixture App")
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(
                platform.entered.load(Ordering::SeqCst),
                1,
                "the accepted action runs once"
            );
            assert_eq!(
                launcher.state.screen,
                Some(Screen::Search),
                "stale Close must not hide the newer view"
            );
            assert!(
                launcher.error.is_none(),
                "old action result must not leak into the newer query"
            );
            assert!(
                launcher.service.as_ref().unwrap().search_request.is_some(),
                "new view remains actionable"
            );
            assert!(
                launcher
                    .service
                    .as_ref()
                    .unwrap()
                    .authorized_actions
                    .values()
                    .any(|actions| actions.iter().any(|action| action.id.0 == "open"))
            );
        });

        *platform.released.lock().unwrap() = false;
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    let item = launcher
                        .state
                        .items
                        .iter()
                        .find(|item| item.title == "Fixture App")
                        .unwrap()
                        .clone();
                    launcher
                        .state
                        .selection
                        .reconcile(std::slice::from_ref(&item));
                    launcher.refresh_actions();
                    let open = launcher
                        .actions
                        .iter()
                        .find(|entry| entry.action.id.0 == "open")
                        .unwrap()
                        .action
                        .clone();
                    launcher.activate(item, open, cx);
                })
                .unwrap()
        });
        let entered_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut released = platform.released.lock().unwrap();
        while platform.entered.load(Ordering::SeqCst) < 2 {
            let remaining = entered_deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "second activation did not reach fake platform"
            );
            let (next, timeout) = platform.changed.wait_timeout(released, remaining).unwrap();
            released = next;
            assert!(
                !timeout.timed_out(),
                "second activation remained blocked before entry"
            );
        }
        drop(released);

        // The accepted activation blocks the actor, so these deterministically fill
        // its bounded queue without relying on IPC scheduling timing.
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    let service = launcher.service.as_mut().unwrap();
                    for _ in 0..16 {
                        assert!(service.search("Fixture App".into()).1.is_ok());
                    }
                    assert!(service.search("overflow".into()).1.is_err());
                    launcher.hide_window = false;
                    launcher.hide(cx);
                })
                .unwrap()
        });
        // Cancel was rejected, but still advanced the actor's request watermark.
        {
            let mut released = platform.released.lock().unwrap();
            *released = true;
            platform.changed.notify_all();
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });
        wait_for!(|launcher: &Launcher| {
            !launcher.state.searching && launcher.state.screen == Some(Screen::Search)
        });
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert_eq!(platform.entered.load(Ordering::SeqCst), 2);
            assert_eq!(
                launcher.state.generation,
                launcher.service.as_ref().unwrap().search_request.unwrap(),
                "reopened search uses the request ID after the rejected Cancel watermark"
            );
            assert!(
                launcher.state.items.iter().any(|item| item.id.0 == "apps"),
                "reopened service search must receive home results"
            );
            assert!(launcher.error.is_none());
        });
    }

    #[test]
    fn stale_service_action_outcomes_cannot_change_the_current_view() {
        assert!(!service_action_is_current(None, 4));
        assert!(!service_action_is_current(Some(5), 4));
        assert!(service_action_is_current(Some(4), 4));
    }

    #[gpui::test]
    fn service_escape_from_actions_returns_to_actions_before_home(cx: &mut TestAppContext) {
        let server = FixtureService::start();
        let window = cx.update(|cx| {
            open_service_launcher(
                &server.socket(),
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.service.as_mut().unwrap().provider_scope = true;
                    launcher.state.screen = Some(Screen::Actions);
                    launcher.state.query.clear();
                    launcher.on_escape(&Escape, window, cx);
                    assert_eq!(launcher.state.screen, Some(Screen::Search));
                    assert!(launcher.service.as_ref().unwrap().provider_scope);
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn cmd_p_invalidates_old_activation_before_returning_home(cx: &mut TestAppContext) {
        let server = FixtureService::start();
        let window = cx.update(|cx| {
            open_service_launcher(
                &server.socket(),
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.service.as_mut().unwrap().provider_scope = true;
                    launcher.activation_in_flight = Some(77);
                    launcher.state.screen = Some(Screen::Actions);
                    launcher.output = Some(("old".into(), "output".into()));
                    launcher.on_provider_picker(&ProviderPicker, window, cx);
                    assert_eq!(launcher.activation_in_flight, None);
                    assert_eq!(launcher.state.screen, Some(Screen::Search));
                    assert!(launcher.output.is_none());
                    assert!(launcher.state.query.is_empty());
                    assert!(!launcher.service.as_ref().unwrap().provider_scope);
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn service_frontend_reports_missing_daemon_without_embedded_fallback(cx: &mut TestAppContext) {
        let socket =
            std::env::temp_dir().join(format!("maccer-no-daemon-{}.sock", std::process::id()));
        let window = cx.update(|cx| {
            open_service_launcher(
                &socket,
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.hide_window = false;
                    launcher.show(window, cx);
                })
                .unwrap()
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            cx.update(|cx| {
                window
                    .update(cx, |launcher, _, cx| launcher.poll_service(cx))
                    .unwrap()
            });
            if cx.read(|cx| window.read(cx).unwrap().error.is_some()) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "missing-service error timeout"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        cx.read(|cx| {
            let launcher = window.read(cx).unwrap();
            assert!(launcher.coordinator.is_none());
            assert!(
                launcher.error.as_deref().unwrap().contains("No such file")
                    || launcher.error.as_deref().unwrap().contains("connect")
            );
        });
    }

    #[gpui::test]
    fn preview_and_result_resizes_keep_the_whole_launcher_centered(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        cx.update(|cx| window.update(cx, |launcher, window, cx| {
            let center = window.display(cx).unwrap().bounds().center();
            launcher.state.screen = Some(Screen::Search);
            for (count, preview) in [(0, false), (1, true), (8, true), (3, false), (50, true), (0, false)] {
                launcher.state.items = (0..count).map(|i| Item {
                    id: ItemId(i.to_string()), provider: ProviderId("files".into()),
                    title: "Result".into(), subtitle: None, keywords: vec![], icon: None,
                    score: 0., payload: serde_json::Value::Null,
                }).collect();
                launcher.preview_key = preview.then(|| ("files".into(), "image".into(), 0));
                launcher.resize_window(window);
                assert_eq!(window.bounds().center(), center,
                    "entire launcher must stay centered when preview={preview}, results={count}");
                assert_eq!(window.bounds().size.width, px(if preview { 960. } else { 680. }));
            }
            launcher.state.screen = Some(Screen::Output);
            launcher.resize_window(window);
            assert_eq!(window.bounds().center(), center);
        }).unwrap());
    }

    #[gpui::test]
    fn suggestions_fill_the_container_without_clipping(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let view = window.root(cx).unwrap();
        for count in [1, 3, 8] {
            cx.update(|cx| {
                window
                    .update(cx, |launcher, window, _| {
                        launcher.options.width = 480.;
                        launcher.state.screen = Some(Screen::Search);
                        launcher.state.items = (0..count)
                            .map(|i| Item {
                                id: ItemId(i.to_string()),
                                provider: ProviderId("apps".into()),
                                title:
                                    "Alacritty Terminal — Résumé gypj with a long application name"
                                        .into(),
                                subtitle: Some("Application — gypj".into()),
                                keywords: vec![],
                                icon: (i == 0).then(|| {
                                    crate::core::IconDescriptor::Png(Arc::from(
                                        include_bytes!("../../tests/fixtures/pixel.png").as_slice(),
                                    ))
                                }),
                                score: 0.,
                                payload: serde_json::Value::Null,
                            })
                            .collect();
                        launcher.state.selection.reconcile(&launcher.state.items);
                        launcher.resize_window(window);
                    })
                    .unwrap()
            });
            let bounds = cx.update(|cx| window.update(cx, |_, window, _| window.bounds()).unwrap());
            let mut visual = VisualTestContext::from_window(*window, cx);
            visual.simulate_resize(bounds.size);
            visual.draw(
                point(px(0.), px(0.)),
                size(
                    AvailableSpace::Definite(bounds.size.width),
                    AvailableSpace::Definite(bounds.size.height),
                ),
                |_, _| view.clone().into_any_element(),
            );
            let container = visual.debug_bounds("launcher-container").unwrap();
            let first = visual.debug_bounds("suggestion-0").unwrap();
            let icon = visual
                .debug_bounds("suggestion-icon-0")
                .expect("application icon slot");
            assert_eq!(icon.size, size(px(ICON_SIZE), px(ICON_SIZE)));
            assert!(icon.top() >= first.top() && icon.bottom() <= first.bottom());
            let title = visual.debug_bounds("suggestion-title-0").unwrap();
            assert!(title.left() >= icon.right() + px(10.));
            if count > 1 {
                let fallback = visual
                    .debug_bounds("suggestion-icon-1")
                    .expect("missing icon fallback");
                assert_eq!(fallback.size, icon.size);
            }
            assert!(
                (container.size.height - bounds.size.height).abs() <= px(1.),
                "container should fill window: {container:?}, window: {bounds:?}"
            );
            assert!(
                (first.size.width - (container.size.width - px(18.))).abs() <= px(1.),
                "suggestions should fill width with 8px margins: {first:?}, {container:?}"
            );
            let last = match count {
                1 => "suggestion-0",
                3 => "suggestion-2",
                _ => "suggestion-5",
            };
            let last = visual.debug_bounds(last).unwrap();
            assert_eq!(last.size.height, px(ROW_HEIGHT));
            assert!(
                last.bottom() <= container.bottom() - px(FOOTER_HEIGHT),
                "footer must not clip suggestions"
            );
            for (selector, min_height) in [("suggestion-title-0", 18.), ("suggestion-type-0", 14.)]
            {
                let text = visual.debug_bounds(selector).unwrap();
                assert!(
                    text.size.height >= px(min_height),
                    "line box must fit the glyphs: {text:?}"
                );
                assert!(
                    text.top() >= first.top() + px(2.) && text.bottom() <= first.bottom() - px(2.),
                    "title/type must fit inside the row without clipping: {text:?}, {first:?}"
                );
            }
        }
    }

    fn app_bundle_item(id: &str, path: &std::path::Path) -> Item {
        Item {
            id: ItemId(id.into()),
            provider: ProviderId("apps".into()),
            title: id.into(),
            subtitle: None,
            keywords: vec![],
            icon: Some(crate::core::IconDescriptor::ApplicationBundle(
                path.to_path_buf(),
            )),
            score: 0.,
            payload: serde_json::Value::Null,
        }
    }

    fn icon_image() -> Arc<gpui::RenderImage> {
        crate::ui::preview_image::PreparedPreview::new(Preview::Pixels {
            bgra: vec![0, 0, 255, 255],
            width: 1,
            height: 1,
        })
        .unwrap()
        .image
        .unwrap()
    }

    /// Records paste backend calls without touching the system clipboard or
    /// synthesizing keystrokes.
    #[derive(Default)]
    struct FakePaste {
        frontmost: std::sync::Mutex<Option<i32>>,
        prepared: std::sync::Mutex<Vec<(String, i32)>>,
        sent: std::sync::Mutex<Vec<i32>>,
        prepare_error: std::sync::Mutex<Option<String>>,
        send_error: std::sync::Mutex<Option<String>>,
    }

    impl FakePaste {
        fn backend(self: &Arc<Self>) -> PasteBackend {
            let frontmost = self.clone();
            let prepare = self.clone();
            let send = self.clone();
            PasteBackend {
                frontmost: Arc::new(move || *frontmost.frontmost.lock().unwrap()),
                prepare: Arc::new(move |text: &str, pid: i32| {
                    if let Some(error) = prepare.prepare_error.lock().unwrap().clone() {
                        anyhow::bail!(error);
                    }
                    prepare
                        .prepared
                        .lock()
                        .unwrap()
                        .push((text.to_string(), pid));
                    Ok(())
                }),
                send: Arc::new(move |pid: i32| {
                    if let Some(error) = send.send_error.lock().unwrap().clone() {
                        anyhow::bail!(error);
                    }
                    send.sent.lock().unwrap().push(pid);
                    Ok(())
                }),
            }
        }
    }

    #[gpui::test]
    fn refresh_search_outcome_returns_to_results_and_clears_transient_state(
        cx: &mut TestAppContext,
    ) {
        let window = test_window(cx, ProviderRegistry::new());
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.state.screen = Some(Screen::Output);
                    launcher.output = Some(("Command".into(), "text".into()));
                    launcher.action_item =
                        Some(app_bundle_item("x", std::path::Path::new("/x.app")));
                    launcher.preview_key = Some(("apps".into(), "x".into(), 0));
                    launcher.error = Some("stale".into());
                    launcher.apply_action_outcome(ActionOutcome::RefreshSearch, cx);
                    assert_eq!(launcher.state.screen, Some(Screen::Search));
                    assert!(launcher.output.is_none());
                    assert!(launcher.action_item.is_none());
                    assert!(launcher.preview_key.is_none());
                    assert!(launcher.error.is_none());
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn paste_target_capture_ignores_self_and_keeps_the_existing_target(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let fake = Arc::new(FakePaste::default());
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    launcher.paste_backend = Some(fake.backend());
                    launcher.state.screen = None;
                    launcher.paste_target = None;

                    // The launcher's own pid is not a valid paste target.
                    *fake.frontmost.lock().unwrap() = Some(std::process::id() as i32);
                    launcher.capture_paste_target();
                    assert!(launcher.paste_target.is_none());

                    // Another application is captured.
                    *fake.frontmost.lock().unwrap() = Some(4242);
                    launcher.capture_paste_target();
                    assert_eq!(launcher.paste_target, Some(4242));

                    // While already shown, an existing target is never replaced.
                    launcher.state.screen = Some(Screen::Search);
                    *fake.frontmost.lock().unwrap() = Some(9999);
                    launcher.capture_paste_target();
                    assert_eq!(launcher.paste_target, Some(4242));

                    // A later self/empty reading keeps the last external target.
                    launcher.state.screen = None;
                    *fake.frontmost.lock().unwrap() = Some(std::process::id() as i32);
                    launcher.capture_paste_target();
                    assert_eq!(launcher.paste_target, Some(4242));
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn paste_permission_failure_keeps_results_visible_and_sends_no_keystroke(
        cx: &mut TestAppContext,
    ) {
        let window = test_window(cx, ProviderRegistry::new());
        let fake = Arc::new(FakePaste::default());
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.paste_backend = Some(fake.backend());
                    launcher.state.screen = Some(Screen::Search);
                    launcher.paste_target = Some(4242);
                    *fake.prepare_error.lock().unwrap() =
                        Some("accessibility permission required".into());
                    launcher.apply_action_outcome(ActionOutcome::PasteText("secret".into()), cx);
                    // The launcher stays open so the failure is visible, and no
                    // Command-V is queued.
                    assert_eq!(launcher.state.screen, Some(Screen::Search));
                    assert!(launcher.paste_cancellation.is_none());
                    assert!(
                        launcher
                            .error
                            .as_deref()
                            .unwrap()
                            .contains("accessibility permission required")
                    );
                    assert!(fake.prepared.lock().unwrap().is_empty());
                    assert!(fake.sent.lock().unwrap().is_empty());
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn paste_outcome_hides_copies_and_sends_command_v_after_the_delay(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let fake = Arc::new(FakePaste::default());
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.paste_backend = Some(fake.backend());
                    launcher.state.screen = Some(Screen::Search);
                    launcher.paste_target = Some(4242);
                    launcher.apply_action_outcome(ActionOutcome::PasteText("hello 🦀".into()), cx);
                    // The launcher hides immediately and no keystroke is sent yet.
                    assert_eq!(launcher.state.screen, None);
                    assert!(launcher.paste_cancellation.is_some());
                    assert_eq!(
                        *fake.prepared.lock().unwrap(),
                        vec![("hello 🦀".to_string(), 4242)]
                    );
                    assert!(fake.sent.lock().unwrap().is_empty());
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(PASTE_DELAY_MS));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert_eq!(*fake.sent.lock().unwrap(), vec![4242]);
                    assert!(launcher.paste_cancellation.is_none());
                    assert!(launcher.error.is_none());
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn failed_delayed_paste_remains_visible_after_reopen_and_search(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let fake = Arc::new(FakePaste::default());
        *fake.send_error.lock().unwrap() = Some("target lost focus".into());
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.paste_backend = Some(fake.backend());
                    launcher.paste_target = Some(4242);
                    launcher.state.screen = Some(Screen::Search);
                    launcher.apply_action_outcome(ActionOutcome::PasteText("safe".into()), cx);
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(PASTE_DELAY_MS));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| launcher.show(window, cx))
                .unwrap()
        });
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert!(
                        launcher
                            .visible_error()
                            .unwrap()
                            .contains("target lost focus")
                    );
                    assert!(fake.sent.lock().unwrap().is_empty());
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn pending_paste_is_cancelled_when_the_launcher_reopens_or_the_query_changes(
        cx: &mut TestAppContext,
    ) {
        for reopen in [false, true] {
            let window = test_window(cx, ProviderRegistry::new());
            let fake = Arc::new(FakePaste::default());
            cx.update(|cx| {
                window
                    .update(cx, |launcher, window, cx| {
                        launcher.paste_backend = Some(fake.backend());
                        launcher.state.screen = Some(Screen::Search);
                        launcher.paste_target = Some(4242);
                        launcher.apply_action_outcome(ActionOutcome::PasteText("gone".into()), cx);
                        assert!(launcher.paste_cancellation.is_some());
                        if reopen {
                            launcher.show(window, cx);
                        } else {
                            launcher.replace_query("other".into(), cx);
                        }
                        assert!(
                            launcher.paste_cancellation.is_none(),
                            "reopen={reopen} must cancel the delayed paste"
                        );
                    })
                    .unwrap()
            });
            cx.run_until_parked();
            cx.background_executor
                .advance_clock(std::time::Duration::from_millis(PASTE_DELAY_MS));
            cx.run_until_parked();
            cx.update(|cx| {
                window
                    .update(cx, |_, _, _| {
                        assert!(
                            fake.sent.lock().unwrap().is_empty(),
                            "reopen={reopen} must not send a stale Command-V"
                        );
                    })
                    .unwrap()
            });
        }
    }

    #[gpui::test]
    fn icon_scheduler_fills_four_slots_without_duplicating_a_bundle(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.items = (0..8)
                        .map(|i| {
                            app_bundle_item(&i.to_string(), &PathBuf::from(format!("/app-{i}.app")))
                        })
                        .collect();
                    launcher
                        .state
                        .items
                        .insert(1, launcher.state.items[0].clone());
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(
                        launcher.icon_loading.len(),
                        4,
                        "visible icons must not wait for one serial load"
                    );
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(
                        launcher.icon_loading.len(),
                        4,
                        "a second call must not duplicate or exceed the active limit"
                    );
                })
                .unwrap();
        });
    }

    #[gpui::test]
    fn icon_candidates_prioritize_viewport_and_skip_cached_failed_and_active(
        cx: &mut TestAppContext,
    ) {
        let window = test_window(cx, ProviderRegistry::new());
        let paths: Vec<_> = (0..8)
            .map(|i| PathBuf::from(format!("/app-{i}.app")))
            .collect();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    launcher.state.items = paths
                        .iter()
                        .enumerate()
                        .map(|(i, path)| app_bundle_item(&i.to_string(), path))
                        .collect();
                    launcher.state.generation = 1;
                    launcher.icon_viewport = Some((1, 4..7));
                    assert_eq!(launcher.next_icon_to_load(), Some(paths[4].clone()));
                    launcher.icon_cache.insert(paths[4].clone(), icon_image());
                    launcher.icon_loading.insert(paths[5].clone());
                    launcher.icon_failed.insert(paths[6].clone());
                    assert_eq!(launcher.next_icon_to_load(), Some(paths[0].clone()));
                    // An old query's scrolled viewport must not reorder new results.
                    launcher.icon_viewport = Some((0, 7..8));
                    assert_eq!(launcher.next_icon_to_load(), Some(paths[0].clone()));
                    // Out-of-bounds ranges are safe after a result-count change.
                    launcher.icon_viewport = Some((1, 100..200));
                    assert_eq!(launcher.next_icon_to_load(), Some(paths[0].clone()));
                })
                .unwrap();
        });
    }

    #[gpui::test]
    fn completion_refills_from_current_results_without_exceeding_global_slots(
        cx: &mut TestAppContext,
    ) {
        let window = test_window(cx, ProviderRegistry::new());
        let paths: Vec<_> = (0..4)
            .map(|i| PathBuf::from(format!("/app-{i}.app")))
            .collect();
        let new_path = PathBuf::from("/new-query.app");
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.state.screen = Some(Screen::Search);
                    launcher.icon_loading.extend(paths.iter().cloned());
                    launcher.state.items = vec![app_bundle_item("new", &new_path)];
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(launcher.icon_loading.len(), MAX_ICON_LOADS);
                    assert!(!launcher.icon_loading.contains(&new_path));
                    // Simulate one old query load completing. The new result takes
                    // its freed slot rather than an old queue of offscreen bundles.
                    launcher.icon_loading.remove(&paths[0]);
                    launcher.icon_cache.insert(paths[0].clone(), icon_image());
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(launcher.icon_loading.len(), MAX_ICON_LOADS);
                    assert!(launcher.icon_loading.contains(&new_path));
                })
                .unwrap();
        });
    }

    #[gpui::test]
    fn hidden_launcher_stops_scheduling_icon_loads(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let finder = PathBuf::from("/System/Library/CoreServices/Finder.app");
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.state.screen = None;
                    launcher.state.items = vec![app_bundle_item("finder", &finder)];
                    launcher.schedule_icon_loads(cx);
                    assert!(launcher.icon_loading.is_empty());
                    for screen in [Screen::Actions, Screen::Output] {
                        launcher.state.screen = Some(screen);
                        launcher.schedule_icon_loads(cx);
                        assert!(launcher.icon_loading.is_empty());
                    }
                })
                .unwrap()
        });
    }

    #[cfg(target_os = "macos")]
    #[gpui::test]
    fn application_bundle_icons_load_and_cache_off_the_search_path(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let finder = PathBuf::from("/System/Library/CoreServices/Finder.app");
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.items = vec![app_bundle_item("finder", &finder)];
                    launcher.schedule_icon_loads(cx);
                    assert!(launcher.icon_loading.contains(&finder));
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert!(launcher.icon_cache.contains_key(&finder));
                    assert!(launcher.icon_loading.is_empty());
                    assert!(!launcher.icon_failed.contains(&finder));
                    let image = launcher.icon_cache.get(&finder).unwrap();
                    assert_eq!(image.frame_count(), 1);
                    assert_eq!(image.as_bytes(0).unwrap().len(), 64 * 64 * 4);
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn compact_empty_and_action_screens_fit_the_window(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let view = window.root(cx).unwrap();
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, _| {
                    launcher.options.width = 480.;
                    launcher.state.screen = Some(Screen::Search);
                    launcher.error = Some("No results".into());
                    launcher.resize_window(window);
                    assert_eq!(window.bounds().size, size(px(480.), px(120.)));
                    launcher.state.screen = Some(Screen::Actions);
                    launcher.actions = (0..12)
                        .map(|index| LauncherAction {
                            item: Item {
                                id: ItemId("row".into()),
                                provider: ProviderId("test".into()),
                                title: "row".into(),
                                subtitle: None,
                                keywords: Vec::new(),
                                icon: None,
                                score: 0.,
                                payload: serde_json::Value::Null,
                            },
                            action: Action {
                                id: ActionId(index.to_string()),
                                title: format!("Action {index}"),
                            },
                        })
                        .collect();
                    launcher.state.action_index = 11;
                    launcher.resize_window(window);
                    assert_eq!(window.bounds().size, size(px(480.), px(348.)));
                })
                .unwrap()
        });
        let mut visual = VisualTestContext::from_window(*window, cx);
        visual.simulate_resize(size(px(480.), px(348.)));
        visual.draw(
            point(px(0.), px(0.)),
            size(
                AvailableSpace::Definite(px(480.)),
                AvailableSpace::Definite(px(348.)),
            ),
            |_, _| view.clone().into_any_element(),
        );
        let container = visual.debug_bounds("launcher-container").unwrap();
        let selected = visual.debug_bounds("action-11").unwrap();
        assert!(selected.top() >= container.top() + px(52.));
        assert!(selected.bottom() <= container.bottom() - px(FOOTER_HEIGHT));
        assert!(
            visual.debug_bounds("action-0").is_none(),
            "earlier actions scroll out of view"
        );
    }

    struct FastPreviewProvider {
        calls: Arc<std::sync::atomic::AtomicUsize>,
        revision: Arc<std::sync::atomic::AtomicU64>,
    }
    impl Provider for FastPreviewProvider {
        fn id(&self) -> ProviderId {
            ProviderId("fast-preview".into())
        }
        fn name(&self) -> &str {
            "Fast preview"
        }
        fn revision(&self) -> u64 {
            self.revision.load(std::sync::atomic::Ordering::Relaxed)
        }
        fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
            Ok(vec![])
        }
        fn supports_preview(&self, _: &Item) -> bool {
            true
        }
        fn preview(&self, _: &Item, _: &CancellationToken) -> Result<Option<Preview>> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(Some(Preview::Image {
                png: include_bytes!("../../tests/fixtures/pixel.png").to_vec(),
            }))
        }
        fn actions(&self, _: &Item) -> Vec<Action> {
            vec![]
        }
        fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
            anyhow::bail!("no test actions")
        }
    }

    #[gpui::test]
    fn fast_previews_do_not_flash_loading_and_stale_timers_do_not_change_selection(
        cx: &mut TestAppContext,
    ) {
        let window = test_window(cx, ProviderRegistry::new());
        let key = ("files".into(), "slow".into(), 0);
        let token = CancellationToken::default();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    // Isolate timer behavior from render's selected-provider lookup.
                    launcher.state.searching = true;
                    launcher.preview_key = Some(key.clone());
                    launcher.watch_preview_loading(key.clone(), token.clone(), cx);
                    assert!(!launcher.preview_loading_visible);
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(149));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert!(!launcher.preview_loading_visible)
                })
                .unwrap()
        });
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(1));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    assert!(
                        launcher.preview_loading_visible,
                        "slow previews still explain loading"
                    );
                    launcher.clear_preview(cx);
                    launcher.preview_key = Some(key.clone());
                    launcher.watch_preview_loading(key.clone(), CancellationToken::default(), cx);
                    launcher.apply_preview(
                        &key,
                        Ok(Some(Preview::Image {
                            png: include_bytes!("../../tests/fixtures/pixel.png").to_vec(),
                        })),
                        cx,
                    );
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    assert!(
                        !launcher.preview_loading_visible,
                        "ready images must not show a loading label"
                    );
                    launcher.clear_preview(cx);
                    launcher.preview_key = Some(key.clone());
                    let stale = CancellationToken::default();
                    launcher.watch_preview_loading(key, stale.clone(), cx);
                    stale.cancel();
                    launcher.preview_key = Some(("files".into(), "new".into(), 1));
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(150));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert!(!launcher.preview_loading_visible)
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn image_preview_starts_without_debounce_and_revisit_is_synchronous(cx: &mut TestAppContext) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let revision = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(FastPreviewProvider {
                    calls: calls.clone(),
                    revision: revision.clone(),
                }),
                crate::core::ProviderConfig::default(),
            )
            .unwrap();
        let window = test_window(cx, registry);
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    let mut item = app_bundle_item("image", std::path::Path::new("/unused"));
                    item.provider = ProviderId("fast-preview".into());
                    item.icon = None;
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.items = vec![item];
                    launcher.state.selection.reconcile(&launcher.state.items);
                    launcher.refresh_preview(cx);
                })
                .unwrap()
        });
        // A ready fake decode must complete without advancing the 40ms clock.
        cx.run_until_parked();
        let first = cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
                    launcher
                        .preview_image
                        .clone()
                        .expect("image ready without debounce")
                })
                .unwrap()
        });
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.clear_preview(cx);
                    launcher.refresh_preview(cx);
                    assert!(Arc::ptr_eq(
                        launcher
                            .preview_image
                            .as_ref()
                            .expect("synchronous cache hit"),
                        &first
                    ));
                    assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
                    revision.store(1, std::sync::atomic::Ordering::Relaxed);
                    launcher.refresh_preview(cx);
                    assert!(
                        launcher.preview_image.is_none(),
                        "revision change must miss cache"
                    );
                })
                .unwrap()
        });
        cx.run_until_parked();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[gpui::test]
    fn stale_preview_does_not_replace_current_selection(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        cx.update(|cx| window.update(cx, |launcher, _, cx| {
            let current = ("files".into(), "new-file".into(), 2);
            launcher.preview_key = Some(current.clone());
            launcher.apply_preview(&("files".into(), "old-file".into(), 1), Ok(Some(Preview::Info("stale".into()))), cx);
            assert!(launcher.preview.is_none());
            launcher.apply_preview(&current, Ok(Some(Preview::Text { text: "current 🦀".into(), truncated: false })), cx);
            assert!(matches!(&launcher.preview, Some(Preview::Text { text, .. }) if text == "current 🦀"));
            let token = CancellationToken::default();
            launcher.preview_cancellation = Some(token.clone());
            launcher.clear_preview(cx);
            assert!(token.is_cancelled() && launcher.preview_key.is_none());
        }).unwrap());
    }
    #[gpui::test]
    fn stalled_preview_reports_delay_without_overwriting_ready_or_newer_previews(
        cx: &mut TestAppContext,
    ) {
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(PreviewDragProvider(Arc::new(
                    std::sync::atomic::AtomicUsize::new(0),
                ))),
                crate::core::ProviderConfig::default(),
            )
            .unwrap();
        let window = test_window(cx, registry);
        let key = ("preview-test".into(), "slow".into(), 0);
        let token = CancellationToken::default();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher
                        .coordinator
                        .as_ref()
                        .unwrap()
                        .registry
                        .get(&ProviderId("preview-test".into()))
                        .expect("test preview provider");
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.items = vec![Item {
                        id: ItemId("slow".into()),
                        provider: ProviderId("preview-test".into()),
                        title: "Slow preview".into(),
                        subtitle: None,
                        keywords: vec![],
                        icon: None,
                        score: 0.,
                        payload: serde_json::Value::Null,
                    }];
                    launcher.state.selection.reconcile(&launcher.state.items);
                    launcher.preview_key = Some(key.clone());
                    launcher.preview_cancellation = Some(token.clone());
                    launcher.watch_preview_deadline(key.clone(), token.clone(), cx);
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(1999));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| assert!(launcher.preview.is_none()))
                .unwrap()
        });
        cx.background_executor
            .advance_clock(std::time::Duration::from_millis(1));
        cx.run_until_parked();
        cx.update(|cx| window.update(cx, |launcher, _, cx| {
            assert!(!token.is_cancelled(), "valid slow work should still complete and cache");
            assert!(matches!(&launcher.preview, Some(Preview::Info(message)) if message.contains("taking too long")));
            launcher.apply_preview(&key, Ok(Some(Preview::Info("late but valid".into()))), cx);
            assert!(matches!(&launcher.preview, Some(Preview::Info(message)) if message == "late but valid"));
            launcher.clear_preview(cx);
            // A completed preview must not be replaced by its old deadline.
            let ready = CancellationToken::default();
            launcher.preview_key = Some(key.clone());
            launcher.watch_preview_deadline(key.clone(), ready, cx);
            launcher.apply_preview(&key, Ok(Some(Preview::Info("ready".into()))), cx);
        }).unwrap());
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        cx.update(|cx| window.update(cx, |launcher, _, cx| {
            assert!(matches!(&launcher.preview, Some(Preview::Info(message)) if message == "ready"));
            launcher.clear_preview(cx);
            // A stale deadline must not cancel a newer request.
            let old = CancellationToken::default();
            launcher.preview_key = Some(key.clone());
            launcher.watch_preview_deadline(key, old, cx);
            launcher.state.items[0].id = ItemId("new".into());
            launcher.state.selection.reconcile(&launcher.state.items);
            launcher.preview_key = Some(("preview-test".into(), "new".into(), 0));
        }).unwrap());
        cx.run_until_parked();
        cx.background_executor
            .advance_clock(std::time::Duration::from_secs(2));
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert!(launcher.preview.is_none());
                    assert_eq!(launcher.preview_key.as_ref().unwrap().1, "new");
                })
                .unwrap()
        });
    }

    struct PreviewDragProvider(Arc<std::sync::atomic::AtomicUsize>);
    impl Provider for PreviewDragProvider {
        fn id(&self) -> ProviderId {
            ProviderId("preview-test".into())
        }
        fn name(&self) -> &str {
            "Preview test"
        }
        fn supports_preview(&self, _: &Item) -> bool {
            true
        }
        fn supports_drag(&self, _: &Item) -> bool {
            true
        }
        fn begin_drag(&self, _: &Item) -> Result<()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
            Ok(vec![])
        }
        fn actions(&self, _: &Item) -> Vec<Action> {
            vec![]
        }
        fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
            Ok(ActionOutcome::Close)
        }
    }
    #[gpui::test]
    fn file_preview_scrolls_and_drag_starts_once_after_threshold(cx: &mut TestAppContext) {
        let drags = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(PreviewDragProvider(drags.clone())),
                crate::core::ProviderConfig::default(),
            )
            .unwrap();
        let window = test_window(cx, registry);
        let view = window.root(cx).unwrap();
        let item = Item {
            id: ItemId("fixture".into()),
            provider: ProviderId("preview-test".into()),
            title: "notes.txt".into(),
            subtitle: None,
            keywords: vec![],
            icon: None,
            score: 0.,
            payload: serde_json::Value::Null,
        };
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.items = vec![item.clone()];
                    launcher.state.selection.reconcile(&launcher.state.items);
                    launcher.preview_key = Some((item.provider.0.clone(), item.id.0.clone(), 0));
                    launcher.preview = Some(Preview::Text {
                        text: "Unicode 🦀 preview line\n".repeat(300),
                        truncated: false,
                    });
                    launcher.drag_start = Some((item, point(px(100.), px(100.))));
                    for x in [101., 110., 120.] {
                        launcher.on_drag_move(
                            &gpui::MouseMoveEvent {
                                position: point(px(x), px(100.)),
                                pressed_button: Some(gpui::MouseButton::Left),
                                ..Default::default()
                            },
                            window,
                            cx,
                        );
                    }
                    assert_eq!(drags.load(std::sync::atomic::Ordering::SeqCst), 1);
                })
                .unwrap()
        });
        let mut visual = VisualTestContext::from_window(*window, cx);
        let draw = |visual: &mut VisualTestContext| {
            visual.draw(
                point(px(0.), px(0.)),
                size(
                    AvailableSpace::Definite(px(1040.)),
                    AvailableSpace::Definite(px(400.)),
                ),
                |_, _| view.clone().into_any_element(),
            )
        };
        draw(&mut visual);
        // TestPlatform::resize does not dispatch the native resize callback.
        visual.simulate_resize(size(px(1040.), px(400.)));
        draw(&mut visual);
        assert!(visual.debug_bounds("file-preview").is_some());
        visual.simulate_event(ScrollWheelEvent {
            position: point(px(900.), px(180.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-500.))),
            ..Default::default()
        });
        draw(&mut visual);
        cx.read(|cx| assert!(view.read(cx).preview_scroll.offset().y < px(0.)));
    }
    struct OutputProvider;
    impl Provider for OutputProvider {
        fn id(&self) -> ProviderId {
            ProviderId("output-test".into())
        }
        fn name(&self) -> &str {
            "Output test"
        }
        fn search(&self, _: &SearchQuery, _: &SearchContext) -> Result<Vec<Item>> {
            Ok(vec![])
        }
        fn actions(&self, _: &Item) -> Vec<Action> {
            vec![Action {
                id: ActionId("execute".into()),
                title: "Execute".into(),
            }]
        }
        fn activate(&self, _: &Item, _: &Action) -> Result<ActionOutcome> {
            Ok(ActionOutcome::KeepOpen("unexpected repeat".into()))
        }
    }

    #[gpui::test]
    fn output_screen_scrolls_and_copies_the_actual_text(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let view = window.root(cx).unwrap();
        let text = (0..120)
            .map(|line| format!("Unicode output 🦀 line {line}\n"))
            .collect::<String>();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    launcher.state.screen = Some(Screen::Output);
                    launcher.output = Some(("Test output".into(), text.clone()));
                })
                .unwrap()
        });
        let mut visual = VisualTestContext::from_window(*window, cx);
        let draw = |visual: &mut VisualTestContext| {
            visual.draw(
                point(px(0.), px(0.)),
                size(
                    AvailableSpace::Definite(px(680.)),
                    AvailableSpace::Definite(px(460.)),
                ),
                |_, _| view.clone().into_any_element(),
            );
        };
        draw(&mut visual);
        visual.dispatch_action(MoveDown);
        draw(&mut visual);
        cx.read(|cx| assert!(view.read(cx).output_scroll.offset().y < px(0.)));
        visual.dispatch_action(CopyOutput);
        assert_eq!(cx.read_from_clipboard().unwrap().text().unwrap(), text);
    }

    #[gpui::test]
    fn output_confirmation_does_not_repeat_the_hidden_command(cx: &mut TestAppContext) {
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(OutputProvider),
                crate::core::ProviderConfig::default(),
            )
            .unwrap();
        let window = test_window(cx, registry);
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.state.screen = Some(Screen::Output);
                    launcher.state.items = vec![Item {
                        id: ItemId("command".into()),
                        provider: ProviderId("output-test".into()),
                        title: "Command".into(),
                        subtitle: None,
                        keywords: vec![],
                        icon: None,
                        score: 0.0,
                        payload: serde_json::json!({}),
                    }];
                    launcher.state.selection.reconcile(&launcher.state.items);
                    launcher.refresh_actions();
                    launcher.on_confirm(&Confirm, window, cx);
                    assert!(
                        launcher.activation_in_flight.is_none(),
                        "Enter on output must not repeat execution"
                    );
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn pending_query_keeps_window_bounds_and_blocks_old_actions(cx: &mut TestAppContext) {
        let mut registry = ProviderRegistry::new();
        registry
            .register(
                Arc::new(OutputProvider),
                crate::core::ProviderConfig::default(),
            )
            .unwrap();
        let window = test_window(cx, registry);
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.apply_results(
                        0,
                        vec![Item {
                            id: ItemId("old".into()),
                            provider: ProviderId("output-test".into()),
                            title: "Old suggestion".into(),
                            subtitle: None,
                            keywords: vec![],
                            icon: None,
                            score: 0.,
                            payload: serde_json::Value::Null,
                        }],
                    );
                    let mut second = launcher.state.items[0].clone();
                    second.id = ItemId("second".into());
                    launcher.state.items.push(second);
                    launcher.resize_window(window);
                    let before = window.bounds().size;
                    launcher.replace_query("new".into(), cx);
                    launcher.resize_window(window);
                    assert_eq!(window.bounds().size, before);
                    let selection =
                        selected_result_index(&launcher.state.items, &launcher.state.selection);
                    launcher.on_move_down(&MoveDown, window, cx);
                    assert_eq!(
                        selected_result_index(&launcher.state.items, &launcher.state.selection),
                        selection
                    );
                    launcher.on_confirm(&Confirm, window, cx);
                    launcher.on_next_action(&NextAction, window, cx);
                    launcher.on_actions(&OpenActions, window, cx);
                    assert!(launcher.activation_in_flight.is_none());
                    assert_eq!(launcher.state.screen, Some(Screen::Search));
                    assert!(launcher.state.searching);
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn query_edits_and_escape_cancel_long_running_actions(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        for mode in 0..2 {
            let token = CancellationToken::default();
            cx.update(|cx| {
                window
                    .update(cx, |launcher, window, cx| {
                        launcher.activation_cancellation = Some(token.clone());
                        launcher.activation_in_flight = Some(1);
                        launcher.state.screen = Some(Screen::Search);
                        match mode {
                            0 => launcher.replace_query("different query".into(), cx),
                            _ => launcher.on_escape(&Escape, window, cx),
                        }
                        assert!(token.is_cancelled());
                        assert!(launcher.activation_in_flight.is_none());
                    })
                    .unwrap()
            });
        }
    }

    #[gpui::test]
    fn full_launcher_list_supports_wheel_and_keyboard_scrolling_beyond_eight_rows(
        cx: &mut TestAppContext,
    ) {
        let history = Arc::new(HistoryStore::in_memory().unwrap());
        let coordinator = Arc::new(SearchCoordinator::new(
            Arc::new(ProviderRegistry::new()),
            history.clone(),
            50,
        ));
        let window = cx.update(|cx| {
            open_launcher(
                coordinator,
                history,
                LauncherOptions {
                    width: 680.0,
                    max_results: 50,
                    ..Default::default()
                },
                cx,
            )
            .unwrap()
        });
        let view = window.root(cx).unwrap();
        cx.update(|cx| {
            window
                .update(cx, |launcher, window, cx| {
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.items = (0..50)
                        .map(|i| Item {
                            id: ItemId(i.to_string()),
                            provider: ProviderId("files".into()),
                            title: format!("report-{i:02}.pdf"),
                            subtitle: None,
                            keywords: vec![],
                            icon: None,
                            score: 0.0,
                            payload: serde_json::json!({}),
                        })
                        .collect();
                    launcher.state.selection.reconcile(&launcher.state.items);
                    window.focus(&launcher.search_input.read(cx).focus_handle(cx));
                })
                .unwrap()
        });
        let scroll = cx.read(|cx| view.read(cx).list_scroll.clone());
        let mut visual = VisualTestContext::from_window(*window, cx);
        let draw = |visual: &mut VisualTestContext| {
            visual.draw(
                point(px(0.), px(0.)),
                size(
                    AvailableSpace::Definite(px(680.)),
                    AvailableSpace::Definite(px(560.)),
                ),
                |_, _| view.clone().into_any_element(),
            );
        };
        draw(&mut visual);
        visual.simulate_event(ScrollWheelEvent {
            position: point(px(340.), px(200.)),
            delta: ScrollDelta::Pixels(point(px(0.), px(-600.))),
            ..Default::default()
        });
        draw(&mut visual);
        assert!(
            scroll.0.borrow().base_handle.offset().y <= px(-384.),
            "wheel must reach beyond the first eight rows"
        );
        cx.read(|cx| {
            let view = view.read(cx);
            let (generation, range) = view.icon_viewport.as_ref().unwrap();
            assert_eq!(*generation, view.state.generation);
            assert!(
                range.start >= 8,
                "icon priority must follow the actual wheel-scrolled viewport"
            );
        });
        for _ in 0..49 {
            visual.dispatch_action(MoveDown);
        }
        draw(&mut visual);
        cx.read(|cx| {
            assert_eq!(
                selected_result_index(&view.read(cx).state.items, &view.read(cx).state.selection),
                Some(49)
            )
        });
        assert!(
            scroll.0.borrow().base_handle.offset().y < px(-1500.),
            "keyboard selection must keep the final row visible"
        );
        cx.read(|cx| {
            assert!(
                view.read(cx)
                    .icon_viewport
                    .as_ref()
                    .unwrap()
                    .1
                    .contains(&49),
                "icon priority must follow keyboard scrolling too"
            );
        });
    }
}
