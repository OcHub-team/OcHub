//! Multi-line code editor for config files, built on Zed's `text::Buffer`
//! (rope storage + transactional, time-grouped undo/redo) with lightweight
//! syntax highlighting from [`crate::highlight`].
//!
//! The GPUI element layer follows the same pattern as `text_input.rs` (which
//! itself follows `zed/crates/gpui/examples/input.rs`): an
//! [`EntityInputHandler`] for IME/keystroke text input plus a custom
//! [`Element`] that shapes each logical line and paints gutter, selection,
//! and cursor. The buffer is the source of truth; `content` is a cached
//! mirror refreshed after every mutation so layout/hit-testing can stay on
//! plain string coordinates (config files are small).

use std::collections::HashSet;
use std::ops::Range;
use std::time::{Duration, Instant};

use gpui::{
    actions, div, fill, point, prelude::*, px, size, App, Bounds, ClipboardItem, Context,
    CursorStyle, ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle,
    Focusable, GlobalElementId, Hitbox, HitboxBehavior, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ShapedLine, SharedString, Style,
    TextRun, UTF16Selection, UnderlineStyle, Window,
};
use unicode_segmentation::UnicodeSegmentation;
use zed_text::{Buffer as TextBuffer, BufferId, ReplicaId};

use crate::fold::{fold_regions, FoldRegion};
use crate::highlight::{self, Lang};
use crate::text_input::{
    Backspace, Copy, Cut, Delete, Down, End, Home, Left, Newline, Paste, Right, SelectAll,
    SelectLeft, SelectRight, ShowCharacterPalette, Up,
};
use crate::theme;

actions!(code_editor, [Undo, Redo, IndentTab]);

/// Typing pauses longer than this start a new undo group.
const UNDO_GROUP_INTERVAL: Duration = Duration::from_millis(300);

/// Cursor blink half-period (visible / hidden phase length).
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

/// Register code-editor-specific key bindings (scoped to the CodeEditor key
/// context so they never shadow global shortcuts). Call once from `main.rs`,
/// after `text_input::bind_keys`.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("cmd-z", Undo, Some("CodeEditor")),
        gpui::KeyBinding::new("cmd-shift-z", Redo, Some("CodeEditor")),
        gpui::KeyBinding::new("tab", IndentTab, Some("CodeEditor")),
    ]);
}

pub struct CodeEditor {
    focus_handle: FocusHandle,
    /// Source of truth for text + undo history.
    buffer: TextBuffer,
    /// Cached mirror of the buffer text (refreshed after every mutation).
    content: SharedString,
    placeholder: SharedString,
    lang: Lang,
    selected_range: Range<usize>,
    selection_reversed: bool,
    marked_range: Option<Range<usize>>,
    is_selecting: bool,
    /// Shaped layouts for the currently *visible* rows (post-folding),
    /// captured during the last paint; used for hit-testing and vertical
    /// cursor movement.
    lines: Vec<ShapedLine>,
    /// Buffer-line index of each visible row (post-folding), same order as
    /// `lines`.
    rows: Vec<usize>,
    /// Foldable regions for the current content.
    regions: Vec<FoldRegion>,
    /// Header lines of currently collapsed regions.
    collapsed: HashSet<usize>,
    /// Window-space origin of the text column (right of the gutter).
    text_origin: Option<Point<Pixels>>,
    line_height: Pixels,
    last_bounds: Option<Bounds<Pixels>>,
    /// Cursor blink state (Zed's BlinkManager pattern): an epoch counter
    /// invalidates in-flight timers whenever the cursor moves or edits land,
    /// so the cursor is always solid right after interaction.
    blink_epoch: usize,
    cursor_visible: bool,
}

fn make_buffer(text: &str) -> TextBuffer {
    let mut buffer = TextBuffer::new(
        ReplicaId::LOCAL,
        BufferId::new(1).expect("nonzero buffer id"),
        text.to_string(),
    );
    buffer.set_group_interval(UNDO_GROUP_INTERVAL);
    buffer
}

impl CodeEditor {
    pub fn new(cx: &mut Context<Self>, lang: Lang, placeholder: impl Into<SharedString>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            buffer: make_buffer(""),
            content: SharedString::default(),
            placeholder: placeholder.into(),
            lang,
            selected_range: 0..0,
            selection_reversed: false,
            marked_range: None,
            is_selecting: false,
            lines: Vec::new(),
            rows: Vec::new(),
            regions: Vec::new(),
            collapsed: HashSet::new(),
            text_origin: None,
            line_height: px(20.),
            last_bounds: None,
            blink_epoch: 0,
            cursor_visible: true,
        }
    }

    // ---- cursor blinking -----------------------------------------------------

    /// Show the cursor solid and restart the blink cycle (called on every
    /// cursor move / edit, and once on first focus).
    fn reset_blink(&mut self, cx: &mut Context<Self>) {
        self.cursor_visible = true;
        self.blink_epoch += 1;
        self.schedule_blink(self.blink_epoch, cx);
    }

    fn schedule_blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BLINK_INTERVAL).await;
            this.update(cx, |this, cx| {
                // A newer epoch means the cursor moved meanwhile and owns a
                // fresh timer chain; let this stale one die out.
                if this.blink_epoch == epoch {
                    this.cursor_visible = !this.cursor_visible;
                    cx.notify();
                    this.schedule_blink(epoch, cx);
                }
            })
            .ok();
        })
        .detach();
    }

    /// Current text content.
    pub fn content(&self) -> SharedString {
        self.content.clone()
    }

    /// Replace the whole document (fresh undo history), cursor at the start.
    pub fn set_content(&mut self, content: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.content = content.into();
        self.buffer = make_buffer(&self.content);
        self.selected_range = 0..0;
        self.selection_reversed = false;
        self.marked_range = None;
        self.collapsed.clear();
        self.refresh_regions();
        cx.notify();
    }

    /// Apply one edit through the buffer (grouped for undo) and refresh the
    /// mirror. `range` is a byte range into the current content.
    fn edit(&mut self, range: Range<usize>, new_text: &str) {
        let now = Instant::now();
        self.buffer.start_transaction_at(now);
        self.buffer.edit([(range, new_text)]);
        self.buffer.end_transaction_at(now);
        self.content = self.buffer.snapshot().text().into();
        self.refresh_regions();
    }

    /// Recompute fold regions after any content change; drop collapsed marks
    /// whose region no longer exists.
    fn refresh_regions(&mut self) {
        self.regions = fold_regions(self.lang, &self.content);
        let regions = &self.regions;
        self.collapsed
            .retain(|h| regions.iter().any(|r| r.header == *h));
    }

    // ---- folding -------------------------------------------------------------

    /// Buffer-line index for each visible row, honoring collapsed regions.
    fn visible_rows(&self) -> Vec<usize> {
        let line_count = self.content.split('\n').count().max(1);
        let mut hidden = vec![false; line_count];
        for region in &self.regions {
            if self.collapsed.contains(&region.header) {
                for line in region.hidden() {
                    if line < line_count {
                        hidden[line] = true;
                    }
                }
            }
        }
        (0..line_count).filter(|&l| !hidden[l]).collect()
    }

    /// The fold region headed at `line`, if any (innermost = shortest).
    fn region_at(&self, line: usize) -> Option<FoldRegion> {
        self.regions
            .iter()
            .filter(|r| r.header == line)
            .min_by_key(|r| r.last)
            .copied()
    }

    fn toggle_fold(&mut self, header: usize, cx: &mut Context<Self>) {
        if self.collapsed.contains(&header) {
            self.collapsed.remove(&header);
        } else if let Some(region) = self.region_at(header) {
            // Never leave the cursor inside a hidden range: park it at the
            // end of the header line first.
            let starts = self.line_starts();
            let (cursor_line, _) = self.line_index_for_offset(self.cursor_offset());
            if region.hidden().contains(&cursor_line) {
                let offset = starts[header] + self.line_len(header, &starts);
                self.selected_range = offset..offset;
                self.selection_reversed = false;
            }
            self.collapsed.insert(header);
        }
        cx.notify();
    }

    /// Expand any collapsed region hiding the line that contains `offset`.
    fn ensure_offset_visible(&mut self, offset: usize) {
        let (line, _) = self.line_index_for_offset(offset);
        loop {
            let hiding: Vec<usize> = self
                .regions
                .iter()
                .filter(|r| self.collapsed.contains(&r.header) && r.hidden().contains(&line))
                .map(|r| r.header)
                .collect();
            if hiding.is_empty() {
                break;
            }
            for header in hiding {
                self.collapsed.remove(&header);
            }
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.content.len();
        let start = self.selected_range.start.min(len);
        let end = self.selected_range.end.min(len);
        self.selected_range = start..end;
    }

    fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.undo().is_some() {
            self.content = self.buffer.snapshot().text().into();
            self.marked_range = None;
            self.clamp_selection();
            self.refresh_regions();
            self.ensure_offset_visible(self.cursor_offset());
            self.reset_blink(cx);
            cx.notify();
        } else {
            window.play_system_bell();
        }
    }

    fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer.redo().is_some() {
            self.content = self.buffer.snapshot().text().into();
            self.marked_range = None;
            self.clamp_selection();
            self.refresh_regions();
            self.ensure_offset_visible(self.cursor_offset());
            self.reset_blink(cx);
            cx.notify();
        } else {
            window.play_system_bell();
        }
    }

    fn indent_tab(&mut self, _: &IndentTab, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text_in_range(None, "  ", window, cx);
    }

    // ---- line geometry (byte offsets over the content mirror) --------------

    fn line_starts(&self) -> Vec<usize> {
        let mut starts = vec![0usize];
        for (idx, byte) in self.content.bytes().enumerate() {
            if byte == b'\n' {
                starts.push(idx + 1);
            }
        }
        starts
    }

    fn line_len(&self, line: usize, starts: &[usize]) -> usize {
        let start = starts[line];
        let end = if line + 1 < starts.len() {
            starts[line + 1].saturating_sub(1)
        } else {
            self.content.len()
        };
        end.saturating_sub(start)
    }

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

    /// Visible row index under a window-space y position.
    fn row_for_position(&self, position: Point<Pixels>) -> Option<usize> {
        let origin = self.text_origin?;
        if self.lines.is_empty() {
            return None;
        }
        let line_height = f32::from(self.line_height).max(1.0);
        let rel_y = f32::from(position.y - origin.y);
        let row = (rel_y / line_height).floor().max(0.0) as usize;
        Some(row.min(self.lines.len() - 1))
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(origin) = self.text_origin else {
            return self.cursor_offset();
        };
        let Some(row) = self.row_for_position(position) else {
            return 0;
        };
        let line = self.rows.get(row).copied().unwrap_or(row);
        let starts = self.line_starts();
        if line >= starts.len() {
            return self.content.len();
        }
        let local_x = position.x - origin.x;
        let col = self.lines[row].closest_index_for_x(local_x);
        let line_len = self.line_len(line, &starts);
        starts[line] + col.min(line_len)
    }

    // ---- movement & selection actions ---------------------------------------

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

    /// Visible row of the cursor's buffer line (post-folding).
    fn cursor_row(&self) -> Option<usize> {
        let (line, _) = self.line_index_for_offset(self.cursor_offset());
        self.rows.iter().position(|&l| l == line)
    }

    /// Move vertically by one *visible* row, keeping the x position.
    fn move_vertical(&mut self, delta: isize, cx: &mut Context<Self>) {
        let (_, col) = self.line_index_for_offset(self.cursor_offset());
        let Some(row) = self.cursor_row() else {
            self.move_to(if delta < 0 { 0 } else { self.content.len() }, cx);
            return;
        };
        let target_row = row as isize + delta;
        if target_row < 0 {
            self.move_to(0, cx);
            return;
        }
        let Some(&target_line) = self.rows.get(target_row as usize) else {
            self.move_to(self.content.len(), cx);
            return;
        };
        let x = self
            .lines
            .get(row)
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let target_col = self
            .lines
            .get(target_row as usize)
            .map(|l| l.closest_index_for_x(x))
            .unwrap_or(0);
        let starts = self.line_starts();
        let offset = starts[target_line] + target_col.min(self.line_len(target_line, &starts));
        self.move_to(offset, cx);
    }

    fn up(&mut self, _: &Up, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(-1, cx);
    }

    fn down(&mut self, _: &Down, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_vertical(1, cx);
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
        let (line, _) = self.line_index_for_offset(self.cursor_offset());
        let starts = self.line_starts();
        self.move_to(starts[line], cx);
    }

    fn end(&mut self, _: &End, _: &mut Window, cx: &mut Context<Self>) {
        let (line, _) = self.line_index_for_offset(self.cursor_offset());
        let starts = self.line_starts();
        self.move_to(starts[line] + self.line_len(line, &starts), cx);
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

    fn newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        // Auto-indent: carry the current line's leading whitespace over.
        let (line, _) = self.line_index_for_offset(self.cursor_offset());
        let starts = self.line_starts();
        let start = starts[line];
        let line_text = &self.content[start..start + self.line_len(line, &starts)];
        let indent: String = line_text
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let insert = format!("\n{indent}");
        self.replace_text_in_range(None, &insert, window, cx);
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

    fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.replace_text_in_range(None, &text, window, cx);
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

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Clicks in the gutter toggle the fold region headed on that row.
        if let (Some(origin), Some(bounds)) = (self.text_origin, self.last_bounds) {
            if event.position.x < origin.x && event.position.x >= bounds.left() {
                if let Some(row) = self.row_for_position(event.position) {
                    if let Some(&line) = self.rows.get(row) {
                        if self.region_at(line).is_some() {
                            self.toggle_fold(line, cx);
                            return;
                        }
                    }
                }
            }
        }
        self.is_selecting = true;
        if event.modifiers.shift {
            self.select_to(self.offset_for_position(event.position), cx);
        } else {
            self.move_to(self.offset_for_position(event.position), cx)
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, _: &mut Context<Self>) {
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.is_selecting {
            self.select_to(self.offset_for_position(event.position), cx);
        }
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.ensure_offset_visible(offset);
        self.reset_blink(cx);
        cx.notify()
    }

    fn cursor_offset(&self) -> usize {
        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
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
        self.ensure_offset_visible(offset);
        self.reset_blink(cx);
        cx.notify()
    }

    // ---- UTF-16 <-> UTF-8 mapping (IME contract) ----------------------------

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

impl EntityInputHandler for CodeEditor {
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

        self.edit(range.clone(), new_text);
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.ensure_offset_visible(self.selected_range.start);
        self.reset_blink(cx);
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

        self.edit(range.clone(), new_text);
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

        self.reset_blink(cx);
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // Approximate: anchor the IME candidate window to the cursor line.
        let range = self.range_from_utf16(&range_utf16);
        let (line, col) = self.line_index_for_offset(range.start);
        let origin = self.text_origin?;
        let x = self
            .lines
            .get(line)
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let _ = bounds;
        let y = origin.y + self.line_height * line as f32;
        Some(Bounds::new(
            point(origin.x + x, y),
            size(px(2.), self.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let offset = self.offset_for_position(point);
        Some(self.offset_to_utf16(offset))
    }
}

/// The painting element: shapes every logical line with syntax-highlight
/// runs, paints a line-number gutter, cross-line selection, and the cursor.
struct CodeEditorElement {
    editor: Entity<CodeEditor>,
}

struct PrepaintState {
    lines: Vec<ShapedLine>,
    /// Buffer-line index per visible row.
    rows: Vec<usize>,
    numbers: Vec<(ShapedLine, Pixels)>,
    /// Fold chevrons: (shaped glyph, x) per visible row, when foldable.
    chevrons: Vec<Option<(ShapedLine, Pixels)>>,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    gutter: Pixels,
    line_height: Pixels,
    text_origin: Point<Pixels>,
    /// Cursor-style regions: I-beam over text, arrow over the gutter,
    /// pointing hand over fold chevron rows.
    text_hitbox: Hitbox,
    gutter_hitbox: Hitbox,
    chevron_hitboxes: Vec<Hitbox>,
}

impl IntoElement for CodeEditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for CodeEditorElement {
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
        let row_count = {
            let editor = self.editor.read(cx);
            editor.visible_rows().len().max(1)
        };
        let line_height = window.line_height();
        let mut style = Style::default();
        style.size.width = gpui::relative(1.).into();
        style.size.height = (line_height * row_count as f32).into();
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
        let editor = self.editor.read(cx);
        let style = window.text_style();
        let font_size = style.font_size.to_pixels(window.rem_size());
        let line_height = window.line_height();
        let muted: gpui::Hsla = theme::muted().opacity(0.85).into();

        const FOLD_MARKER: &str = " ⋯";

        let empty = editor.content.is_empty();
        let all_lines: Vec<&str> = editor.content.split('\n').collect();
        let rows: Vec<usize> = if empty {
            vec![0]
        } else {
            editor.visible_rows()
        };
        let line_count = all_lines.len();
        let digits = line_count.to_string().len().max(2);
        let gutter = px(30. + digits as f32 * 8.5);
        let text_origin = point(bounds.left() + gutter, bounds.top());

        let starts = editor.line_starts();
        let marked = editor.marked_range.clone();

        let mut lines = Vec::with_capacity(rows.len());
        let mut numbers = Vec::with_capacity(rows.len());
        let mut chevrons = Vec::with_capacity(rows.len());
        for &line_idx in &rows {
            let (display_text, runs): (SharedString, Vec<TextRun>) = if empty {
                let text = editor.placeholder.clone();
                let run = TextRun {
                    len: text.len(),
                    font: style.font(),
                    color: muted,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                (text, vec![run])
            } else {
                let text = all_lines.get(line_idx).copied().unwrap_or("");
                let line_start = starts.get(line_idx).copied().unwrap_or(0);
                let marked_local = marked.as_ref().and_then(|m| {
                    let line_end = line_start + text.len();
                    (m.start < line_end && m.end > line_start).then(|| {
                        m.start.saturating_sub(line_start).min(text.len())
                            ..(m.end - line_start).min(text.len())
                    })
                });
                let mut runs = highlight_runs(editor.lang, text, &style, marked_local);
                let folded = editor.collapsed.contains(&line_idx);
                let display: SharedString = if folded {
                    // Collapsed header: append a muted `⋯` marker.
                    runs.push(TextRun {
                        len: FOLD_MARKER.len(),
                        font: style.font(),
                        color: muted,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    });
                    SharedString::from(format!("{text}{FOLD_MARKER}"))
                } else {
                    SharedString::from(text.to_string())
                };
                (display, runs)
            };
            lines.push(
                window
                    .text_system()
                    .shape_line(display_text, font_size, &runs, None),
            );

            let num_text = SharedString::from((line_idx + 1).to_string());
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

            // Fold chevron at the far left of the gutter.
            let chevron = (!empty && editor.region_at(line_idx).is_some()).then(|| {
                let glyph: SharedString = if editor.collapsed.contains(&line_idx) {
                    "▸".into()
                } else {
                    "▾".into()
                };
                let run = TextRun {
                    len: glyph.len(),
                    font: style.font(),
                    color: theme::muted().opacity(0.9).into(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window
                    .text_system()
                    .shape_line(glyph, font_size, &[run], None);
                (shaped, bounds.left() + px(4.))
            });
            chevrons.push(chevron);
        }

        let mut selections = Vec::new();
        let selection = editor.selected_range.clone();
        if !selection.is_empty() && !empty {
            for (row, &line_idx) in rows.iter().enumerate() {
                let line_start = starts[line_idx];
                let line_end = line_start + editor.line_len(line_idx, &starts);
                if selection.start > line_end || selection.end < line_start {
                    continue;
                }
                let seg_start = selection.start.max(line_start);
                let seg_end = selection.end.min(line_end);
                let line = &lines[row];
                let x0 = line.x_for_index(seg_start - line_start);
                let mut x1 = line.x_for_index(seg_end - line_start);
                let includes_newline = selection.end > line_end;
                if includes_newline {
                    x1 = x1 + px(6.);
                }
                if x1 > x0 {
                    let y = bounds.top() + line_height * row as f32;
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

        let cursor = if !editor.cursor_visible {
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
            let (line, col) = editor.line_index_for_offset(editor.cursor_offset());
            rows.iter().position(|&l| l == line).map(|row| {
                let cursor_x = lines.get(row).map(|l| l.x_for_index(col)).unwrap_or(px(0.));
                let y = bounds.top() + line_height * row as f32;
                fill(
                    Bounds::new(
                        point(text_origin.x + cursor_x, y),
                        size(px(2.), line_height),
                    ),
                    theme::accent(),
                )
            })
        };

        // Cursor-style hitboxes: gutter (arrow), chevron rows (hand), text (I-beam).
        let gutter_hitbox = window.insert_hitbox(
            Bounds::new(bounds.origin, size(gutter, bounds.size.height)),
            HitboxBehavior::Normal,
        );
        let text_hitbox = window.insert_hitbox(
            Bounds::new(
                point(text_origin.x, bounds.top()),
                size((bounds.size.width - gutter).max(px(0.)), bounds.size.height),
            ),
            HitboxBehavior::Normal,
        );
        let chevron_hitboxes = chevrons
            .iter()
            .enumerate()
            .filter(|(_, c)| c.is_some())
            .map(|(row, _)| {
                window.insert_hitbox(
                    Bounds::new(
                        point(bounds.left(), bounds.top() + line_height * row as f32),
                        size(gutter, line_height),
                    ),
                    HitboxBehavior::Normal,
                )
            })
            .collect();

        PrepaintState {
            lines,
            rows,
            numbers,
            chevrons,
            selections,
            cursor,
            gutter,
            line_height,
            text_origin,
            text_hitbox,
            gutter_hitbox,
            chevron_hitboxes,
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
        let focus_handle = self.editor.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.editor.clone()),
            cx,
        );

        // Per-region mouse cursors (later pushes win where regions overlap).
        window.set_cursor_style(CursorStyle::Arrow, &prepaint.gutter_hitbox);
        window.set_cursor_style(CursorStyle::IBeam, &prepaint.text_hitbox);
        for hitbox in &prepaint.chevron_hitboxes {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }

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
            if let Some(Some((chevron, chev_x))) = prepaint.chevrons.get(i) {
                chevron
                    .paint(
                        point(*chev_x, y),
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
        let rows = std::mem::take(&mut prepaint.rows);
        let text_origin = prepaint.text_origin;
        self.editor.update(cx, |editor, _cx| {
            editor.lines = lines;
            editor.rows = rows;
            editor.text_origin = Some(text_origin);
            editor.line_height = line_height;
            editor.last_bounds = Some(bounds);
        });
    }
}

/// Build syntax-highlight `TextRun`s for one line, splitting further at the
/// IME marked range (rendered with an underline) when present.
fn highlight_runs(
    lang: Lang,
    line: &str,
    style: &gpui::TextStyle,
    marked_local: Option<Range<usize>>,
) -> Vec<TextRun> {
    let spans = highlight::line_spans(lang, line);
    let mut runs = Vec::with_capacity(spans.len());
    let mut offset = 0usize;
    for (len, token) in spans {
        let color: gpui::Hsla = token.color().into();
        let range = offset..offset + len;
        match marked_local
            .as_ref()
            .filter(|m| m.start < range.end && m.end > range.start)
        {
            None => runs.push(TextRun {
                len,
                font: style.font(),
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }),
            Some(marked) => {
                // Split this span into [before][marked][after].
                let m_start = marked.start.max(range.start);
                let m_end = marked.end.min(range.end);
                for (seg_start, seg_end, underlined) in [
                    (range.start, m_start, false),
                    (m_start, m_end, true),
                    (m_end, range.end, false),
                ] {
                    if seg_end > seg_start {
                        runs.push(TextRun {
                            len: seg_end - seg_start,
                            font: style.font(),
                            color,
                            background_color: None,
                            underline: underlined.then(|| UnderlineStyle {
                                color: Some(color),
                                thickness: px(1.0),
                                wavy: false,
                            }),
                            strikethrough: None,
                        });
                    }
                }
            }
        }
        offset += len;
    }
    if runs.is_empty() {
        runs.push(TextRun {
            len: line.len(),
            font: style.font(),
            color: style.color,
            background_color: None,
            underline: None,
            strikethrough: None,
        });
    }
    runs
}

impl Render for CodeEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let focused = self.focus_handle.is_focused(window);
        // Arm the blink cycle the first time we render focused (covers focus
        // via tab / programmatic focus, before any mouse or key interaction).
        if focused && self.blink_epoch == 0 {
            self.reset_blink(cx);
        }
        div()
            .id("code-editor")
            .role(gpui::Role::TextInput)
            .aria_label(self.placeholder.clone())
            .focusable()
            .tab_stop(true)
            .flex()
            .key_context("CodeEditor")
            .track_focus(&self.focus_handle(cx))
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
            .on_action(cx.listener(Self::indent_tab))
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
            .line_height(px(20.))
            .font_family("Menlo")
            .h(px(380.))
            .items_start()
            .overflow_y_scroll()
            .child(div().w_full().child(CodeEditorElement {
                editor: cx.entity(),
            }))
    }
}

impl Focusable for CodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}
