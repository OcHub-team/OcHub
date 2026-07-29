//! A reusable single-line text input entity for GPUI, adapted from
//! `zed/crates/gpui/examples/input.rs`.
//!
//! Provides focus, cursor, selection, editing, clipboard, and IME support.
//! Host views embed it via `cx.new(|cx| TextInput::new(cx, "placeholder"))`,
//! render the entity as a child, and read/write text via [`TextInput::content`]
//! / [`TextInput::set_content`].

use std::ops::Range;
use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardItem, Context, CursorStyle, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, FocusHandle, Focusable, GlobalElementId, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollHandle,
    ShapedLine, SharedString, Style, TextRun, UTF16Selection, UnderlineStyle, Window, actions, div,
    fill, point, prelude::*, px, size,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::i18n::{k, raw, t};
use crate::icons::{IconName, icon};
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
        Undo,
        Redo,
        Find,
        FindNext,
        FindPrevious,
        CloseFind,
    ]
);

pub(crate) enum TextInputEvent {
    Changed,
}

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
        gpui::KeyBinding::new("home", Home, None),
        gpui::KeyBinding::new("end", End, None),
        gpui::KeyBinding::new("enter", Newline, None),
        gpui::KeyBinding::new("shift-enter", Newline, None),
        gpui::KeyBinding::new("enter", FindNext, Some("SearchInput")),
        gpui::KeyBinding::new("shift-enter", FindPrevious, Some("SearchInput")),
        gpui::KeyBinding::new("escape", CloseFind, Some("SearchInput")),
        gpui::KeyBinding::new("f3", FindNext, None),
        gpui::KeyBinding::new("shift-f3", FindPrevious, None),
    ]);

    #[cfg(target_os = "macos")]
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-a", SelectAll, None),
        gpui::KeyBinding::new("cmd-v", Paste, None),
        gpui::KeyBinding::new("cmd-c", Copy, None),
        gpui::KeyBinding::new("cmd-x", Cut, None),
        gpui::KeyBinding::new("cmd-z", Undo, None),
        gpui::KeyBinding::new("cmd-shift-z", Redo, None),
        gpui::KeyBinding::new("cmd-f", Find, None),
        gpui::KeyBinding::new("cmd-g", FindNext, None),
        gpui::KeyBinding::new("cmd-shift-g", FindPrevious, None),
        gpui::KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
    ]);

    #[cfg(any(target_os = "windows", target_os = "linux"))]
    cx.bind_keys([
        gpui::KeyBinding::new("ctrl-a", SelectAll, None),
        gpui::KeyBinding::new("ctrl-v", Paste, None),
        gpui::KeyBinding::new("ctrl-c", Copy, None),
        gpui::KeyBinding::new("ctrl-x", Cut, None),
        gpui::KeyBinding::new("ctrl-z", Undo, None),
        gpui::KeyBinding::new("ctrl-y", Redo, None),
        gpui::KeyBinding::new("ctrl-shift-z", Redo, None),
        gpui::KeyBinding::new("ctrl-f", Find, None),
        gpui::KeyBinding::new("ctrl-g", FindNext, None),
        gpui::KeyBinding::new("ctrl-shift-g", FindPrevious, None),
    ]);
}

const MAX_UNDO_SNAPSHOTS: usize = 100;
const CARET_BLINK_INTERVAL: Duration = Duration::from_millis(500);
const MASK_GLYPH: &str = "•";

fn accessible_value(raw: &str, masked: bool) -> SharedString {
    if masked && !raw.is_empty() {
        MASK_GLYPH.repeat(raw.chars().count()).into()
    } else {
        SharedString::from(raw.to_string())
    }
}

/// Translate an offset in the stored value to the text actually painted by the
/// lightweight input. Masked fields replace every Unicode scalar with one
/// bullet, so raw byte offsets cannot be used directly against the shaped line.
fn display_offset(raw: &str, masked: bool, raw_offset: usize) -> usize {
    let mut offset = raw_offset.min(raw.len());
    while !raw.is_char_boundary(offset) {
        offset = offset.saturating_sub(1);
    }
    if masked {
        raw[..offset].chars().count() * MASK_GLYPH.len()
    } else {
        offset
    }
}

/// Byte offset into `text` for a UTF-16 offset measured from its start.
/// Saturates at `text.len()` and always lands on a character boundary, so the
/// result is safe to slice with even when the platform hands us an offset that
/// overruns the string or splits a surrogate pair.
fn utf8_offset_from_utf16(text: &str, utf16_offset: usize) -> usize {
    let mut utf8_offset = 0;
    let mut utf16_count = 0;
    for ch in text.chars() {
        if utf16_count >= utf16_offset {
            break;
        }
        utf16_count += ch.len_utf16();
        utf8_offset += ch.len_utf8();
    }
    utf8_offset
}

fn raw_offset_from_display(raw: &str, masked: bool, display_offset: usize) -> usize {
    if !masked {
        let mut offset = display_offset.min(raw.len());
        while !raw.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        return offset;
    }

    let character = display_offset / MASK_GLYPH.len();
    raw.char_indices()
        .nth(character)
        .map(|(offset, _)| offset)
        .unwrap_or(raw.len())
}

/// Keep the active caret inside a single-line viewport while preserving the
/// user's current scroll position until the caret reaches either edge.
fn horizontal_scroll_for_caret(
    current: Pixels,
    caret_x: Pixels,
    content_width: Pixels,
    viewport_width: Pixels,
    focused: bool,
) -> Pixels {
    if !focused || viewport_width <= px(0.) || content_width <= viewport_width {
        return px(0.);
    }

    let padding = px(4.);
    let max_scroll = (content_width - viewport_width + padding).max(px(0.));
    let current = current.clamp(px(0.), max_scroll);
    let next = if caret_x < current + padding {
        (caret_x - padding).max(px(0.))
    } else if caret_x > current + viewport_width - padding {
        caret_x - viewport_width + padding
    } else {
        current
    };
    next.clamp(px(0.), max_scroll)
}

/// Shared focus/visibility state for every custom caret in the app. Timer
/// owners only need to schedule the returned epoch and call [`Self::tick`].
#[derive(Default)]
pub(crate) struct CaretBlink {
    focused: bool,
    visible: bool,
    epoch: usize,
}

impl CaretBlink {
    pub(crate) fn set_focused(&mut self, focused: bool) -> Option<usize> {
        if self.focused == focused {
            return None;
        }
        self.focused = focused;
        self.visible = focused;
        self.epoch = self.epoch.wrapping_add(1);
        focused.then_some(self.epoch)
    }

    pub(crate) fn reset(&mut self) -> Option<usize> {
        self.visible = true;
        self.epoch = self.epoch.wrapping_add(1);
        self.focused.then_some(self.epoch)
    }

    pub(crate) fn tick(&mut self, epoch: usize) -> bool {
        if !self.focused || self.epoch != epoch {
            return false;
        }
        self.visible = !self.visible;
        true
    }

    pub(crate) fn visible(&self) -> bool {
        self.visible
    }
}

#[derive(Clone, PartialEq)]
struct EditSnapshot {
    content: SharedString,
    selected_range: Range<usize>,
    selection_reversed: bool,
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
    /// Positive amount by which single-line text is shifted left. Unlike a
    /// generic scroll container this follows keyboard/IME caret movement too.
    horizontal_scroll: Pixels,
    is_selecting: bool,
    masked: bool,
    multiline: bool,
    /// Code-editor mode: multiline rendering, monospace, line-number gutter.
    code: bool,
    /// Per-line shaped layouts captured during the last paint (code mode), used
    /// for hit-testing and vertical cursor movement. Only the scroll viewport
    /// (plus overdraw) is shaped; `code_first_row` maps buffer rows onto it.
    code_lines: Vec<ShapedLine>,
    /// Buffer row represented by `code_lines[0]`.
    code_first_row: usize,
    /// Cached line split of `content` (code mode). Rebuilt only when the
    /// content changes, so scroll and caret-blink frames never re-split or
    /// re-allocate the whole document.
    code_line_cache: Vec<SharedString>,
    /// The content the line cache was built from; equality-checked to detect
    /// staleness (SharedString compare is a cheap pointer-or-memcmp).
    code_line_cache_source: Option<SharedString>,
    /// Window-space origin of the text column (right of the gutter), last paint.
    code_origin: Option<Point<Pixels>>,
    code_line_height: Pixels,
    scroll_handle: ScrollHandle,
    undo_stack: Vec<EditSnapshot>,
    redo_stack: Vec<EditSnapshot>,
    find_input: Option<Entity<TextInput>>,
    find_visible: bool,
    find_matches: Vec<Range<usize>>,
    find_active: Option<usize>,
    search_field: bool,
    compact: bool,
    caret_blink: CaretBlink,
    /// 单行输入按下 Enter 时的提交回调（多行/代码模式仍插入换行）。
    on_enter: Option<EnterHandler>,
}

impl gpui::EventEmitter<TextInputEvent> for TextInput {}

/// 单行 Enter 提交回调的签名（如翻页条“跳至 X 页”）。
type EnterHandler = Box<dyn Fn(&mut Window, &mut App)>;

/// Byte ranges for non-overlapping matches. ASCII matching is case-insensitive;
/// non-ASCII text (including Chinese) remains exact so byte offsets stay stable.
pub(crate) fn find_matches(content: &str, query: &str) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    if query.is_ascii() {
        let haystack = content.to_ascii_lowercase();
        let needle = query.to_ascii_lowercase();
        haystack
            .match_indices(&needle)
            .map(|(start, matched)| start..start + matched.len())
            .collect()
    } else {
        content
            .match_indices(query)
            .map(|(start, matched)| start..start + matched.len())
            .collect()
    }
}

pub(crate) fn closest_match(matches: &[Range<usize>], cursor: usize) -> Option<usize> {
    if matches.is_empty() {
        None
    } else {
        Some(
            matches
                .iter()
                .position(|range| range.start >= cursor || range.contains(&cursor))
                .unwrap_or(0),
        )
    }
}

/// Shared inline find chrome used by both text controls. Buttons dispatch
/// actions back through the focused editor, preserving normal GPUI routing.
pub(crate) fn render_find_bar(
    input: Entity<TextInput>,
    active: Option<usize>,
    total: usize,
) -> gpui::Div {
    let id = input.entity_id().as_u64();
    let counter = if total == 0 {
        "0 / 0".to_string()
    } else {
        format!("{} / {total}", active.unwrap_or(0) + 1)
    };
    let control = |element_id, label: &'static str, icon_name: IconName| {
        div()
            .id((element_id, id))
            .role(gpui::Role::Button)
            .aria_label(label)
            .flex()
            .items_center()
            .justify_center()
            .w(px(26.))
            .h(px(26.))
            .rounded_md()
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .child(icon(icon_name, theme::muted(), 12.))
    };

    div()
        .absolute()
        .top(px(8.))
        .right(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .gap_1()
        .w(px(330.))
        .p_1()
        .rounded_lg()
        .border_1()
        .border_color(theme::border_strong())
        .bg(theme::overlay())
        .shadow(theme::shadow_popover())
        .occlude()
        .child(div().flex_1().min_w(px(0.)).child(input))
        .child(
            div()
                .w(px(48.))
                .text_center()
                .text_xs()
                .text_color(theme::muted())
                .child(SharedString::from(counter)),
        )
        .child(
            control(
                "find-previous",
                raw(k::COMMON_FIND_PREVIOUS),
                IconName::ChevronLeft,
            )
            .on_click(|_event, window, cx| {
                window.dispatch_action(Box::new(FindPrevious), cx);
            }),
        )
        .child(
            control(
                "find-next",
                raw(k::COMMON_FIND_NEXT),
                IconName::ChevronRight,
            )
            .on_click(|_event, window, cx| {
                window.dispatch_action(Box::new(FindNext), cx);
            }),
        )
        .child(
            control("find-close", raw(k::COMMON_FIND_CLOSE), IconName::Close).on_click(
                |_event, window, cx| {
                    window.dispatch_action(Box::new(CloseFind), cx);
                },
            ),
        )
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
            horizontal_scroll: px(0.),
            is_selecting: false,
            masked: false,
            multiline: false,
            code: false,
            code_lines: Vec::new(),
            code_first_row: 0,
            code_line_cache: Vec::new(),
            code_line_cache_source: None,
            code_origin: None,
            code_line_height: px(20.),
            scroll_handle: ScrollHandle::new(),
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            find_input: None,
            find_visible: false,
            find_matches: Vec::new(),
            find_active: None,
            search_field: false,
            compact: false,
            caret_blink: CaretBlink::default(),
            on_enter: None,
        }
    }

    /// Seed a newly-created input without emitting a change event or scheduling
    /// an extra repaint. Runtime replacements should continue to use
    /// [`Self::set_content`].
    pub fn with_content(mut self, content: impl Into<SharedString>) -> Self {
        self.content = content.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self
    }

    /// Keep caret animation owned by the input entity instead of whichever
    /// host happens to render it. This makes nested inputs (such as find bars)
    /// behave exactly like standalone fields and stops timers on blur.
    fn set_caret_focus(&mut self, focused: bool, cx: &mut Context<Self>) {
        if let Some(epoch) = self.caret_blink.set_focused(focused) {
            self.schedule_caret_blink(epoch, cx);
        }
    }

    fn reset_caret_blink(&mut self, cx: &mut Context<Self>) {
        if let Some(epoch) = self.caret_blink.reset() {
            self.schedule_caret_blink(epoch, cx);
        }
    }

    fn schedule_caret_blink(&self, epoch: usize, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CARET_BLINK_INTERVAL).await;
            this.update(cx, |this, cx| {
                if this.caret_blink.tick(epoch) {
                    cx.notify();
                    this.schedule_caret_blink(epoch, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Compact text field used by the editor's non-modal find bar.
    pub(crate) fn search_field(mut self) -> Self {
        self.search_field = true;
        self
    }

    /// Compact single-line field for pagination and dense inline controls.
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// 注册单行 Enter 提交回调（如翻页条的“跳至 X 页”）。
    pub fn set_on_enter(&mut self, callback: impl Fn(&mut Window, &mut App) + 'static) {
        self.on_enter = Some(Box::new(callback));
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

    /// Rebuild the cached per-line split when the content changed. Code-mode
    /// rendering reads from this cache every frame.
    fn ensure_code_line_cache(&mut self) {
        if self.code_line_cache_source.as_ref() == Some(&self.content) {
            return;
        }
        self.code_line_cache = self
            .content
            .split('\n')
            .map(|line| SharedString::from(line.to_string()))
            .collect();
        self.code_line_cache_source = Some(self.content.clone());
    }

    /// Shaped layout for a buffer row, if it falls inside the shaped window.
    fn code_line(&self, row: usize) -> Option<&ShapedLine> {
        self.code_lines.get(row.checked_sub(self.code_first_row)?)
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
        let starts = self.line_starts();
        let line_idx = (line_idx as usize).min(starts.len().saturating_sub(1));
        let local_x = position.x - origin.x;
        // Rows outside the shaped window (only reachable mid-drag before the
        // follow-scroll catches up) fall back to the line start.
        let col = self
            .code_line(line_idx)
            .map(|line| line.closest_index_for_x(local_x))
            .unwrap_or(0);
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
            .code_line(line)
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let starts = self.line_starts();
        let target = line - 1;
        let target_col = self
            .code_line(target)
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
            .code_line(line)
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let target = line + 1;
        let target_col = self
            .code_line(target)
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

    /// Toggle masking at runtime (reveal/hide a secret in place).
    pub fn set_masked(&mut self, masked: bool, cx: &mut Context<Self>) {
        if self.masked != masked {
            self.masked = masked;
            cx.notify();
        }
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

    pub(crate) fn select_all_content(&mut self, cx: &mut Context<Self>) {
        self.selected_range = 0..self.content.len();
        self.selection_reversed = false;
        self.reset_caret_blink(cx);
        cx.notify();
    }

    /// Replace the full content, resetting the cursor to the end.
    /// Replace the placeholder without disturbing the field's content, cursor
    /// or focus.
    ///
    /// Needed because placeholders are captured when a view is constructed, and
    /// every top-level view is built once at startup and lives for the whole
    /// process — so after a locale switch their placeholders would otherwise
    /// stay in the old language until the app restarted. Rebuilding the entity
    /// instead would drop whatever the user had typed.
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    pub fn set_content(&mut self, content: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = content.into();
        let end = self.content.len();
        self.selected_range = end..end;
        self.selection_reversed = false;
        self.marked_range = None;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.refresh_find_matches(cx);
        self.reset_caret_blink(cx);
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn snapshot(&self) -> EditSnapshot {
        EditSnapshot {
            content: self.content.clone(),
            selected_range: self.selected_range.clone(),
            selection_reversed: self.selection_reversed,
        }
    }

    fn record_undo(&mut self) {
        let snapshot = self.snapshot();
        if self.undo_stack.last() != Some(&snapshot) {
            if self.undo_stack.len() == MAX_UNDO_SNAPSHOTS {
                self.undo_stack.remove(0);
            }
            self.undo_stack.push(snapshot);
        }
        self.redo_stack.clear();
    }

    fn restore_snapshot(&mut self, snapshot: EditSnapshot, cx: &mut Context<Self>) {
        self.content = snapshot.content;
        self.selected_range = snapshot.selected_range;
        self.selection_reversed = snapshot.selection_reversed;
        self.marked_range = None;
        self.refresh_find_matches(cx);
        self.scroll_selection_into_view();
        self.reset_caret_blink(cx);
        cx.emit(TextInputEvent::Changed);
        cx.notify();
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.undo_stack.pop() else {
            window.play_system_bell();
            return;
        };
        self.redo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot, cx);
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.redo_stack.pop() else {
            window.play_system_bell();
            return;
        };
        self.undo_stack.push(self.snapshot());
        self.restore_snapshot(snapshot, cx);
    }

    fn refresh_find_matches(&mut self, cx: &mut Context<Self>) {
        let query = self
            .find_input
            .as_ref()
            .map(|input| input.read(cx).content.to_string())
            .unwrap_or_default();
        self.update_find_matches(&query, cx);
    }

    fn update_find_matches(&mut self, query: &str, cx: &mut Context<Self>) {
        if !self.find_visible {
            self.find_matches.clear();
            self.find_active = None;
            cx.notify();
            return;
        }
        self.find_matches = find_matches(&self.content, query);
        self.find_active = closest_match(&self.find_matches, self.cursor_offset());
        self.select_active_match();
        self.scroll_selection_into_view();
        cx.notify();
    }

    fn select_active_match(&mut self) {
        let Some(range) = self
            .find_active
            .and_then(|index| self.find_matches.get(index))
            .cloned()
        else {
            return;
        };
        self.selected_range = range;
        self.selection_reversed = false;
    }

    fn scroll_selection_into_view(&self) {
        if !self.code {
            return;
        }
        let (line, _) = self.line_index_for_offset(self.cursor_offset());
        let row_top = self.code_line_height * line as f32;
        let row_bottom = row_top + self.code_line_height;
        let viewport_height = self.scroll_handle.bounds().size.height;
        if viewport_height <= px(0.) {
            return;
        }
        let current = self.scroll_handle.offset();
        let current_top = -current.y;
        let target_top = if row_top < current_top {
            row_top
        } else if row_bottom > current_top + viewport_height {
            row_bottom - viewport_height
        } else {
            current_top
        }
        .clamp(px(0.), self.scroll_handle.max_offset().y);
        self.scroll_handle.set_offset(point(current.x, -target_top));
    }

    fn open_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if self.masked || self.search_field {
            window.play_system_bell();
            return;
        }
        if self.find_input.is_none() {
            let selected = if !self.selected_range.is_empty()
                && self.selected_range.len() <= 200
                && !self.content[self.selected_range.clone()].contains(['\r', '\n'])
            {
                self.content[self.selected_range.clone()].to_string()
            } else {
                String::new()
            };
            let input =
                cx.new(|cx| TextInput::new(cx, t(k::COMMON_FIND_PLACEHOLDER)).search_field());
            if !selected.is_empty() {
                input.update(cx, |input, cx| input.set_content(selected, cx));
            }
            cx.subscribe(&input, |this, input, _: &TextInputEvent, cx| {
                let query = input.read(cx).content.to_string();
                this.update_find_matches(&query, cx);
            })
            .detach();
            self.find_input = Some(input);
        }
        self.find_visible = true;
        self.refresh_find_matches(cx);
        if let Some(input) = &self.find_input {
            input.update(cx, |input, cx| input.select_all_content(cx));
            input.read(cx).focus_handle(cx).focus(window, cx);
        }
        cx.notify();
    }

    fn find_next(&mut self, _: &FindNext, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_visible {
            self.open_find(&Find, window, cx);
            return;
        }
        if self.find_matches.is_empty() {
            window.play_system_bell();
            return;
        }
        self.find_active = Some(
            self.find_active
                .map(|index| (index + 1) % self.find_matches.len())
                .unwrap_or(0),
        );
        self.select_active_match();
        self.scroll_selection_into_view();
        cx.notify();
    }

    fn find_previous(&mut self, _: &FindPrevious, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find_visible {
            self.open_find(&Find, window, cx);
            return;
        }
        if self.find_matches.is_empty() {
            window.play_system_bell();
            return;
        }
        self.find_active = Some(
            self.find_active
                .map(|index| {
                    if index == 0 {
                        self.find_matches.len() - 1
                    } else {
                        index - 1
                    }
                })
                .unwrap_or(self.find_matches.len() - 1),
        );
        self.select_active_match();
        self.scroll_selection_into_view();
        cx.notify();
    }

    fn close_find(&mut self, _: &CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        self.find_visible = false;
        self.find_matches.clear();
        self.find_active = None;
        self.focus_handle.focus(window, cx);
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
        } else if let Some(on_enter) = &self.on_enter {
            on_enter(window, cx);
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
        self.selection_reversed = false;
        self.reset_caret_blink(cx);
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
        let display = line.closest_index_for_x(position.x - bounds.left() + self.horizontal_scroll);
        raw_offset_from_display(&self.content, self.masked, display)
    }

    fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        if offset == self.cursor_offset() {
            return;
        }
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };
        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = self.selected_range.end..self.selected_range.start;
        }
        self.reset_caret_blink(cx);
        cx.notify()
    }

    fn offset_from_utf16(&self, offset: usize) -> usize {
        utf8_offset_from_utf16(&self.content, offset)
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
        let completes_composition = self.marked_range.is_some();
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        if !completes_composition {
            self.record_undo();
        }
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.selection_reversed = false;
        self.refresh_find_matches(cx);
        self.scroll_selection_into_view();
        self.reset_caret_blink(cx);
        cx.emit(TextInputEvent::Changed);
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
        let starts_composition = self.marked_range.is_none();
        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.marked_range.clone())
            .unwrap_or(self.selected_range.clone());

        if starts_composition {
            self.record_undo();
        }
        self.content =
            (self.content[0..range.start].to_owned() + new_text + &self.content[range.end..])
                .into();
        if !new_text.is_empty() {
            self.marked_range = Some(range.start..range.start + new_text.len());
        } else {
            self.marked_range = None;
        }
        // The platform reports the composition caret relative to the marked
        // text, not to the whole field, so both endpoints are measured inside
        // `new_text` and rebased onto `range.start`. Converting them against
        // `self.content` (or adding `range.end` to the end offset) yields
        // offsets past the end of the value — or in the middle of a multi-byte
        // character — which survive an unmark and panic the next time the
        // selection is sliced.
        self.selected_range = new_selected_range_utf16
            .as_ref()
            .map(|range_utf16| {
                range.start + utf8_offset_from_utf16(new_text, range_utf16.start)
                    ..range.start + utf8_offset_from_utf16(new_text, range_utf16.end)
            })
            .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len());

        self.selection_reversed = false;
        self.refresh_find_matches(cx);
        self.scroll_selection_into_view();
        self.reset_caret_blink(cx);
        cx.emit(TextInputEvent::Changed);
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
        let start = display_offset(&self.content, self.masked, range.start);
        let end = display_offset(&self.content, self.masked, range.end);
        Some(Bounds::from_corners(
            point(
                bounds.left() + last_layout.x_for_index(start) - self.horizontal_scroll,
                bounds.top(),
            ),
            point(
                bounds.left() + last_layout.x_for_index(end) - self.horizontal_scroll,
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
        let display_index = last_layout.index_for_x(line_point.x + self.horizontal_scroll)?;
        let raw_index = raw_offset_from_display(&self.content, self.masked, display_index);
        Some(self.offset_to_utf16(raw_index))
    }
}

struct TextElement {
    input: Entity<TextInput>,
}

struct PrepaintState {
    line: Option<ShapedLine>,
    cursor: Option<PaintQuad>,
    selection: Option<PaintQuad>,
    search_highlights: Vec<PaintQuad>,
    text_origin: Point<Pixels>,
    horizontal_scroll: Pixels,
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
        let selected_range = display_offset(&raw_content, masked, input.selected_range.start)
            ..display_offset(&raw_content, masked, input.selected_range.end);
        let cursor = display_offset(&raw_content, masked, input.cursor_offset());
        let marked_range = input.marked_range.as_ref().map(|range| {
            display_offset(&raw_content, masked, range.start)
                ..display_offset(&raw_content, masked, range.end)
        });
        let cursor_visible = input.caret_blink.visible();
        let focused = input.focus_handle.is_focused(window);
        let style = window.text_style();

        let display_content: SharedString = if masked {
            accessible_value(&raw_content, true)
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
            (display_content.clone(), style.color)
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let runs = if let Some(marked_range) = marked_range.as_ref() {
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
            .shape_line(display_text.clone(), font_size, &runs, None);

        let cursor_pos = line.x_for_index(cursor);
        let content_width = line.x_for_index(display_text.len());
        let horizontal_scroll = horizontal_scroll_for_caret(
            input.horizontal_scroll,
            cursor_pos,
            content_width,
            bounds.size.width,
            focused && !display_content.is_empty(),
        );
        let text_origin = point(bounds.left() - horizontal_scroll, bounds.top());
        let search_highlights = if display_content.is_empty() {
            Vec::new()
        } else {
            input
                .find_matches
                .iter()
                .map(|range| {
                    display_offset(&raw_content, masked, range.start)
                        ..display_offset(&raw_content, masked, range.end)
                })
                .filter(|range| range.end <= display_text.len() && range.start < range.end)
                .map(|range| {
                    fill(
                        Bounds::from_corners(
                            point(text_origin.x + line.x_for_index(range.start), bounds.top()),
                            point(text_origin.x + line.x_for_index(range.end), bounds.bottom()),
                        ),
                        theme::yellow_soft(),
                    )
                })
                .collect()
        };
        let (selection, cursor) = if selected_range.is_empty() {
            (
                None,
                cursor_visible.then(|| {
                    fill(
                        Bounds::new(
                            point(text_origin.x + cursor_pos, bounds.top()),
                            size(px(2.), bounds.bottom() - bounds.top()),
                        ),
                        theme::accent(),
                    )
                }),
            )
        } else {
            (
                Some(fill(
                    Bounds::from_corners(
                        point(
                            text_origin.x + line.x_for_index(selected_range.start),
                            bounds.top(),
                        ),
                        point(
                            text_origin.x + line.x_for_index(selected_range.end),
                            bounds.bottom(),
                        ),
                    ),
                    theme::selection(),
                )),
                None,
            )
        };
        PrepaintState {
            line: Some(line),
            cursor,
            selection,
            search_highlights,
            text_origin,
            horizontal_scroll,
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
        for highlight in prepaint.search_highlights.drain(..) {
            window.paint_quad(highlight);
        }
        if let Some(selection) = prepaint.selection.take() {
            window.paint_quad(selection)
        }
        let line = prepaint.line.take().unwrap();
        line.paint(
            prepaint.text_origin,
            window.line_height(),
            gpui::TextAlign::Left,
            None,
            window,
            cx,
        )
        .ok();

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        self.input.update(cx, |input, _cx| {
            input.last_layout = Some(line);
            input.last_bounds = Some(bounds);
            input.horizontal_scroll = prepaint.horizontal_scroll;
        });
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        self.set_caret_focus(focused, cx);
        let find_bar = self
            .find_visible
            .then(|| self.find_input.clone())
            .flatten()
            .map(|input| render_find_bar(input, self.find_active, self.find_matches.len()));
        let aria_value = accessible_value(&self.content, self.masked);
        let base = div()
            .id("text-input")
            .role(gpui::Role::TextInput)
            .aria_label(self.placeholder.clone())
            .aria_placeholder(self.placeholder.clone())
            .aria_value(aria_value)
            .focusable()
            .tab_stop(true)
            .flex()
            .relative()
            .key_context(if self.search_field {
                "SearchInput"
            } else {
                "TextInput"
            })
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
            .on_action(cx.listener(Self::undo))
            .on_action(cx.listener(Self::redo))
            .when(
                (self.multiline || self.code) && !self.search_field,
                |element| {
                    element
                        .on_action(cx.listener(Self::open_find))
                        .on_action(cx.listener(Self::find_next))
                        .on_action(cx.listener(Self::find_previous))
                        .on_action(cx.listener(Self::close_find))
                },
            )
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .w_full()
            .min_w_0()
            // Custom-shaped text can be wider than its layout node. Every
            // input owns the clipping boundary so secrets and long IDs can
            // never paint into adjacent fields.
            .overflow_hidden()
            .rounded_md()
            .bg(if self.search_field {
                if focused {
                    theme::surface_hover()
                } else {
                    theme::inset()
                }
            } else {
                theme::surface()
            })
            .border_1()
            .border_color(if focused && (self.multiline || self.code) {
                // Editors already expose focus through the caret and selection.
                // Keep their frame neutral so opening Find never leaves a large
                // accent-blue rectangle around the editing surface.
                theme::border_strong()
            } else if self.search_field {
                theme::border()
            } else if focused {
                theme::accent()
            } else {
                theme::border()
            })
            .text_color(theme::text())
            .text_sm()
            .line_height(px(20.))
            .when(self.search_field || self.compact, |element| {
                element.px_2().py_1()
            })
            .when(!self.search_field && !self.compact, |element| {
                element.px_3().py_2()
            });
        if self.code {
            base.font_family("Menlo")
                .relative()
                .overflow_hidden()
                .h(px(380.))
                .items_start()
                // Stopping propagation (below) only outranks ancestors that
                // registered their wheel handler before this editor painted.
                // `gpui::list` registers its own *after* painting its items, so
                // in the reverse-ordered bubble phase the list scrolls the page
                // before this editor is ever asked — which is how a form field
                // ends up dragging the whole page. Ancestors gate on
                // `should_handle_scroll`, i.e. on the hit test, so take their
                // hitboxes out of it while the pointer is inside this editor.
                // Only once it really can scroll: a short snippet must still let
                // the wheel move the page instead of dead-ending here.
                .when(self.scroll_handle.max_offset().y > px(0.), |element| {
                    element.occlude()
                })
                .child(
                    div()
                        .id(("text-input-scroll", cx.entity_id().as_u64()))
                        .w_full()
                        .h_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll_handle)
                        // Ancestors that are plain scroll containers (painted
                        // before their children) are still stopped the ordinary
                        // way. Once this editor can scroll, the gesture belongs
                        // to it alone.
                        .on_scroll_wheel(cx.listener(|this, _event, _window, cx| {
                            if this.scroll_handle.max_offset().y > px(0.) {
                                cx.stop_propagation();
                            }
                        }))
                        .child(div().w_full().child(CodeElement { input: cx.entity() })),
                )
                .child(crate::scrollbar::VerticalScrollbar::new(
                    ("text-input-scrollbar", cx.entity_id().as_u64()),
                    self.scroll_handle.clone(),
                ))
                .when_some(find_bar, |element, bar| element.child(bar))
        } else {
            base.when(self.multiline, |s| s.h(px(132.)).items_start())
                .child(div().w_full().child(TextElement { input: cx.entity() }))
                .when_some(find_bar, |element, bar| element.child(bar))
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
    /// Shaped layouts for `first_row..first_row + lines.len()` only.
    lines: Vec<ShapedLine>,
    first_row: usize,
    numbers: Vec<(ShapedLine, Pixels)>,
    selections: Vec<PaintQuad>,
    search_highlights: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    gutter: Pixels,
    line_height: Pixels,
    text_origin: Point<Pixels>,
}

/// Buffer rows worth shaping/painting: the scroll viewport plus a small
/// overdraw. Falls back to a bounded prefix before the first layout pass.
fn code_visible_rows(
    content_bounds: Bounds<Pixels>,
    viewport_bounds: Bounds<Pixels>,
    line_height: Pixels,
    row_count: usize,
) -> Range<usize> {
    if row_count == 0 {
        return 0..0;
    }
    if viewport_bounds.size.height <= px(0.) || line_height <= px(0.) {
        return 0..row_count.min(64);
    }
    const OVERDRAW: isize = 3;
    let line_height = f32::from(line_height);
    let first = (f32::from(viewport_bounds.top() - content_bounds.top()) / line_height).floor()
        as isize
        - OVERDRAW;
    let last = (f32::from(viewport_bounds.bottom() - content_bounds.top()) / line_height).ceil()
        as isize
        + OVERDRAW;
    let first = first.max(0) as usize;
    let last = last.max(first as isize).min(row_count as isize) as usize;
    first.min(row_count)..last
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
        self.input
            .update(cx, |input, _cx| input.ensure_code_line_cache());
        let input = self.input.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let text_color = style.color;
        let muted: gpui::Hsla = theme::muted().opacity(0.85).into();

        let empty = input.content.is_empty();
        let placeholder_line = [input.placeholder.clone()];
        let line_texts: &[SharedString] = if empty {
            &placeholder_line
        } else {
            &input.code_line_cache
        };
        let line_count = line_texts.len();
        let digits = line_count.to_string().len().max(2);
        let gutter = px(18. + digits as f32 * 8.5);
        let text_origin = point(bounds.left() + gutter, bounds.top());

        // Shape only the rows the scroll viewport can show: shaping every row
        // of a large document each frame is what made these editors janky.
        let row_range = code_visible_rows(
            bounds,
            input.scroll_handle.bounds(),
            line_height,
            line_count,
        );
        let first_row = row_range.start;

        let mut lines = Vec::with_capacity(row_range.len());
        let mut numbers = Vec::with_capacity(row_range.len());
        for i in row_range.clone() {
            let text = &line_texts[i];
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
        let mut search_highlights = Vec::new();
        if !empty {
            let starts = input.line_starts();
            for matched in &input.find_matches {
                for (ix, line) in lines.iter().enumerate() {
                    let i = first_row + ix;
                    let line_start = starts[i];
                    let line_end = line_start + input.line_len(i, &starts);
                    if matched.start > line_end || matched.end < line_start {
                        continue;
                    }
                    let seg_start = matched.start.max(line_start);
                    let seg_end = matched.end.min(line_end);
                    if seg_end <= seg_start {
                        continue;
                    }
                    let y = bounds.top() + line_height * i as f32;
                    search_highlights.push(fill(
                        Bounds::from_corners(
                            point(text_origin.x + line.x_for_index(seg_start - line_start), y),
                            point(
                                text_origin.x + line.x_for_index(seg_end - line_start),
                                y + line_height,
                            ),
                        ),
                        theme::yellow_soft(),
                    ));
                }
            }
        }
        let selection = input.selected_range.clone();
        if !selection.is_empty() && !empty {
            let starts = input.line_starts();
            for (ix, line) in lines.iter().enumerate() {
                let i = first_row + ix;
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
                    x1 += px(6.);
                }
                if x1 > x0 {
                    let y = bounds.top() + line_height * i as f32;
                    selections.push(fill(
                        Bounds::from_corners(
                            point(text_origin.x + x0, y),
                            point(text_origin.x + x1, y + line_height),
                        ),
                        theme::selection(),
                    ));
                }
            }
        }

        let cursor = if !input.caret_blink.visible() || !input.selected_range.is_empty() {
            None
        } else if empty {
            Some(fill(
                Bounds::new(
                    point(text_origin.x, bounds.top()),
                    size(px(2.), line_height),
                ),
                theme::accent(),
            ))
        } else {
            let (line, col) = input.line_index_for_offset(input.cursor_offset());
            let cursor_x = line
                .checked_sub(first_row)
                .and_then(|ix| lines.get(ix))
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
            first_row,
            numbers,
            selections,
            search_highlights,
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

        for highlight in prepaint.search_highlights.drain(..) {
            window.paint_quad(highlight);
        }
        for selection in prepaint.selections.drain(..) {
            window.paint_quad(selection);
        }

        let line_height = prepaint.line_height;
        let text_x = prepaint.text_origin.x;
        for (ix, line) in prepaint.lines.iter().enumerate() {
            let y = bounds.top() + line_height * (prepaint.first_row + ix) as f32;
            if let Some((number, num_x)) = prepaint.numbers.get(ix) {
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

        if focus_handle.is_focused(window)
            && let Some(cursor) = prepaint.cursor.take()
        {
            window.paint_quad(cursor);
        }

        let lines = std::mem::take(&mut prepaint.lines);
        let first_row = prepaint.first_row;
        let text_origin = prepaint.text_origin;
        self.input.update(cx, |input, _cx| {
            input.code_lines = lines;
            input.code_first_row = first_row;
            input.code_origin = Some(text_origin);
            input.code_line_height = line_height;
            input.last_bounds = Some(bounds);
        });
    }
}

#[cfg(test)]
mod tests {
    use gpui::{Bounds, point, px, size};

    use super::{
        CaretBlink, accessible_value, closest_match, code_visible_rows, display_offset,
        find_matches, horizontal_scroll_for_caret, raw_offset_from_display, utf8_offset_from_utf16,
    };

    /// The composition caret arrives as a UTF-16 offset into the marked text.
    /// Measuring it there (rather than against the whole field) is what keeps
    /// the resulting selection inside the value after an IME switch.
    #[test]
    fn composition_caret_stays_inside_the_marked_text() {
        assert_eq!(utf8_offset_from_utf16("ni", 2), 2);
        assert_eq!(utf8_offset_from_utf16("你好", 1), 3);
        // Emoji outside the BMP: a caret between the surrogates snaps to the
        // character boundary instead of splitting it.
        assert_eq!(utf8_offset_from_utf16("😀", 1), 4);
        assert_eq!(utf8_offset_from_utf16("😀", 2), 4);
        // An offset past the end saturates rather than overrunning the string.
        assert_eq!(utf8_offset_from_utf16("ni", 9), 2);
        assert_eq!(utf8_offset_from_utf16("", 3), 0);
    }

    #[test]
    fn code_mode_shapes_only_viewport_rows_with_small_overdraw() {
        let content = Bounds::new(point(px(10.), px(100.)), size(px(600.), px(20_000.)));
        let viewport = Bounds::new(point(px(10.), px(300.)), size(px(600.), px(200.)));
        assert_eq!(code_visible_rows(content, viewport, px(20.), 1_000), 7..23);
    }

    #[test]
    fn code_mode_caps_first_layout_before_scroll_bounds_are_known() {
        let content = Bounds::new(point(px(0.), px(0.)), size(px(600.), px(20_000.)));
        assert_eq!(
            code_visible_rows(content, Bounds::default(), px(20.), 1_000),
            0..64
        );
        assert_eq!(
            code_visible_rows(content, Bounds::default(), px(20.), 0),
            0..0
        );
    }

    #[test]
    fn find_is_ascii_case_insensitive_and_preserves_utf8_offsets() {
        assert_eq!(find_matches("你好 FoO foo", "foo"), vec![7..10, 11..14]);
    }

    #[test]
    fn find_supports_exact_non_ascii_queries() {
        assert_eq!(
            find_matches("网关测试，网关正常", "网关"),
            vec![0..6, 15..21]
        );
        assert!(find_matches("anything", "").is_empty());
    }

    #[test]
    fn closest_match_wraps_to_the_first_result() {
        let matches = vec![2..5, 10..13];
        assert_eq!(closest_match(&matches, 4), Some(0));
        assert_eq!(closest_match(&matches, 7), Some(1));
        assert_eq!(closest_match(&matches, 99), Some(0));
        assert_eq!(closest_match(&[], 0), None);
    }

    #[test]
    fn caret_blink_restarts_and_rejects_stale_timers() {
        let mut caret = CaretBlink::default();
        assert!(!caret.visible());

        let first_epoch = caret.set_focused(true).expect("focus starts timer");
        assert!(caret.visible());
        assert!(caret.tick(first_epoch));
        assert!(!caret.visible());

        let fresh_epoch = caret.reset().expect("interaction restarts timer");
        assert!(caret.visible());
        assert!(!caret.tick(first_epoch));
        assert!(caret.visible());
        assert!(caret.tick(fresh_epoch));
        assert!(!caret.visible());

        assert_eq!(caret.set_focused(false), None);
        assert!(!caret.tick(fresh_epoch));
        assert!(!caret.visible());

        assert!(caret.set_focused(true).is_some());
        assert!(caret.visible());
    }

    #[test]
    fn masked_offsets_follow_unicode_characters_instead_of_raw_bytes() {
        let raw = "a好🙂";
        assert_eq!(display_offset(raw, true, 0), 0);
        assert_eq!(display_offset(raw, true, 1), 3);
        assert_eq!(display_offset(raw, true, 4), 6);
        assert_eq!(display_offset(raw, true, raw.len()), 9);

        assert_eq!(raw_offset_from_display(raw, true, 0), 0);
        assert_eq!(raw_offset_from_display(raw, true, 3), 1);
        assert_eq!(raw_offset_from_display(raw, true, 6), 4);
        assert_eq!(raw_offset_from_display(raw, true, 9), raw.len());
    }

    #[test]
    fn masked_accessibility_value_never_exposes_the_secret() {
        assert_eq!(accessible_value("sk-secret", true).as_ref(), "•••••••••");
        assert_eq!(accessible_value("sk-secret", false).as_ref(), "sk-secret");
        assert_eq!(accessible_value("", true).as_ref(), "");
    }

    #[test]
    fn horizontal_scroll_keeps_the_caret_inside_the_input_viewport() {
        assert_eq!(
            horizontal_scroll_for_caret(px(0.), px(150.), px(160.), px(100.), true),
            px(54.)
        );
        assert_eq!(
            horizontal_scroll_for_caret(px(54.), px(20.), px(160.), px(100.), true),
            px(16.)
        );
        assert_eq!(
            horizontal_scroll_for_caret(px(54.), px(150.), px(160.), px(100.), false),
            px(0.)
        );
        assert_eq!(
            horizontal_scroll_for_caret(px(20.), px(80.), px(90.), px(100.), true),
            px(0.)
        );
    }
}
