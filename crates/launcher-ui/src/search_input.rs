// Text input follows GPUI 0.2.2's `examples/input.rs` EntityInputHandler pattern.
// GPUI is Apache-2.0 licensed. This implementation adapts its documented public API.
use std::ops::Range;

use gpui::{
    App, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element, ElementId,
    ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, GlobalElementId,
    LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, div, fill,
    point, prelude::*, px, relative, rgba, size,
};

use crate::theme::Theme;
use unicode_segmentation::UnicodeSegmentation;

gpui::actions!(
    search_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        SelectLeft,
        SelectRight,
        SelectHome,
        SelectEnd,
        SelectAll,
        Home,
        End,
        Paste,
        Copy,
        Cut
    ]
);

#[derive(Clone, Debug)]
pub(crate) enum SearchInputEvent {
    Changed(String),
}

pub(crate) struct SearchInput {
    focus_handle: FocusHandle,
    content: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    horizontal_offset: Pixels,
    events: async_channel::Sender<SearchInputEvent>,
}

pub(crate) fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("backspace", Backspace, Some("Launcher")),
        gpui::KeyBinding::new("delete", Delete, Some("Launcher")),
        gpui::KeyBinding::new("left", Left, Some("Launcher")),
        gpui::KeyBinding::new("right", Right, Some("Launcher")),
        gpui::KeyBinding::new("shift-left", SelectLeft, Some("Launcher")),
        gpui::KeyBinding::new("shift-right", SelectRight, Some("Launcher")),
        gpui::KeyBinding::new("cmd-a", SelectAll, Some("Launcher")),
        gpui::KeyBinding::new("cmd-left", Home, Some("Launcher")),
        gpui::KeyBinding::new("cmd-right", End, Some("Launcher")),
        gpui::KeyBinding::new("shift-cmd-left", SelectHome, Some("Launcher")),
        gpui::KeyBinding::new("shift-cmd-right", SelectEnd, Some("Launcher")),
        gpui::KeyBinding::new("home", Home, Some("Launcher")),
        gpui::KeyBinding::new("end", End, Some("Launcher")),
        gpui::KeyBinding::new("cmd-v", Paste, Some("Launcher")),
        gpui::KeyBinding::new("cmd-c", Copy, Some("Launcher")),
        gpui::KeyBinding::new("cmd-x", Cut, Some("Launcher")),
    ]);
}

impl SearchInput {
    pub(crate) fn new(
        cx: &mut Context<Self>,
        events: async_channel::Sender<SearchInputEvent>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: "".into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            horizontal_offset: px(0.),
            events,
        }
    }

    pub(crate) fn set_text(&mut self, text: String, cx: &mut Context<Self>) {
        if self.content.as_ref() == text {
            return;
        }
        self.content = text.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn emit(&mut self, event: SearchInputEvent, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.events.try_send(event);
        let _ = (window, cx);
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.selection_reversed = false;
        cx.notify();
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset;
        } else {
            self.selected_range.end = offset;
        }
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify();
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(index, _)| (index < offset).then_some(index))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(index, _)| (index > offset).then_some(index))
            .unwrap_or(self.content.len())
    }

    fn replace_selection(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, text, window, cx);
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx);
        }
    }
    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.end, cx);
        }
    }
    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }
    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }
    fn select_home(&mut self, _: &SelectHome, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(0, cx);
    }
    fn select_end(&mut self, _: &SelectEnd, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.content.len(), cx);
    }
    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx);
    }
    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
    }
    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(self.content.len(), cx);
    }
    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.previous_boundary(self.cursor_offset()), cx);
        }
        self.replace_selection("", window, cx);
    }
    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor_offset()), cx);
        }
        self.replace_selection("", window, cx);
    }
    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_selection(&text.replace('\n', " "), window, cx);
        }
    }
    fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
        }
    }
    fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selected_range.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(
                self.content[self.selected_range.clone()].to_string(),
            ));
            self.replace_selection("", window, cx);
        }
    }
    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.is_selecting = true;
        let offset = self.index_for_mouse_position(event.position);
        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, cx);
        }
    }
    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }
    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }
    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        let (Some(bounds), Some(line)) = (&self.last_bounds, &self.last_layout) else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left() + self.horizontal_offset)
    }

    fn utf8_to_utf16(&self, offset: usize) -> usize {
        utf8_to_utf16(&self.content, offset)
    }
    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.utf8_to_utf16(range.start)..self.utf8_to_utf16(range.end)
    }
    fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        range_from_utf16(&self.content, range)
    }
}

impl EntityInputHandler for SearchInput {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        actual: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range);
        actual.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_range = None;
        cx.notify();
    }
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|r| self.range_from_utf16(r))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        let end = range.start + new_text.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
        self.emit(
            SearchInputEvent::Changed(self.content.to_string()),
            window,
            cx,
        );
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        selected_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (content, marked_range, selected_range) = replace_and_mark_text(
            &self.content,
            self.marked_range.as_ref(),
            &self.selected_range,
            range_utf16.as_ref(),
            new_text,
            selected_utf16,
        );
        self.content = content.into();
        self.marked_range = marked_range;
        self.selected_range = selected_range;
        self.selection_reversed = false;
        cx.notify();
        self.emit(
            SearchInputEvent::Changed(self.content.to_string()),
            window,
            cx,
        );
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let line = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range);
        Some(Bounds::from_corners(
            point(
                bounds.left() + line.x_for_index(range.start) - self.horizontal_offset,
                bounds.top(),
            ),
            point(
                bounds.left() + line.x_for_index(range.end) - self.horizontal_offset,
                bounds.bottom(),
            ),
        ))
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let local = self.last_bounds?.localize(&point)?;
        let line = self.last_layout.as_ref()?;
        Some(self.utf8_to_utf16(line.index_for_x(point.x - local.x + self.horizontal_offset)?))
    }
}

struct SearchInputElement {
    input: Entity<SearchInput>,
    theme: Theme,
}
impl IntoElement for SearchInputElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
struct InputPrepaint {
    line: ShapedLine,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    horizontal_offset: Pixels,
}
impl Element for SearchInputElement {
    type RequestLayoutState = ();
    type PrepaintState = InputPrepaint;
    fn id(&self) -> Option<ElementId> {
        None
    }
    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> InputPrepaint {
        let input = self.input.read(cx);
        let content = if input.content.is_empty() {
            SharedString::from("Search…")
        } else {
            input.content.clone()
        };
        let style = window.text_style();
        let color = if input.content.is_empty() {
            self.theme.muted
        } else {
            style.color
        };
        let base = TextRun {
            len: content.len(),
            font: style.font(),
            color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked) = &input.marked_range {
            vec![
                TextRun {
                    len: marked.start,
                    ..base.clone()
                },
                TextRun {
                    len: marked.end - marked.start,
                    underline: Some(UnderlineStyle {
                        color: Some(color),
                        thickness: px(1.),
                        wavy: false,
                    }),
                    ..base.clone()
                },
                TextRun {
                    len: content.len() - marked.end,
                    ..base
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![base]
        };
        let line = window.text_system().shape_line(
            content,
            style.font_size.to_pixels(window.rem_size()),
            &runs,
            None,
        );
        let selected = input.selected_range.clone();
        let cursor_x = line.x_for_index(input.cursor_offset());
        let available_width = (bounds.size.width - px(8.)).max(px(1.));
        let horizontal_offset = if cursor_x - input.horizontal_offset > available_width {
            cursor_x - available_width + px(4.)
        } else if cursor_x < input.horizontal_offset + px(4.) {
            (cursor_x - px(4.)).max(px(0.))
        } else {
            input.horizontal_offset
        };
        let (cursor, selection) = if selected.is_empty() {
            (
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_x - horizontal_offset, bounds.top()),
                        size(px(1.5), bounds.bottom() - bounds.top()),
                    ),
                    self.theme.foreground,
                )),
                None,
            )
        } else {
            (
                None,
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected.start) - horizontal_offset,
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected.end) - horizontal_offset,
                            bounds.bottom(),
                        ),
                    ),
                    rgba(0x336c7fb8),
                )),
            )
        };
        InputPrepaint {
            line,
            cursor,
            selection,
            horizontal_offset,
        }
    }
    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut InputPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection);
        }
        let line = std::mem::replace(
            &mut prepaint.line,
            window
                .text_system()
                .shape_line("".into(), px(12.), &[], None),
        );
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            line.paint(
                point(bounds.left() - prepaint.horizontal_offset, bounds.top()),
                window.line_height(),
                window,
                cx,
            )
            .ok();
        });
        if focus.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }
        self.input.update(cx, |input, _| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.horizontal_offset = prepaint.horizontal_offset;
        });
    }
}

impl Focusable for SearchInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
impl Render for SearchInput {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("Launcher")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_home))
            .on_action(cx.listener(Self::select_end))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::cut))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .line_height(px(22.))
            .text_size(px(16.))
            .child(
                div()
                    .h(px(30.))
                    .w_full()
                    .overflow_x_hidden()
                    .px_1()
                    .child(SearchInputElement {
                        input: cx.entity(),
                        theme: crate::theme::theme(),
                    }),
            )
    }
}

// Convert an IME marked-text update without involving GPUI, so the production
// EntityInputHandler path and regression tests share the same range handling.
fn replace_and_mark_text(
    content: &str,
    marked_range: Option<&Range<usize>>,
    selected_range: &Range<usize>,
    range_utf16: Option<&Range<usize>>,
    new_text: &str,
    selected_utf16: Option<Range<usize>>,
) -> (String, Option<Range<usize>>, Range<usize>) {
    let replacement = range_utf16
        .map(|range| range_from_utf16(content, range))
        .or_else(|| marked_range.cloned())
        .unwrap_or_else(|| selected_range.clone());
    let replacement = clamp_utf8_range(content, replacement);
    let updated = format!(
        "{}{}{}",
        &content[..replacement.start],
        new_text,
        &content[replacement.end..]
    );
    let composition_end = replacement.start + new_text.len();
    let marked = (!new_text.is_empty()).then_some(replacement.start..composition_end);
    // EntityInputHandler's selected range here is relative to `new_text`, not
    // to the pre-edit buffer or the updated full string.
    let selection = selected_utf16
        .map(|range| range_from_utf16(new_text, &range))
        .unwrap_or(new_text.len()..new_text.len());
    let selection = clamp_utf8_range(new_text, selection);
    (
        updated,
        marked,
        replacement.start + selection.start..replacement.start + selection.end,
    )
}

fn clamp_utf8_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = utf8_boundary_at_or_after(text, range.start.min(text.len()));
    let end = utf8_boundary_at_or_after(text, range.end.min(text.len())).max(start);
    start..end
}

fn utf8_boundary_at_or_after(text: &str, offset: usize) -> usize {
    if text.is_char_boundary(offset) {
        return offset;
    }
    (offset + 1..=text.len())
        .find(|&candidate| text.is_char_boundary(candidate))
        .unwrap_or(text.len())
}

fn range_from_utf16(text: &str, range: &Range<usize>) -> Range<usize> {
    let max = text.encode_utf16().count();
    let start = utf16_to_utf8(text, range.start.min(max));
    let end = utf16_to_utf8(text, range.end.min(max)).max(start);
    start..end
}

fn utf16_to_utf8(text: &str, offset: usize) -> usize {
    let mut u16 = 0;
    let mut u8 = 0;
    for ch in text.chars() {
        if u16 >= offset {
            break;
        }
        u16 += ch.len_utf16();
        u8 += ch.len_utf8();
    }
    u8
}
fn utf8_to_utf16(text: &str, offset: usize) -> usize {
    let mut u8 = 0;
    let mut u16 = 0;
    for ch in text.chars() {
        if u8 >= offset {
            break;
        }
        u8 += ch.len_utf8();
        u16 += ch.len_utf16();
    }
    u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;
    #[test]
    fn utf16_round_trip_handles_astral_and_combining_text() {
        let text = "a🧑🏽‍💻e\u{301}";
        for (i, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            assert_eq!(utf16_to_utf8(text, utf8_to_utf16(text, i)), i);
        }
    }
    #[test]
    fn grapheme_navigation_keeps_emoji_sequences_together() {
        let text = "x👨‍👩‍👧‍👦y";
        let starts: Vec<_> = text.grapheme_indices(true).map(|(i, _)| i).collect();
        assert_eq!(starts, vec![0, 1, text.len() - 1]);
    }
    #[test]
    fn utf16_offsets_inside_surrogate_pair_clamp_to_codepoint_boundary() {
        assert_eq!(utf16_to_utf8("🧑", 1), 4);
        assert_eq!(utf8_to_utf16("🧑", 2), 2);
    }

    #[test]
    fn ime_selection_is_relative_to_composition_not_old_buffer() {
        let (content, marked, selection) =
            replace_and_mark_text("café", None, &(5..5), None, "🧑🏽‍💻", Some(0..7));
        assert_eq!(content, "café🧑🏽‍💻");
        let marked = marked.unwrap();
        assert_eq!(&content[marked.clone()], "🧑🏽‍💻");
        assert_eq!(&content[selection.clone()], "🧑🏽‍💻");

        // An insertion range is expressed in UTF-16 coordinates of the full
        // current buffer, including the non-ASCII prefix and marked text.
        let insertion = range_from_utf16(&content, &(11..11));
        assert_eq!(insertion, content.len()..content.len());
        let (committed, next_marked, after_commit) =
            replace_and_mark_text(&content, Some(&marked), &selection, None, "", None);
        assert_eq!(committed, "café");
        assert_eq!(next_marked, None);
        assert_eq!(after_commit, 5..5);
    }

    #[test]
    fn repeated_ime_update_uses_full_buffer_range_and_composition_relative_selection() {
        let (first, marked, selected) =
            replace_and_mark_text("AéZ", None, &(3..3), Some(&(2..3)), "🧑", Some(2..2));
        assert_eq!(first, "Aé🧑");
        assert_eq!(marked, Some(3..7));
        assert_eq!(selected, 7..7);

        let (second, marked, selected) =
            replace_and_mark_text(&first, marked.as_ref(), &selected, None, "語🧑", Some(0..3));
        assert_eq!(second, "Aé語🧑");
        assert_eq!(marked, Some(3..10));
        assert_eq!(&second[selected], "語🧑");
    }

    #[test]
    fn malformed_or_overlong_ime_ranges_are_clamped_to_valid_boundaries() {
        let (content, marked, selection) = replace_and_mark_text(
            "pré",
            None,
            &(4..4),
            Some(&(usize::MAX..usize::MAX)),
            "🧑",
            Some(0..usize::MAX),
        );
        assert_eq!(content, "pré🧑");
        let marked = marked.unwrap();
        for boundary in [marked.start, marked.end, selection.start, selection.end] {
            assert!(content.is_char_boundary(boundary));
        }
        assert_eq!(&content[selection], "🧑");
    }
}
