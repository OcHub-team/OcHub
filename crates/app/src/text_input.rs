//! A reusable single-line text input entity for GPUI, adapted from
//! `zed/crates/gpui/examples/input.rs`.
//!
//! Provides focus, cursor, selection, editing, clipboard, and IME support.
//! Host views embed it via `cx.new(|cx| TextInput::new(cx, "placeholder"))`,
//! render the entity as a child, and read/write text via [`TextInput::content`]
//! / [`TextInput::set_content`].

use std::ops::Range;

use gpui::{
    actions, div, fill, point, prelude::*, px, size, App, Bounds, ClipboardItem, Context,
    CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    Focusable, GlobalElementId, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style, TextRun,
    UTF16Selection, UnderlineStyle, Window,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::theme;

actions!(
    text_input,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectAll,
        Home,
        End,
        Newline,
        ShowCharacterPalette,
        Paste,
        Cut,
        Copy,
    ]
);

/// Register the text-input key bindings. Call once from `main.rs`.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("backspace", Backspace, None),
        gpui::KeyBinding::new("delete", Delete, None),
        gpui::KeyBinding::new("left", Left, None),
        gpui::KeyBinding::new("right", Right, None),
        gpui::KeyBinding::new("up", Up, None),
        gpui::KeyBinding::new("down", Down, None),
        gpui::KeyBinding::new("shift-left", SelectLeft, None),
        gpui::KeyBinding::new("shift-right", SelectRight, None),
        gpui::KeyBinding::new("cmd-a", SelectAll, None),
        gpui::KeyBinding::new("cmd-v", Paste, None),
        gpui::KeyBinding::new("cmd-c", Copy, None),
        gpui::KeyBinding::new("cmd-x", Cut, None),
        gpui::KeyBinding::new("home", Home, None),
        gpui::KeyBinding::new("end", End, None),
        gpui::KeyBinding::new("enter", Newline, None),
        gpui::KeyBinding::new("shift-enter", Newline, None),
        gpui::KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
    ]);
}

pub struct TextInput {
    focus_handle: FocusHandle,
    content: SharedString,
    placeholder: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    last_layout: Option<ShapedLine>,
    last_bounds: Option<Bounds<Pixels>>,
    is_selecting: bool,
    masked: bool,
    multiline: bool,
    /// Code-editor mode: multiline rendering, monospace, line-number gutter.
    code: bool,
    /// Per-line shaped layouts captured during the last paint (code mode), used
    /// for hit-testing and vertical cursor movement.
    code_lines: Vec<ShapedLine>,
    /// Window-space origin of the text column (right of the gutter), last paint.
    code_origin: Option<Point<Pixels>>,
    code_line_height: Pixels,
}

impl TextInput {
    pub fn new(cx: &mut Context<Self>, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            last_layout: None,
            last_bounds: None,
            is_selecting: false,
            masked: false,
            multiline: false,
            code: false,
            code_lines: Vec::new(),
            code_origin: None,
            code_line_height: px(20.),
        }
    }

    /// Render as a multiline code editor (monospace, line numbers, real line
    /// breaks). Implies [`Self::multiline`].
    pub fn code(mut self, code: bool) -> Self {
        self.code = code;
        if code {
            self.multiline = true;
        }
        self
    }

    /// Byte offset of the start of each logical line.
    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0usize];
        for (idx, byte) in self.content.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(idx + 1);
            }
        }
        starts
    }

    /// Length (bytes) of logical line `line`, excluding its trailing newline.
    fn line_len(&self, line: usize, starts: &[usize]) -> usize {
        let start = starts[line];
        let end = if line + 1 < starts.len() {
            starts[line + 1].saturating_sub(1)
        } else {
            self.content.len()
        };
        end.saturating_sub(start)
    }

    /// Map a byte offset to (line index, byte offset within that line).
    fn line_index_for_offset(&self, offset: usize) -> (usize, usize) {
        let starts = self.line_starts();
        let mut line = 0;
        for (idx, &start) in starts.iter().enumerate() {
            if start <= offset {
                line = idx;
            } else {
                break;
            }
        }
        (line, offset.saturating_sub(starts[line]))
    }

    /// Hit-test a window-space point to a byte offset (code mode).
    fn code_offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(origin) = self.code_origin else {
            return self.cursor_offset();
        };
        if self.code_lines.is_empty() {
            return 0;
        }
        let line_height = f32::from(self.code_line_height).max(1.0);
        let rel_y = f32::from(position.y - origin.y);
        let mut line_idx = (rel_y / line_height).floor() as isize;
        if line_idx < 0 {
            line_idx = 0;
        }
        let line_idx = (line_idx as usize).min(self.code_lines.len() - 1);
        let starts = self.line_starts();
        let local_x = position.x - origin.x;
        let col = self.code_lines[line_idx].closest_index_for_x(local_x);
        let line_len = self.line_len(line_idx, &starts);
        starts[line_idx] + col.min(line_len)
    }

    fn up(&mut self, _: &Up, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.code {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
            return;
        }
        let (line, col) = self.line_index_for_offset(self.cursor_offset());
        if line == 0 {
            self.move_to(0, cx);
            return;
        }
        let x = self
            .code_lines
            .get(line)
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let starts = self.line_starts();
        let target = line - 1;
        let target_col = self
            .code_lines
            .get(target)
            .map(|l| l.closest_index_for_x(x))
            .unwrap_or(0);
        let offset = starts[target] + target_col.min(self.line_len(target, &starts));
        self.move_to(offset, cx);
    }

    fn down(&mut self, _: &Down, _window: &mut Window, cx: &mut Context<Self>) {
        if !self.code {
            self.move_to(self.next_boundary(self.cursor_offset()), cx);
            return;
        }
        let (line, col) = self.line_index_for_offset(self.cursor_offset());
        let starts = self.line_starts();
        if line + 1 >= starts.len() {
            self.move_to(self.content.len(), cx);
            return;
        }
        let x = self
            .code_lines
            .get(line)
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let target = line + 1;
        let target_col = self
            .code_lines
            .get(target)
            .map(|l| l.closest_index_for_x(x))
            .unwrap_or(0);
        let offset = starts[target] + target_col.min(self.line_len(target, &starts));
        self.move_to(offset, cx);
    }

    /// Render the masked variant (secrets shown as bullets).
    pub fn masked(mut self, masked: bool) -> Self {
        self.masked = masked;
        self
    }

    /// Allow Enter and pasted text to preserve newlines. Rendering still uses
    /// the lightweight GPUI input element, but the saved value remains faithful.
    pub fn multiline(mut self, multiline: bool) -> Self {
        self.multiline = multiline;
        self
    }

    /// Current text content.
    pub fn content(&self) -> SharedString {
        self.content.clone()
    }

    /// Replace the full content, resetting the cursor to the end.
    pub fn set_content(&mut self, content: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = content.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        cx.notify();
    }

    fn left(&mut self, _: &Left, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.previous_boundary(self.cursor_offset()), cx);
        } else {
            self.move_to(self.selected_range.start, cx)
        }
    }

    fn right(&mut self, _: &Right, _: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.move_to(self.next_boundary(self.selected_range.end), cx);
        } else {
            self.move_to(self.selected_range.end, cx)
        }
    }

    fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor_offset()), cx);
    }

    fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor_offset()), cx);
    }

    fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.move_to(0, cx);
        self.select_to(self.content.len(), cx)
    }

    fn home(&mut self, _: &Home, _: &mut Window, cx: &mut Context<Self>) {
        if self.code {
            let (line, _) = self.line_index_for_offset(self.cursor_offset());
            let starts = self.line_starts();
            self.move_to(starts[line], cx);
        } else {
            self.move_to(0, cx);
        }
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        if self.code {
            let (line, _) = self.line_index_for_offset(self.cursor_offset());
            let starts = self.line_starts();
            self.move_to(starts[line] + self.line_len(line, &starts), cx);
        } else {
            self.move_to(self.content.len(), cx);
        }
    }

    fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let prev = self.previous_boundary(self.cursor_offset());
            if self.cursor_offset() == prev {
                window.play_system_bell();
                return;
            }
            self.select_to(prev, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            let next = self.next_boundary(self.cursor_offset());
            if self.cursor_offset() == next {
                window.play_system_bell();
                return;
            }
            self.select_to(next, cx)
        }
        self.replace_text_in_range(None, "", window, cx)
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        } else {
            self.move_to(self.index_for_mouse_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.index_for_mouse_position(event.position), cx);
        }
    }

    fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            let text = if self.multiline {
                text
            } else {
                text.replace('\n', " ")
            };
            self.replace_text_in_range(None, &text, window, cx);
        }
    }

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.multiline {
            self.replace_text_in_range(None, "\n", window, cx);
        } else {
            window.play_system_bell();
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
            self.replace_text_in_range(None, "", window, cx)
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        if self.content.is_empty() {
            return 0;
        }
        if self.code {
            return self.code_offset_for_position(position);
        }
        let (Some(bounds), Some(line)) = (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };
        if position.y < bounds.top() {
            return 0;
        }
        if position.y > bounds.bottom() {
            return self.content.len();
        }
        line.closest_index_for_x(position.x - bounds.left())
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8_offset = 0;
        let mut utf16_count = 0;
        for ch in self.content.chars() {
            if utf16_count >= offset {
                break;
            }
            utf16_count += ch.len_utf16();
            utf8_offset += ch.len_utf8();
        }
        utf8_offset
    }

    fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16_offset = 0;
        let mut utf8_count = 0;
        for ch in self.content.chars() {
            if utf8_count >= offset {
                break;
            }
            utf8_count += ch.len_utf8();
            utf16_offset += ch.len_utf16();
        }
        utf16_offset
    }

    fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    fn previous_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .rev()
            .find_map(|(idx, _)| (idx < offset).then_some(idx))
            .unwrap_or(0)
    }

    fn next_boundary(&self, offset: usize) -> usize {
        self.content
            .grapheme_indices(true)
            .find_map(|(idx, _)| (idx > offset).then_some(idx))
            .unwrap_or(self.content.len())
    }
}

impl EntityInputHandler for TextInput {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        actual_range.replace(self.range_to_utf16(&range));
        Some(self.content[range].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range),
            reversed: self.selection_reversed,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.marked_range
            .as_ref()
            .map(|range| self.range_to_utf16(range))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.marked_range = None;
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .map(|new_range| new_range.start + range.start..new_range.end + range.end)
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let range = self.range_from_utf16(&range_utf16);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(range.start),
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(range.end),
                bounds.bottom(),
            ),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let line_point = self.last_bounds?.localize(&point)?;
        let last_layout = self.last_layout.as_ref()?;
        let utf8_index = last_layout.index_for_x(point.x - line_point.x)?;
        Some(self.offset_to_utf16(utf8_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = window.line_height().into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let raw_content = input.content.clone();
        let masked = input.masked;
        let selected_range = input.selected_range.clone();
        let cursor = input.cursor_offset();
        let style = window.text_style();

        let display_content: SharedString = if masked && !raw_content.is_empty() {
            "•".repeat(raw_content.chars().count()).into()
        } else if input.multiline && (raw_content.contains('\n') || raw_content.contains('\r')) {
            raw_content.replace(['\r', '\n'], " ").into()
        } else {
            raw_content.clone()
        };

        let (display_text, text_color) = if display_content.is_empty() {
            (
                input.placeholder.clone(),
                theme::muted().opacity(0.78).into(),
            )
        } else {
            (display_content, style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = input.marked_range.as_ref() {
            vec![
                TextRun {
                    len: marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: marked_range.end - marked_range.start,
                    underline: Some(UnderlineStyle {
                        color: Some(run.color),
                        thickness: px(1.0),
                        wavy: false,
                    }),
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - marked_range.end,
                    ..run
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let font_size = style.font_size.to_pixels(window.rem_size());
        let line = window
            .text_system()
            .shape_line(display_text, font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                Some(fill(
                    Bounds::new(
                        point(bounds.left() + cursor_pos, bounds.top()),
                        size(px(2.), bounds.bottom() - bounds.top()),
                    ),
                    theme::accent(),
                )),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            bounds.left() + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            bounds.left() + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    gpui::rgba(0x89b4fa44),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let line = prepaint.line.take().unwrap();
        line.paint(
            bounds.origin,
            window.line_height(),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(_window);
        let base = div()
            .id("text-input")
            .role(gpui::Role::TextInput)
            .aria_label(self.placeholder.clone())
            .aria_placeholder(self.placeholder.clone())
            .aria_value(self.content.clone())
            .focusable()
            .tab_stop(true)
            .flex()
            .key_context("TextInput")
            .track_focus(&self.focus_handle(cx))
            .cursor(CursorStyle::IBeam)
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::left))
            .on_action(cx.listener(Self::right))
            .on_action(cx.listener(Self::up))
            .on_action(cx.listener(Self::down))
            .on_action(cx.listener(Self::select_left))
            .on_action(cx.listener(Self::select_right))
            .on_action(cx.listener(Self::select_all))
            .on_action(cx.listener(Self::home))
            .on_action(cx.listener(Self::end))
            .on_action(cx.listener(Self::newline))
            .on_action(cx.listener(Self::show_character_palette))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::cut))
            .on_action(cx.listener(Self::copy))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(theme::surface())
            .border_1()
            .border_color(if focused {
                theme::accent()
            } else {
                theme::border()
            })
            .text_color(theme::text())
            .text_sm()
            .line_height(px(20.));
        if self.code {
            base.font_family("Menlo")
                .h(px(380.))
                .items_start()
                .overflow_y_scroll()
                .child(div().w_full().child(CodeElement { input: cx.entity() }))
        } else {
            base.when(self.multiline, |s| s.h(px(132.)).items_start())
                .child(div().w_full().child(TextElement { input: cx.entity() }))
        }
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Multiline code-editor element: shapes each logical line, paints a
/// line-number gutter, and supports cross-line cursor/selection. Shares the
/// `TextInput` entity (content, selection, IME) with the single-line element.
struct CodeElement {
    input: Entity<TextInput>,
}

struct CodePrepaintState {
    lines: Vec<ShapedLine>,
    numbers: Vec<(ShapedLine, Pixels)>,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    gutter: Pixels,
    line_height: Pixels,
    text_origin: Point<Pixels>,
}

impl IntoElement for CodeElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeElement {
    type RequestLayoutState = ();
    type PrepaintState = CodePrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let line_count = {
            let input = self.input.read(cx);
            input.content.split('\n').count().max(1)
        };
        let line_height = window.line_height();
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = (line_height * line_count as f32).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let input = self.input.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let text_color = style.color;
        let muted: gpui::Hsla = theme::muted().opacity(0.85).into();

        let empty = input.content.is_empty();
        let line_texts: Vec<SharedString> = if empty {
            vec![input.placeholder.clone()]
        } else {
            input
                .content
                .split('\n')
                .map(|line| SharedString::from(line.to_string()))
                .collect()
        };
        let line_count = line_texts.len();
        let digits = line_count.to_string().len().max(2);
        let gutter = px(18. + digits as f32 * 8.5);
        let text_origin = point(bounds.left() + gutter, bounds.top());

        let mut lines = Vec::with_capacity(line_count);
        let mut numbers = Vec::with_capacity(line_count);
        for (i, text) in line_texts.iter().enumerate() {
            let color = if empty { muted } else { text_color };
            let run = TextRun {
                len: text.len(),
                font: style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            lines.push(
                window
                    .text_system()
                    .shape_line(text.clone(), font_size, &[run], None),
            );

            let num_text = SharedString::from((i + 1).to_string());
            let num_run = TextRun {
                len: num_text.len(),
                font: style.font(),
                color: theme::muted().opacity(0.7).into(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let num_shaped =
                window
                    .text_system()
                    .shape_line(num_text.clone(), font_size, &[num_run], None);
            let num_width = num_shaped.x_for_index(num_text.len());
            let num_x = bounds.left() + gutter - px(10.) - num_width;
            numbers.push((num_shaped, num_x));
        }

        let mut selections = Vec::new();
        let selection = input.selected_range.clone();
        if !selection.is_empty() && !empty {
            let starts = input.line_starts();
            for (i, line) in lines.iter().enumerate() {
                let line_start = starts[i];
                let line_end = line_start + input.line_len(i, &starts);
                if selection.start > line_end || selection.end < line_start {
                    continue;
                }
                let seg_start = selection.start.max(line_start);
                let seg_end = selection.end.min(line_end);
                let x0 = line.x_for_index(seg_start - line_start);
                let mut x1 = line.x_for_index(seg_end - line_start);
                let includes_newline = selection.end > line_end;
                if includes_newline {
                    x1 = x1 + px(6.);
                }
                if x1 > x0 {
                    let y = bounds.top() + line_height * i as f32;
                    selections.push(fill(
                        Bounds::from_corners(
                            point(text_origin.x + x0, y),
                            point(text_origin.x + x1, y + line_height),
                        ),
                        gpui::rgba(0x89b4fa44),
                    ));
                }
            }
        }

        let cursor = if empty {
            Some(fill(
                Bounds::new(
                    point(text_origin.x, bounds.top()),
                    size(px(2.), line_height),
                ),
                theme::accent(),
            ))
        } else {
            let (line, col) = input.line_index_for_offset(input.cursor_offset());
            let cursor_x = lines
                .get(line)
                .map(|l| l.x_for_index(col))
                .unwrap_or(px(0.));
            let y = bounds.top() + line_height * line as f32;
            Some(fill(
                Bounds::new(
                    point(text_origin.x + cursor_x, y),
                    size(px(2.), line_height),
                ),
                theme::accent(),
            ))
        };

        CodePrepaintState {
            lines,
            numbers,
            selections,
            cursor,
            gutter,
            line_height,
            text_origin,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.input.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.input.clone()),
            cx,
        );

        // Gutter background.
        window.paint_quad(fill(
            Bounds::new(
                point(bounds.left(), bounds.top()),
                size(prepaint.gutter, bounds.size.height),
            ),
            theme::inset(),
        ));

        for selection in prepaint.selections.drain(..) {
            window.paint_quad(selection);
        }

        let line_height = prepaint.line_height;
        let text_x = prepaint.text_origin.x;
        for (i, line) in prepaint.lines.iter().enumerate() {
            let y = bounds.top() + line_height * i as f32;
            if let Some((number, num_x)) = prepaint.numbers.get(i) {
                number
                    .paint(
                        point(*num_x, y),
                        line_height,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
            }
            line.paint(
                point(text_x, y),
                line_height,
                gpui::TextAlign::Left,
                None,
                window,
                cx,
            )
            .ok();
        }

        if focus_handle.is_focused(window) {
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        }

        let lines = std::mem::take(&mut prepaint.lines);
        let text_origin = prepaint.text_origin;
        self.input.update(cx, |input, _cx| {
            input.code_lines = lines;
            input.code_origin = Some(text_origin);
            input.code_line_height = line_height;
            input.last_bounds = Some(bounds);
        });
    }
}
