use std::sync::Arc;

use anyhow::{Context as _, Result};
use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, KeyBinding, Render, ScrollStrategy,
    Subscription, UniformListScrollHandle, Window, WindowBackgroundAppearance, WindowBounds,
    WindowHandle, WindowKind, WindowOptions, actions, div, prelude::*, px, size, uniform_list,
};
use launcher_core::{
    Action, ActionId, ActionOutcome, CancellationToken, HistoryStore, Item, SearchCoordinator,
    UsageEvent,
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
        Quit,
        Escape
    ]
);

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
    search_cancellation: Option<CancellationToken>,
    error: Option<String>,
    list_scroll: UniformListScrollHandle,
    activation_subscription: Option<Subscription>,
    was_active: bool,
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
                    search_cancellation: None,
                    error: None,
                    list_scroll: UniformListScrollHandle::new(),
                    activation_subscription: None,
                    was_active: false,
                });
                let weak = launcher.downgrade();
                cx.spawn(async move |cx| {
                    while let Ok(event) = input_rx.recv().await {
                        let result = weak.update(cx, |this, cx| {
                            let SearchInputEvent::Changed(query) = event;
                            this.replace_query(query, cx);
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
        self.state.screen = Some(Screen::Search);
        self.was_active = false;
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
        self.was_active = false;
        self.state.generation = self.state.generation.wrapping_add(1);
        if let Some(cancellation) = self.search_cancellation.take() {
            cancellation.cancel();
        }
        self.actions.clear();
        self.action_item = None;
        self.invalidate_activation();
        cx.hide();
        cx.notify();
    }

    fn replace_query(&mut self, query: String, cx: &mut Context<Self>) {
        self.state.replace_query(query.clone());
        self.actions.clear();
        self.action_item = None;
        self.invalidate_activation();
        self.search_input
            .update(cx, |input, cx| input.set_text(query, cx));
    }

    fn invalidate_activation(&mut self) {
        self.activation_serial = self.activation_serial.wrapping_add(1);
        self.activation_in_flight = None;
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
                    if this.state.apply_results(generation, items) {
                        this.error = errors.first().map(|failure| failure.message.clone());
                        if this.state.screen != Some(Screen::Actions) {
                            this.refresh_actions();
                        }
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
        let row_count = if self.state.screen == Some(Screen::Actions) {
            self.actions.len().min(8)
        } else {
            visible_result_count(self.state.items.len())
        };
        let error_height = usize::from(self.error.is_some()) * 24;
        let height = (106 + row_count * 48 + error_height).clamp(148, 560) as f32;
        let desired = size(px(self.options.width), px(height));
        let current = window.bounds().size;
        if (current.width - desired.width).abs() > px(1.)
            || (current.height - desired.height).abs() > px(1.)
        {
            window.resize(desired);
        }
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.state.screen == Some(Screen::Actions) {
            if !self.actions.is_empty() {
                let len = self.actions.len() as isize;
                self.state.action_index =
                    (self.state.action_index as isize + delta).rem_euclid(len) as usize;
            }
        } else {
            self.state.selection.move_by(&self.state.items, delta);
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

    fn on_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        match self.state.escape() {
            EscapeResult::Back => {
                self.action_item = None;
                self.refresh_actions();
            }
            EscapeResult::Cleared => {
                self.search_input
                    .update(cx, |input, cx| input.set_text(String::new(), cx));
                self.start_search(cx);
            }
            EscapeResult::Hide => self.hide(cx),
        }
        self.resize_window(window);
        cx.notify();
    }

    fn on_actions(&mut self, _: &OpenActions, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_actions();
        self.action_item = self.state.selection.current(&self.state.items).cloned();
        self.state.enter_actions(&self.actions);
        self.resize_window(window);
        cx.notify();
    }

    fn on_next_action(&mut self, _: &NextAction, _: &mut Window, cx: &mut Context<Self>) {
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
        let history = self.history.clone();
        let query = self.state.query.clone();
        let this = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    provider.activate(&item, &action).inspect(|_| {
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
                match result {
                    Ok(outcome) => match outcome {
                        ActionOutcome::Close => this.hide(cx),
                        ActionOutcome::KeepOpen(message) => {
                            this.error = Some(message);
                            cx.notify();
                        }
                        ActionOutcome::SetQuery(query) => {
                            this.replace_query(query, cx);
                            this.start_search(cx);
                        }
                    },
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
        self.resize_window(window);
        let theme = theme();
        let state = self.state.screen.clone();
        let selected = self
            .state
            .selection
            .selected()
            .map(|(p, i)| (p.to_string(), i.to_string()));
        let items = self.state.items.clone();
        let actions = self.actions.clone();
        let action_index = self.state.action_index;
        let mut root = div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_move_up))
            .on_action(cx.listener(Self::on_move_down))
            .on_action(cx.listener(Self::on_confirm))
            .on_action(cx.listener(Self::on_actions))
            .on_action(cx.listener(Self::on_provider_picker))
            .on_action(cx.listener(Self::on_next_action))
            .on_action(cx.listener(Self::on_escape))
            .flex()
            .flex_col()
            .w(px(self.options.width))
            .max_h(px(560.))
            .rounded(px(12.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.background)
            .text_color(theme.foreground)
            .shadow_lg()
            .child(div().px_5().py_4().child(self.search_input.clone()));

        if state.is_some() {
            if state == Some(Screen::Actions) {
                root = root.child(div().mx_4().h(px(1.)).bg(theme.border));
                for (index, action) in actions.iter().enumerate() {
                    let selected = index == action_index;
                    root = root.child(
                        div()
                            .mx_2()
                            .my(px(2.))
                            .px_3()
                            .py_2()
                            .rounded(px(6.))
                            .bg(if selected {
                                theme.selected
                            } else {
                                theme.background
                            })
                            .text_color(if selected {
                                theme.foreground
                            } else {
                                theme.muted
                            })
                            .child(action.title.clone()),
                    );
                }
            } else if items.is_empty() {
                root = root.child(
                    div()
                        .px_5()
                        .pb_4()
                        .text_sm()
                        .text_color(theme.muted)
                        .child(self.error.clone().unwrap_or_else(|| "No results".into())),
                );
            } else {
                if let Some(error) = &self.error {
                    root = root.child(
                        div()
                            .px_5()
                            .pb_2()
                            .text_sm()
                            .text_color(theme.muted)
                            .child(error.clone()),
                    );
                }
                let visible_count = visible_result_count(items.len());
                let row_theme = theme.clone();
                let selected_for_rows = selected.clone();
                root = root.child(
                    uniform_list("launcher-results", items.len(), move |range, _, _| {
                        range
                            .map(|index| {
                                let item = &items[index];
                                let is_selected =
                                    selected_for_rows.as_ref().is_some_and(|(p, id)| {
                                        p == &item.provider.0 && id == &item.id.0
                                    });
                                result_row(item, is_selected, &row_theme).h(px(48.))
                            })
                            .collect::<Vec<_>>()
                    })
                    .h(px(visible_count as f32 * 48.))
                    .track_scroll(self.list_scroll.clone()),
                );
            }
            root = root.child(
                div()
                    .border_t_1()
                    .border_color(theme.border)
                    .px_4()
                    .py_2()
                    .text_xs()
                    .text_color(theme.muted)
                    .child(if state == Some(Screen::Actions) {
                        "↵ Run action    Esc Back"
                    } else {
                        "↵ Open    ⌘↵ Actions    ⌘P Providers    Esc Close"
                    }),
            );
        }
        root
    }
}

fn result_row(item: &Item, selected: bool, theme: &crate::theme::Theme) -> gpui::Div {
    let secondary = item
        .subtitle
        .clone()
        .unwrap_or_else(|| item.provider.0.clone());
    div()
        .mx_2()
        .px_3()
        .py_2()
        .rounded(px(7.))
        .bg(if selected {
            theme.selected
        } else {
            theme.background
        })
        .flex()
        .flex_col()
        .gap(px(2.))
        .child(div().text_size(px(15.)).child(item.title.clone()))
        .child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted)
                .child(secondary),
        )
}
