use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{Context as _, Result};
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding, Render, ScrollHandle,
    ScrollStrategy, Subscription, UniformListScrollHandle, Window, WindowBackgroundAppearance,
    WindowBounds, WindowHandle, WindowKind, WindowOptions, actions, div, prelude::*, px, size,
    uniform_list,
};
use launcher_core::{
    Action, ActionId, ActionOutcome, CancellationToken, HistoryStore, Item, Preview,
    SearchCoordinator, UsageEvent,
};

use crate::{
    search_input::{self, SearchInput, SearchInputEvent},
    state::{
        EscapeResult, LauncherState, Screen, result_limit, selected_result_index,
        should_apply_activation, should_hide_after_deactivation, visible_result_count,
    },
    theme::theme,
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
const PREVIEW_WIDTH: f32 = 280.;
const FOOTER_HEIGHT: f32 = 22.;
const MAX_WINDOW_HEIGHT: f32 = 400.;
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
            frontmost: Arc::new(launcher_macos::clipboard::frontmost_application_pid),
            prepare: Arc::new(launcher_macos::clipboard::prepare_paste),
            send: Arc::new(launcher_macos::clipboard::send_paste),
        }
    }
}

pub struct LauncherOptions {
    pub width: f32,
    pub max_results: usize,
}

pub struct Launcher {
    coordinator: Arc<SearchCoordinator>,
    history: Arc<HistoryStore>,
    options: LauncherOptions,
    focus_handle: FocusHandle,
    search_input: Entity<SearchInput>,
    state: LauncherState,
    actions: Vec<Action>,
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
    preview_images: crate::preview_image::PreviewImageCache,
    preview_loading_visible: bool,
    preview_cancellation: Option<CancellationToken>,
    preview_started: Option<std::time::Instant>,
    preview_span: Option<tracing::Span>,
    preview_applied_at: Option<std::time::Instant>,
    preview_scroll: ScrollHandle,
    preview_enabled: bool,
    drag_start: Option<(Item, gpui::Point<gpui::Pixels>)>,
    icon_cache: HashMap<PathBuf, Arc<gpui::Image>>,
    icon_failed: HashSet<PathBuf>,
    /// The one bundle whose native icon is currently being resolved. At most
    /// one load runs at a time so keystrokes cannot stack overlapping decodes.
    icon_loading: Option<PathBuf>,
    /// PID of the application that was frontmost before the launcher opened.
    paste_target: Option<i32>,
    /// A post-dismissal failure must survive ordinary search updates on reopen.
    paste_error: Option<String>,
    /// Pending delayed Command-V. Cancelled when the launcher reopens or the
    /// query changes so it can never paste into a later, unintended action.
    paste_cancellation: Option<CancellationToken>,
    paste_backend: PasteBackend,
    /// Ask the window platform to hide the window. gpui's test platform does
    /// not implement `Platform::hide`, so tests set this to false to exercise
    /// the rest of the hide path without the unsupported call.
    hide_window: bool,
}

pub fn open_launcher(
    coordinator: Arc<SearchCoordinator>,
    history: Arc<HistoryStore>,
    options: LauncherOptions,
    cx: &mut App,
) -> Result<WindowHandle<Launcher>> {
    search_input::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("down", MoveDown, Some("Launcher")),
        KeyBinding::new("ctrl-n", MoveDown, Some("Launcher")),
        KeyBinding::new("up", MoveUp, Some("Launcher")),
        KeyBinding::new("ctrl-p", MoveUp, Some("Launcher")),
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
    let (input_tx, input_rx) = async_channel::unbounded();
    let window = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                window_background: WindowBackgroundAppearance::Transparent,
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
                let launcher = cx.new(|cx| Launcher {
                    coordinator,
                    history,
                    options,
                    focus_handle: cx.focus_handle(),
                    search_input,
                    state: LauncherState::default(),
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
                    drag_start: None,
                    icon_cache: HashMap::new(),
                    icon_failed: HashSet::new(),
                    icon_loading: None,
                    paste_target: None,
                    paste_error: None,
                    paste_cancellation: None,
                    paste_backend: PasteBackend::default(),
                    hide_window: true,
                });
                let refresh = launcher.downgrade();
                let mut revisions: Vec<(String, u64)> = Vec::new();
                cx.spawn(async move |cx| {
                    loop {
                        gpui::Timer::after(std::time::Duration::from_millis(500)).await;
                        if refresh
                            .update(cx, |this, cx| {
                                let current: Vec<_> = this
                                    .coordinator
                                    .registry
                                    .descriptors()
                                    .iter()
                                    .filter_map(|d| {
                                        this.coordinator
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
                            let SearchInputEvent::Changed(query) = event;
                            // The editor already holds this edit. Do not echo queued text
                            // back and reset the caret or IME composition.
                            this.query_changed(query, cx);
                            this.start_search(cx);
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
    Ok(window)
}

impl Launcher {
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
        self.capture_paste_target();
        self.state.screen = Some(Screen::Search);
        self.was_active = false;
        self.clear_preview(cx);
        self.state.items.clear();
        self.state.selection.reconcile(&self.state.items);
        self.replace_query(String::new(), cx);
        cx.activate(true);
        window.activate_window();
        window.focus(&self.search_input.read(cx).focus_handle(cx));
        self.resize_window(window);
        self.start_search(cx);
        cx.notify();
    }

    pub fn hide(&mut self, cx: &mut Context<Self>) {
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
        self.actions.clear();
        self.action_item = None;
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

    fn query_changed(&mut self, query: String, _: &mut Context<Self>) {
        self.drag_start = None;
        self.output = None;
        self.state.replace_query(query);
        self.actions.clear();
        self.action_item = None;
        self.invalidate_activation();
    }

    fn clear_preview(&mut self, _cx: &mut Context<Self>) {
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
        if self.has_preview() {
            (self.options.width + PREVIEW_WIDTH).min(1200.)
        } else {
            self.options.width
        }
    }
    fn on_toggle_preview(&mut self, _: &TogglePreview, _: &mut Window, cx: &mut Context<Self>) {
        self.preview_enabled = !self.preview_enabled;
        self.clear_preview(cx);
        cx.notify();
    }
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
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
                                .map(crate::preview_image::PreparedPreview::new)
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
        key: crate::preview_image::PreviewKey,
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
                .map(crate::preview_image::PreparedPreview::new)
                .transpose()
        });
        self.apply_prepared_preview(key, prepared, cx);
    }

    fn apply_prepared_preview(
        &mut self,
        key: &crate::preview_image::PreviewKey,
        result: Result<Option<crate::preview_image::PreparedPreview>>,
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
            .border_color(theme().border)
            .p_3()
            .overflow_y_scroll()
            .track_scroll(&self.preview_scroll)
            .text_sm();
        match &self.preview {
            Some(Preview::Text { text, truncated }) => {
                panel = panel.child(div().font_family("monospace").child(text.clone()));
                if *truncated {
                    panel = panel.child(
                        div()
                            .pt_2()
                            .text_color(theme().muted)
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
        if let Some(provider) = self.coordinator.registry.get(&item.provider)
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
        if self.state.screen.is_some() {
            return;
        }
        // Ignore our own pid and empty readings so the last external application
        // is retained rather than clobbered by the launcher itself.
        if let Some(pid) = (self.paste_backend.frontmost)()
            .filter(|pid| *pid > 0 && *pid != std::process::id() as i32)
        {
            self.paste_target = Some(pid);
        }
    }

    fn visible_error(&self) -> Option<&String> {
        self.error.as_ref().or(self.paste_error.as_ref())
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
        if let Err(error) = (self.paste_backend.prepare)(&text, pid) {
            self.error = Some(format!("Could not paste: {error}"));
            cx.notify();
            return;
        }
        // The target now owns the clipboard text; hide and let it activate.
        self.hide(cx);
        self.cancel_paste();
        let token = CancellationToken::default();
        self.paste_cancellation = Some(token.clone());
        let send = self.paste_backend.send.clone();
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
        if let Some(cancellation) = self.search_cancellation.take() {
            cancellation.cancel();
        }
        let session = self.coordinator.start(self.state.query.clone());
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
                        this.error = errors.first().map(|failure| failure.message.clone());
                        if this.state.screen != Some(Screen::Actions) {
                            this.refresh_actions();
                        }
                        this.schedule_icon_loads(cx);
                        this.keep_selection_visible();
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

    /// Starts at most one background icon load at a time. When it finishes, the
    /// next highest-ranked uncached bundle is chosen from the *current* results,
    /// so a stale queue cannot starve what is on screen. Hiding the launcher
    /// gates new loads without discarding the in-flight result.
    fn schedule_icon_loads(&mut self, cx: &mut Context<Self>) {
        if self.state.screen.is_none() {
            return;
        }
        let Some(path) = self.next_icon_to_load() else {
            return;
        };
        self.icon_loading = Some(path.clone());
        let this = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let load_path = path.clone();
            // Native decode and PNG hashing both run off the UI thread.
            let image = cx
                .background_executor()
                .spawn(async move {
                    launcher_macos::application_icon(&load_path)
                        .map(|png| Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png)))
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                this.icon_loading = None;
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

    /// Highest-ranked bundle in the current results that still needs an icon and
    /// that no in-flight load already covers.
    fn next_icon_to_load(&self) -> Option<PathBuf> {
        if self.icon_loading.is_some() {
            return None;
        }
        self.state.items.iter().find_map(|item| {
            let Some(launcher_core::IconDescriptor::ApplicationBundle(path)) = &item.icon else {
                return None;
            };
            if self.icon_cache.contains_key(path) || self.icon_failed.contains(path) {
                return None;
            }
            Some(path.clone())
        })
    }

    fn refresh_actions(&mut self) {
        let Some(item) = self.state.selection.current(&self.state.items) else {
            self.actions.clear();
            return;
        };
        self.actions = if item.provider.0 == "picker" {
            vec![Action {
                id: ActionId("select-provider".into()),
                title: "Select Provider".into(),
            }]
        } else {
            self.coordinator
                .registry
                .get(&item.provider)
                .map(|p| p.actions(item))
                .unwrap_or_default()
        };
        self.state.action_index = self
            .state
            .action_index
            .min(self.actions.len().saturating_sub(1));
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
        let mut row_count = if self.state.screen == Some(Screen::Actions) {
            visible_result_count(self.actions.len())
        } else {
            visible_result_count(self.state.items.len())
        };
        if self.has_preview() {
            row_count = row_count.max(5);
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
        }
        self.resize_window(window);
        cx.notify();
    }

    fn on_move_up(&mut self, _: &MoveUp, window: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, window, cx);
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
        if self.activation_in_flight.is_some() {
            self.invalidate_activation();
            self.error = Some("Action cancelled.".into());
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
        if self.state.screen == Some(Screen::Output) || self.state.searching {
            return;
        }
        self.refresh_actions();
        self.action_item = self.state.selection.current(&self.state.items).cloned();
        self.state.enter_actions(&self.actions);
        self.resize_window(window);
        cx.notify();
    }

    fn on_next_action(&mut self, _: &NextAction, _: &mut Window, cx: &mut Context<Self>) {
        if self.state.screen == Some(Screen::Output) || self.state.searching {
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
        if let Some(action) = self.actions.get(1).cloned() {
            self.activate(item, action, cx);
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
        self.replace_query(";".into(), cx);
        self.start_search(cx);
    }

    fn on_confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        if self.state.screen == Some(Screen::Output) || self.state.searching {
            return;
        }
        if self.state.screen == Some(Screen::Actions) {
            if let (Some(item), Some(action)) = (
                self.action_item.clone(),
                self.actions.get(self.state.action_index).cloned(),
            ) {
                if item.provider.0 == "picker" {
                    self.select_provider(&item, cx);
                } else {
                    self.activate(item, action, cx);
                }
            }
        } else if let Some(item) = self.state.selection.current(&self.state.items).cloned() {
            if item.provider.0 == "picker" {
                self.select_provider(&item, cx);
            } else {
                self.refresh_actions();
                if let Some(action) = self.actions.first().cloned() {
                    self.activate(item, action, cx);
                }
            }
        }
    }

    fn activate(&mut self, item: Item, action: Action, cx: &mut Context<Self>) {
        if self.activation_in_flight.is_some() {
            return;
        }
        let Some(provider) = self.coordinator.registry.get(&item.provider) else {
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
        let history = self.history.clone();
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
        let theme = theme();
        let state = self.state.screen.clone();
        if state == Some(Screen::Output) {
            window.focus(&self.focus_handle);
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
            .max_h(px(MAX_WINDOW_HEIGHT))
            .overflow_hidden()
            .rounded(px(12.))
            .font_family("monospace")
            .text_size(px(14.))
            .line_height(px(18.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .h(px(52.))
                    .flex_shrink_0()
                    .px_3()
                    .py(px(10.))
                    .border_b_1()
                    .border_color(theme.divider)
                    .child(self.search_input.clone()),
            );

        if state.is_some() {
            if state == Some(Screen::Output) {
                if let Some((title, text)) = &self.output {
                    root = root
                        .child(div().px_3().h(px(28.)).child(title.clone()))
                        .child(
                            div()
                                .id("command-output")
                                .mx_3()
                                .h(px(260.))
                                .overflow_y_scroll()
                                .track_scroll(&self.output_scroll)
                                .font_family("monospace")
                                .text_sm()
                                .child(text.clone()),
                        );
                }
            } else if state == Some(Screen::Actions) {
                for (index, action) in actions
                    .iter()
                    .enumerate()
                    .skip(action_index.saturating_sub(crate::state::MAX_VISIBLE_RESULTS - 1))
                    .take(crate::state::MAX_VISIBLE_RESULTS)
                {
                    let selected = index == action_index;
                    root = root.child(
                        div()
                            .debug_selector(|| format!("action-{index}"))
                            .rounded(px(6.))
                            .mx_2()
                            .h(px(ROW_HEIGHT))
                            .flex_shrink_0()
                            .flex()
                            .items_center()
                            .px_3()
                            .bg(if selected {
                                theme.selected
                            } else {
                                theme.background
                            })
                            .text_color(if selected {
                                theme.selected_foreground
                            } else {
                                theme.muted
                            })
                            .child(action.title.clone()),
                    );
                }
            } else if items.is_empty() {
                root = root.child(
                    div()
                        .px_3()
                        .h(px(44.))
                        .flex()
                        .items_center()
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
            } else {
                if let Some(error) = self.visible_error() {
                    root = root.child(
                        div()
                            .px_3()
                            .h(px(24.))
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .line_height(px(14.))
                            .text_color(theme.muted)
                            .child(error.clone()),
                    );
                }
                let visible_count =
                    visible_result_count(items.len()).max(if self.has_preview() { 5 } else { 1 });
                let row_theme = theme.clone();
                let selected_for_rows = selected.clone();
                let rows_view = cx.entity().downgrade();
                // Resolved bundle icons are cloned as cheap `Arc`s; rendering must
                // not re-encode PNG bytes for every frame.
                let mut icon_images: HashMap<PathBuf, Arc<gpui::Image>> = HashMap::new();
                for item in &items {
                    if let Some(launcher_core::IconDescriptor::ApplicationBundle(path)) = &item.icon
                        && let Some(image) = self.icon_cache.get(path)
                    {
                        icon_images.insert(path.clone(), image.clone());
                    }
                }
                let list = uniform_list("launcher-results", items.len(), move |range, _, _| {
                    range
                        .map(|index| {
                            let item = &items[index];
                            let is_selected = selected_for_rows
                                .as_ref()
                                .is_some_and(|(p, id)| p == &item.provider.0 && id == &item.id.0);
                            let view = rows_view.clone();
                            let item_for_click = item.clone();
                            let image = match &item.icon {
                                Some(launcher_core::IconDescriptor::ApplicationBundle(path)) => {
                                    icon_images.get(path).cloned()
                                }
                                _ => None,
                            };
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
            root = root.child(
                div()
                    .h(px(FOOTER_HEIGHT))
                    .flex_shrink_0()
                    .mt_auto()
                    .flex()
                    .items_center()
                    .px_3()
                    .text_size(px(10.))
                    .line_height(px(14.))
                    .text_color(theme.muted)
                    .child(if state == Some(Screen::Output) {
                        "⌘C Copy · Esc Back".to_string()
                    } else if self.state.searching {
                        "Searching…".to_string()
                    } else if self.activation_in_flight.is_some() {
                        "Running… · Esc Cancel".to_string()
                    } else if state == Some(Screen::Actions) {
                        "↵ Run · Esc Back".to_string()
                    } else {
                        let position =
                            selected_result_index(&self.state.items, &self.state.selection)
                                .map_or(0, |index| index + 1);
                        format!("{position}/{}", self.state.items.len())
                    }),
            );
        }
        root
    }
}

fn result_row(
    item: &Item,
    selected: bool,
    theme: &crate::theme::Theme,
    image: Option<Arc<gpui::Image>>,
) -> gpui::Div {
    let secondary = item
        .subtitle
        .clone()
        .unwrap_or_else(|| item.provider.0.clone());
    let icon = if let Some(image) = image {
        Some(
            gpui::img(image)
                .size(px(28.))
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
        )
    } else {
        match &item.icon {
            Some(launcher_core::IconDescriptor::Png(png)) => Some(
                gpui::img(Arc::new(gpui::Image::from_bytes(
                    gpui::ImageFormat::Png,
                    png.to_vec(),
                )))
                .size(px(28.))
                .object_fit(gpui::ObjectFit::Contain)
                .into_any_element(),
            ),
            _ if item.provider.0 == "apps" => Some(div().child("□").into_any_element()),
            _ => None,
        }
    };
    let icon_slot = icon.map(|icon| {
        div()
            .debug_selector(|| format!("suggestion-icon-{}", item.id.0))
            .size(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .child(icon)
    });
    div()
        .debug_selector(|| format!("suggestion-{}", item.id.0))
        .w_full()
        .min_w_0()
        .px_3()
        .py(px(4.))
        .rounded(px(6.))
        .text_color(if selected {
            theme.selected_foreground
        } else {
            theme.foreground
        })
        .bg(if selected {
            theme.selected
        } else {
            theme.background
        })
        .flex()
        .items_center()
        .gap(px(8.))
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
                .child(
                    div()
                        .debug_selector(|| format!("suggestion-type-{}", item.id.0))
                        .text_size(px(11.))
                        .line_height(px(14.))
                        .flex_shrink_0()
                        .truncate()
                        .text_color(if selected {
                            theme.selected_muted
                        } else {
                            theme.muted
                        })
                        .child(secondary),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{
        AvailableSpace, ScrollDelta, ScrollWheelEvent, TestAppContext, VisualTestContext, point,
    };
    use launcher_core::{
        ItemId, Provider, ProviderId, ProviderRegistry, SearchContext, SearchQuery,
    };

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
                                    launcher_core::IconDescriptor::Png(Arc::from(
                                        include_bytes!(
                                            "../../launcher-macos/tests/fixtures/pixel.png"
                                        )
                                        .as_slice(),
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
            assert_eq!(icon.size, size(px(28.), px(28.)));
            assert!(icon.top() >= first.top() && icon.bottom() <= first.bottom());
            let title = visual.debug_bounds("suggestion-title-0").unwrap();
            assert!(title.left() >= icon.right() + px(8.));
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
            icon: Some(launcher_core::IconDescriptor::ApplicationBundle(
                path.to_path_buf(),
            )),
            score: 0.,
            payload: serde_json::Value::Null,
        }
    }

    fn icon_image() -> Arc<gpui::Image> {
        Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            include_bytes!("../../launcher-macos/tests/fixtures/pixel.png").to_vec(),
        ))
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
                    launcher.paste_backend = fake.backend();
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
                    launcher.paste_backend = fake.backend();
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
                    launcher.paste_backend = fake.backend();
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
                    launcher.paste_backend = fake.backend();
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
                        launcher.paste_backend = fake.backend();
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
    fn next_icon_to_load_picks_highest_current_result_and_skips_cached(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let finder = PathBuf::from("/System/Library/CoreServices/Finder.app");
        let safari = PathBuf::from("/Applications/Safari.app");
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    launcher.state.items = vec![
                        app_bundle_item("finder", &finder),
                        app_bundle_item("safari", &safari),
                    ];
                    // The highest-ranked uncached bundle wins.
                    assert_eq!(
                        launcher.next_icon_to_load().as_deref(),
                        Some(finder.as_path())
                    );
                    // One in-flight load suppresses any further scheduling.
                    launcher.icon_loading = Some(finder.clone());
                    assert!(launcher.next_icon_to_load().is_none());
                    // Once resolved, the next current result is chosen.
                    launcher.icon_loading = None;
                    launcher.icon_cache.insert(finder.clone(), icon_image());
                    assert_eq!(
                        launcher.next_icon_to_load().as_deref(),
                        Some(safari.as_path())
                    );
                    // Failed bundles are never retried.
                    launcher.icon_failed.insert(safari.clone());
                    assert!(launcher.next_icon_to_load().is_none());
                })
                .unwrap()
        });
    }

    #[gpui::test]
    fn icon_loads_run_one_at_a_time_and_recompute_from_current_results(cx: &mut TestAppContext) {
        let window = test_window(cx, ProviderRegistry::new());
        let finder = PathBuf::from("/System/Library/CoreServices/Finder.app");
        let safari = PathBuf::from("/Applications/Safari.app");
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, cx| {
                    launcher.state.screen = Some(Screen::Search);
                    launcher.state.items = vec![app_bundle_item("finder", &finder)];
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(launcher.icon_loading.as_deref(), Some(finder.as_path()));
                    // A second call must not launch a parallel decode.
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(launcher.icon_loading.as_deref(), Some(finder.as_path()));
                    // New results neither cancel nor overlap the running load.
                    launcher.state.items = vec![app_bundle_item("safari", &safari)];
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(launcher.icon_loading.as_deref(), Some(finder.as_path()));
                    // Simulated completion: the next load is recomputed from the
                    // current results (Safari), not the stale list.
                    launcher.icon_loading = None;
                    launcher.icon_cache.insert(finder.clone(), icon_image());
                    launcher.schedule_icon_loads(cx);
                    assert_eq!(launcher.icon_loading.as_deref(), Some(safari.as_path()));
                })
                .unwrap()
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
                    assert!(launcher.icon_loading.is_none());
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
                    assert_eq!(launcher.icon_loading.as_deref(), Some(finder.as_path()));
                })
                .unwrap()
        });
        cx.run_until_parked();
        cx.update(|cx| {
            window
                .update(cx, |launcher, _, _| {
                    assert!(launcher.icon_cache.contains_key(&finder));
                    assert!(launcher.icon_loading.is_none());
                    assert!(!launcher.icon_failed.contains(&finder));
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
                        .map(|index| Action {
                            id: ActionId(index.to_string()),
                            title: format!("Action {index}"),
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
                png: include_bytes!("../../launcher-macos/tests/fixtures/pixel.png").to_vec(),
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
                            png: include_bytes!("../../launcher-macos/tests/fixtures/pixel.png")
                                .to_vec(),
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
                launcher_core::ProviderConfig::default(),
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
                launcher_core::ProviderConfig::default(),
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
                launcher_core::ProviderConfig::default(),
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
                launcher_core::ProviderConfig::default(),
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
                launcher_core::ProviderConfig::default(),
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
    }
}
