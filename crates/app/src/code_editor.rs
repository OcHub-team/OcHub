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
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, ScrollHandle, ShapedLine, SharedString,
    Style, TextRun, UTF16Selection, UnderlineStyle, Window,
};
use unicode_segmentation::UnicodeSegmentation;
use zed_text::{Buffer as TextBuffer, BufferId, ReplicaId};

use crate::fold::{fold_regions, FoldRegion};
use crate::highlight::{self, Lang};
use crate::text_input::{
    closest_match, find_matches, render_find_bar, Backspace, CaretBlink, CloseFind, Copy, Cut,
    Delete, Down, End, Find, FindNext, FindPrevious, Home, Left, Newline, Paste, Redo, Right,
    SelectAll, SelectLeft, SelectRight, ShowCharacterPalette, TextInput, Undo, Up,
};
use crate::theme;

actions!(code_editor, [IndentTab]);

/// Typing pauses longer than this start a new undo group.
const UNDO_GROUP_INTERVAL: Duration = Duration::from_millis(300);

/// Cursor blink half-period (visible / hidden phase length).
const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const STRUCTURE_REFRESH_DELAY: Duration = Duration::from_millis(160);
const SCROLLBAR_TRACK_WIDTH: f32 = 10.;
const SCROLLBAR_TRACK_INSET: f32 = 4.;
const SCROLLBAR_MIN_THUMB_HEIGHT: f32 = 32.;
const HORIZONTAL_CONTENT_PADDING: f32 = 24.;
const HORIZONTAL_CARET_PADDING: f32 = 12.;

fn horizontal_scroll_for_caret(
    current_left: Pixels,
    caret_x: Pixels,
    viewport_width: Pixels,
    max_scroll: Pixels,
) -> Pixels {
    if viewport_width <= px(0.) || max_scroll <= px(0.) {
        return px(0.);
    }
    let padding = px(HORIZONTAL_CARET_PADDING);
    let right_edge = current_left + viewport_width - padding;
    if caret_x < current_left + padding {
        caret_x - padding
    } else if caret_x > right_edge {
        caret_x - viewport_width + padding
    } else {
        current_left
    }
    .clamp(px(0.), max_scroll)
}

#[derive(Clone, Copy, Debug)]
struct VerticalScrollbarGeometry {
    track_bounds: Bounds<Pixels>,
    thumb_bounds: Bounds<Pixels>,
    max_scroll: Pixels,
}

#[derive(Clone, Copy, Debug)]
struct ScrollbarDrag {
    grab_offset: Pixels,
}

/// Register code-editor-specific key bindings (scoped to the CodeEditor key
/// context so they never shadow global shortcuts). Call once from `main.rs`,
/// after `text_input::bind_keys`.
pub fn bind_keys(cx: &mut App) {
    cx.bind_keys([gpui::KeyBinding::new("tab", IndentTab, Some("CodeEditor"))]);
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
    /// Shaped layouts for only the rows currently inside the scroll viewport.
    lines: Vec<ShapedLine>,
    /// Buffer-line index for each entry in `lines`.
    rows: Vec<usize>,
    /// First visible row represented by `lines[0]`.
    painted_row_start: usize,
    /// Byte offset for the start of every buffer line.
    line_starts_cache: Vec<usize>,
    /// Post-folding buffer-line indices. Rebuilt only when content/folds change.
    visible_rows_cache: Vec<usize>,
    /// With no collapsed region, visible row `n` is buffer line `n`. Keeping
    /// that mapping implicit avoids rebuilding a hundred-thousand-item vector
    /// after every edit.
    visible_rows_identity: bool,
    /// Foldable regions for the current content.
    regions: Vec<FoldRegion>,
    /// Header lines of currently collapsed regions.
    collapsed: HashSet<usize>,
    /// Window-space origin of the text column (right of the gutter).
    text_origin: Option<Point<Pixels>>,
    line_height: Pixels,
    last_bounds: Option<Bounds<Pixels>>,
    /// Shared caret state invalidates stale timers whenever focus, cursor, or
    /// content changes, so refocusing always starts from a visible caret.
    caret_blink: CaretBlink,
    scroll_handle: ScrollHandle,
    /// Widest content width observed while shaping visible rows. This grows
    /// lazily instead of scanning/shaping the whole document just to discover
    /// one long line, which keeps opening very large files cheap.
    measured_content_width: Pixels,
    /// Last measured gutter width, used to keep keyboard cursor movement
    /// horizontally visible.
    gutter_width: Pixels,
    /// Last painted scrollbar geometry, in window coordinates. Mouse events
    /// use this exact geometry so the visible thumb and drag target cannot
    /// drift apart after scrolling or resizing.
    scrollbar_geometry: Option<VerticalScrollbarGeometry>,
    scrollbar_drag: Option<ScrollbarDrag>,
    find_input: Option<Entity<TextInput>>,
    find_visible: bool,
    find_matches: Vec<Range<usize>>,
    find_active: Option<usize>,
    structure_refresh_epoch: usize,
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
            painted_row_start: 0,
            line_starts_cache: vec![0],
            visible_rows_cache: Vec::new(),
            visible_rows_identity: true,
            regions: Vec::new(),
            collapsed: HashSet::new(),
            text_origin: None,
            line_height: px(20.),
            last_bounds: None,
            caret_blink: CaretBlink::default(),
            scroll_handle: ScrollHandle::new(),
            measured_content_width: px(0.),
            gutter_width: px(0.),
            scrollbar_geometry: None,
            scrollbar_drag: None,
            find_input: None,
            find_visible: false,
            find_matches: Vec::new(),
            find_active: None,
            structure_refresh_epoch: 0,
        }
    }

    // ---- cursor blinking -----------------------------------------------------

    /// Show the cursor solid and restart the blink cycle (called on every
    /// cursor move / edit, and once on first focus).
    fn reset_blink(&mut self, cx: &mut Context<Self>) {
        if let Some(epoch) = self.caret_blink.reset() {
            self.schedule_blink(epoch, cx);
        }
    }

    fn set_caret_focus(&mut self, focused: bool, cx: &mut Context<Self>) {
        if let Some(epoch) = self.caret_blink.set_focused(focused) {
            self.schedule_blink(epoch, cx);
        }
    }

    fn schedule_blink(&mut self, epoch: usize, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(BLINK_INTERVAL).await;
            this.update(cx, |this, cx| {
                // A newer epoch means the cursor moved meanwhile and owns a
                // fresh timer chain; let this stale one die out.
                if this.caret_blink.tick(epoch) {
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
        self.measured_content_width = px(0.);
        self.gutter_width = px(0.);
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        self.refresh_line_index();
        self.schedule_structure_refresh(cx);
        self.refresh_find_matches(cx);
        self.reset_blink(cx);
        cx.notify();
    }

    fn refresh_find_matches(&mut self, cx: &mut Context<Self>) {
        let query = self
            .find_input
            .as_ref()
            .map(|input| input.read(cx).content().to_string())
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
        self.ensure_offset_visible(range.start);
        self.selected_range = range;
        self.selection_reversed = false;
    }

    fn scroll_selection_into_view(&self) {
        let (line, column) = self.line_index_for_offset(self.cursor_offset());
        let Some(row) = self.visible_row_for_line(line) else {
            return;
        };
        let row_top = self.line_height * row as f32;
        let row_bottom = row_top + self.line_height;
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

        let viewport_width = self.scroll_handle.bounds().size.width;
        let max_x = self.scroll_handle.max_offset().x;
        let target_left = row
            .checked_sub(self.painted_row_start)
            .and_then(|painted| self.lines.get(painted))
            .filter(|_| viewport_width > px(0.) && max_x > px(0.))
            .map(|layout| {
                let caret_x = self.gutter_width + layout.x_for_index(column);
                let current_left = -current.x;
                horizontal_scroll_for_caret(current_left, caret_x, viewport_width, max_x)
            })
            .unwrap_or(-current.x);

        self.scroll_handle
            .set_offset(point(-target_left, -target_top));
    }

    fn open_find(&mut self, _: &Find, window: &mut Window, cx: &mut Context<Self>) {
        if self.find_input.is_none() {
            let selected = if !self.selected_range.is_empty()
                && self.selected_range.len() <= 200
                && !self.content[self.selected_range.clone()].contains('\r')
                && !self.content[self.selected_range.clone()].contains('\n')
            {
                self.content[self.selected_range.clone()].to_string()
            } else {
                String::new()
            };
            let input = cx.new(|cx| {
                TextInput::new(cx, crate::i18n::t(crate::i18n::k::COMMON_FIND_PLACEHOLDER))
                    .search_field()
            });
            if !selected.is_empty() {
                input.update(cx, |input, cx| input.set_content(selected, cx));
            }
            cx.subscribe(&input, |this, input, _event, cx| {
                let query = input.read(cx).content().to_string();
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
        self.reset_blink(cx);
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
        self.reset_blink(cx);
        cx.notify();
    }

    fn close_find(&mut self, _: &CloseFind, window: &mut Window, cx: &mut Context<Self>) {
        self.find_visible = false;
        self.find_matches.clear();
        self.find_active = None;
        self.focus_handle.focus(window, cx);
        self.reset_blink(cx);
        cx.notify();
    }

    /// Apply one edit through the buffer (grouped for undo) and refresh the
    /// mirror. `range` is a byte range into the current content.
    fn edit(&mut self, range: Range<usize>, new_text: &str, cx: &mut Context<Self>) {
        self.line_starts_cache = line_starts_after_edit(&self.line_starts_cache, &range, new_text);
        let now = Instant::now();
        self.buffer.start_transaction_at(now);
        self.buffer.edit([(range, new_text)]);
        self.buffer.end_transaction_at(now);
        self.content = self.buffer.snapshot().text().into();
        // Folding is presentation-only. Clear stale fold coordinates now and
        // rescan after the typing burst rather than parsing a huge file on
        // every keystroke.
        self.regions.clear();
        self.collapsed.clear();
        self.refresh_visible_rows();
        self.schedule_structure_refresh(cx);
    }

    /// Refresh the byte line index immediately; the more expensive syntax fold
    /// scan is scheduled separately.
    fn refresh_line_index(&mut self) {
        self.structure_refresh_epoch = self.structure_refresh_epoch.wrapping_add(1);
        self.line_starts_cache.clear();
        self.line_starts_cache.push(0);
        self.line_starts_cache.extend(
            self.content
                .bytes()
                .enumerate()
                .filter_map(|(index, byte)| (byte == b'\n').then_some(index + 1)),
        );
        self.regions.clear();
        self.collapsed.clear();
        self.refresh_visible_rows();
    }

    fn schedule_structure_refresh(&mut self, cx: &mut Context<Self>) {
        self.structure_refresh_epoch = self.structure_refresh_epoch.wrapping_add(1);
        let epoch = self.structure_refresh_epoch;
        let lang = self.lang;
        let content = self.content.clone();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(STRUCTURE_REFRESH_DELAY)
                .await;
            let current = this
                .update(cx, |this, _cx| this.structure_refresh_epoch == epoch)
                .unwrap_or(false);
            if !current {
                return;
            }
            let regions = cx
                .background_spawn(async move { fold_regions(lang, &content) })
                .await;
            this.update(cx, |this, cx| {
                if this.structure_refresh_epoch == epoch {
                    this.regions = regions;
                    this.collapsed
                        .retain(|header| this.regions.iter().any(|r| r.header == *header));
                    if !this.collapsed.is_empty() {
                        this.refresh_visible_rows();
                    }
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    // ---- folding -------------------------------------------------------------

    /// Buffer-line index for each visible row, honoring collapsed regions.
    fn refresh_visible_rows(&mut self) {
        let line_count = self.line_starts_cache.len().max(1);
        if self.collapsed.is_empty() {
            self.visible_rows_identity = true;
            self.visible_rows_cache.clear();
            return;
        }
        self.visible_rows_identity = false;
        let mut hidden_delta = vec![0i32; line_count + 1];
        for region in &self.regions {
            if self.collapsed.contains(&region.header) {
                let start = (region.header + 1).min(line_count);
                let end = region.last.saturating_add(1).min(line_count);
                if start < end {
                    hidden_delta[start] += 1;
                    hidden_delta[end] -= 1;
                }
            }
        }
        self.visible_rows_cache.clear();
        self.visible_rows_cache.reserve(line_count);
        let mut depth = 0i32;
        for (line, delta) in hidden_delta.into_iter().take(line_count).enumerate() {
            depth += delta;
            if depth == 0 {
                self.visible_rows_cache.push(line);
            }
        }
    }

    fn visible_row_count(&self) -> usize {
        if self.visible_rows_identity {
            self.line_starts_cache.len().max(1)
        } else {
            self.visible_rows_cache.len()
        }
    }

    fn line_for_visible_row(&self, row: usize) -> Option<usize> {
        if self.visible_rows_identity {
            (row < self.line_starts_cache.len().max(1)).then_some(row)
        } else {
            self.visible_rows_cache.get(row).copied()
        }
    }

    fn visible_row_for_line(&self, line: usize) -> Option<usize> {
        if self.visible_rows_identity {
            (line < self.line_starts_cache.len().max(1)).then_some(line)
        } else {
            self.visible_rows_cache.binary_search(&line).ok()
        }
    }

    /// The fold region headed at `line`, if any (innermost = shortest).
    fn region_at(&self, line: usize) -> Option<FoldRegion> {
        let first = self.regions.partition_point(|region| region.header < line);
        self.regions[first..]
            .iter()
            .take_while(|region| region.header == line)
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
                let offset = starts[header] + self.line_len(header, starts);
                self.selected_range = offset..offset;
                self.selection_reversed = false;
            }
            self.collapsed.insert(header);
        }
        self.refresh_visible_rows();
        cx.notify();
    }

    /// Expand any collapsed region hiding the line that contains `offset`.
    fn ensure_offset_visible(&mut self, offset: usize) {
        if self.collapsed.is_empty() {
            return;
        }
        let (line, _) = self.line_index_for_offset(offset);
        let mut changed = false;
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
                changed |= self.collapsed.remove(&header);
            }
        }
        if changed {
            self.refresh_visible_rows();
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
            self.refresh_line_index();
            self.schedule_structure_refresh(cx);
            self.ensure_offset_visible(self.cursor_offset());
            self.refresh_find_matches(cx);
            self.scroll_selection_into_view();
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
            self.refresh_line_index();
            self.schedule_structure_refresh(cx);
            self.ensure_offset_visible(self.cursor_offset());
            self.refresh_find_matches(cx);
            self.scroll_selection_into_view();
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

    fn line_starts(&self) -> &[usize] {
        &self.line_starts_cache
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
        let line = self
            .line_starts_cache
            .partition_point(|start| *start <= offset)
            .saturating_sub(1)
            .min(self.line_starts_cache.len().saturating_sub(1));
        (line, offset.saturating_sub(self.line_starts_cache[line]))
    }

    fn line_text(&self, line: usize) -> &str {
        let starts = self.line_starts();
        let Some(&start) = starts.get(line) else {
            return "";
        };
        let len = self.line_len(line, starts);
        &self.content[start..start + len]
    }

    /// Visible row index under a window-space y position.
    fn row_for_position(&self, position: Point<Pixels>) -> Option<usize> {
        let origin = self.text_origin?;
        let row_count = self.visible_row_count();
        if row_count == 0 {
            return None;
        }
        let line_height = f32::from(self.line_height).max(1.0);
        let rel_y = f32::from(position.y - origin.y);
        let row = (rel_y / line_height).floor().max(0.0) as usize;
        Some(row.min(row_count - 1))
    }

    fn offset_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(origin) = self.text_origin else {
            return self.cursor_offset();
        };
        let Some(row) = self.row_for_position(position) else {
            return 0;
        };
        let line = self.line_for_visible_row(row).unwrap_or(row);
        let starts = self.line_starts();
        if line >= starts.len() {
            return self.content.len();
        }
        let local_x = position.x - origin.x;
        let painted = row.saturating_sub(self.painted_row_start);
        let col = self
            .lines
            .get(painted)
            .map(|line| line.closest_index_for_x(local_x))
            .unwrap_or(0);
        let line_len = self.line_len(line, starts);
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
        self.visible_row_for_line(line)
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
        let Some(target_line) = self.line_for_visible_row(target_row as usize) else {
            self.move_to(self.content.len(), cx);
            return;
        };
        let painted_row = row.checked_sub(self.painted_row_start);
        let painted_target = (target_row as usize).checked_sub(self.painted_row_start);
        let x = self
            .lines
            .get(painted_row.unwrap_or(usize::MAX))
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let target_col = self
            .lines
            .get(painted_target.unwrap_or(usize::MAX))
            .map(|l| l.closest_index_for_x(x))
            .unwrap_or(col);
        let starts = self.line_starts();
        let offset = starts[target_line] + target_col.min(self.line_len(target_line, starts));
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
        self.move_to(starts[line] + self.line_len(line, starts), cx);
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
        let line_text = &self.content[start..start + self.line_len(line, starts)];
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
        if let Some(scrollbar) = self
            .scrollbar_geometry
            .filter(|scrollbar| scrollbar.track_bounds.contains(&event.position))
        {
            let grab_offset = if scrollbar.thumb_bounds.contains(&event.position) {
                event.position.y - scrollbar.thumb_bounds.top()
            } else {
                scrollbar.thumb_bounds.size.height * 0.5
            };
            self.is_selecting = false;
            self.scrollbar_drag = Some(ScrollbarDrag { grab_offset });
            self.drag_scrollbar_to(event.position.y, cx);
            cx.stop_propagation();
            return;
        }

        // Clicks in the gutter toggle the fold region headed on that row.
        if let (Some(origin), Some(bounds)) = (self.text_origin, self.last_bounds) {
            if event.position.x < origin.x && event.position.x >= bounds.left() {
                if let Some(row) = self.row_for_position(event.position) {
                    if let Some(line) = self.line_for_visible_row(row) {
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

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.scrollbar_drag.take().is_some() {
            cx.stop_propagation();
            cx.notify();
        }
        self.is_selecting = false;
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.scrollbar_drag.is_some() {
            self.drag_scrollbar_to(event.position.y, cx);
            cx.stop_propagation();
            return;
        }
        if self.is_selecting {
            self.select_to(self.offset_for_position(event.position), cx);
        }
    }

    fn drag_scrollbar_to(&mut self, pointer_y: Pixels, cx: &mut Context<Self>) {
        let (Some(scrollbar), Some(drag)) = (self.scrollbar_geometry, self.scrollbar_drag) else {
            return;
        };
        let scroll_y = scroll_amount_for_thumb_top(&scrollbar, pointer_y - drag.grab_offset);
        let current = self.scroll_handle.offset();
        let next_y = -scroll_y;
        if f32::from((current.y - next_y).abs()) < 0.5 {
            return;
        }
        self.scroll_handle.set_offset(point(current.x, next_y));
        cx.notify();
    }

    fn move_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.selected_range = offset..offset;
        self.ensure_offset_visible(offset);
        self.scroll_selection_into_view();
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
        self.ensure_offset_visible(offset);
        self.scroll_selection_into_view();
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

        self.edit(range.clone(), new_text, cx);
        self.selected_range = range.start + new_text.len()..range.start + new_text.len();
        self.marked_range.take();
        self.selection_reversed = false;
        self.ensure_offset_visible(self.selected_range.start);
        self.refresh_find_matches(cx);
        self.scroll_selection_into_view();
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

        self.edit(range.clone(), new_text, cx);
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

        self.selection_reversed = false;
        self.refresh_find_matches(cx);
        self.scroll_selection_into_view();
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
        let visible_row = self.visible_row_for_line(line)?;
        let painted_row = visible_row.checked_sub(self.painted_row_start);
        let x = self
            .lines
            .get(painted_row.unwrap_or(usize::MAX))
            .map(|l| l.x_for_index(col))
            .unwrap_or(px(0.));
        let _ = bounds;
        let y = origin.y + self.line_height * visible_row as f32;
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
    /// Buffer-line index per painted row.
    rows: Vec<usize>,
    /// Post-folding row index represented by `lines[0]`.
    first_row: usize,
    numbers: Vec<(ShapedLine, Pixels)>,
    /// Fold chevrons: (shaped glyph, x) per visible row, when foldable.
    chevrons: Vec<Option<(ShapedLine, Pixels)>>,
    search_highlights: Vec<PaintQuad>,
    selections: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    gutter: Pixels,
    measured_content_width: Pixels,
    line_height: Pixels,
    text_origin: Point<Pixels>,
    paint_bounds: Bounds<Pixels>,
    /// Cursor-style regions: I-beam over text, arrow over the gutter,
    /// pointing hand over fold chevron rows.
    text_hitbox: Hitbox,
    gutter_hitbox: Hitbox,
    chevron_hitboxes: Vec<Hitbox>,
    scrollbar: Option<ScrollbarPrepaintState>,
}

struct ScrollbarPrepaintState {
    geometry: VerticalScrollbarGeometry,
    hitbox: Hitbox,
}

fn line_starts_after_edit(starts: &[usize], replaced: &Range<usize>, inserted: &str) -> Vec<usize> {
    let prefix_end = starts.partition_point(|start| *start <= replaced.start);
    let suffix_start = starts.partition_point(|start| *start <= replaced.end);
    let removed = replaced.end.saturating_sub(replaced.start);
    let delta = inserted.len() as isize - removed as isize;
    let inserted_lines = inserted.bytes().filter(|byte| *byte == b'\n').count();
    let mut updated = Vec::with_capacity(prefix_end + inserted_lines + starts.len() - suffix_start);
    updated.extend_from_slice(&starts[..prefix_end]);
    for (index, byte) in inserted.bytes().enumerate() {
        if byte == b'\n' {
            updated.push(replaced.start + index + 1);
        }
    }
    for &start in &starts[suffix_start..] {
        let shifted = (start as isize + delta).max(0) as usize;
        if updated.last().copied() != Some(shifted) {
            updated.push(shifted);
        }
    }
    if updated.first().copied() != Some(0) {
        updated.insert(0, 0);
    }
    updated
}

fn painted_row_range(
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

fn vertical_scrollbar_geometry(
    viewport_bounds: Bounds<Pixels>,
    scroll_offset: Pixels,
    max_scroll: Pixels,
) -> Option<VerticalScrollbarGeometry> {
    let viewport_width = f32::from(viewport_bounds.size.width);
    let viewport_height = f32::from(viewport_bounds.size.height);
    let max_scroll = f32::from(max_scroll).max(0.);
    let track_height = viewport_height - SCROLLBAR_TRACK_INSET * 2.;
    if viewport_width <= SCROLLBAR_TRACK_WIDTH + SCROLLBAR_TRACK_INSET
        || track_height <= 0.
        || max_scroll <= 0.
    {
        return None;
    }

    let track_bounds = Bounds::new(
        point(
            viewport_bounds.right() - px(SCROLLBAR_TRACK_INSET + SCROLLBAR_TRACK_WIDTH),
            viewport_bounds.top() + px(SCROLLBAR_TRACK_INSET),
        ),
        size(px(SCROLLBAR_TRACK_WIDTH), px(track_height)),
    );
    let content_height = viewport_height + max_scroll;
    let thumb_height = (track_height * viewport_height / content_height)
        .max(SCROLLBAR_MIN_THUMB_HEIGHT)
        .min(track_height);
    let thumb_travel = (track_height - thumb_height).max(0.);
    let progress = (-f32::from(scroll_offset) / max_scroll).clamp(0., 1.);
    let thumb_bounds = Bounds::new(
        point(
            track_bounds.left(),
            track_bounds.top() + px(thumb_travel * progress),
        ),
        size(px(SCROLLBAR_TRACK_WIDTH), px(thumb_height)),
    );

    Some(VerticalScrollbarGeometry {
        track_bounds,
        thumb_bounds,
        max_scroll: px(max_scroll),
    })
}

fn scroll_amount_for_thumb_top(scrollbar: &VerticalScrollbarGeometry, thumb_top: Pixels) -> Pixels {
    let travel = f32::from(scrollbar.track_bounds.size.height - scrollbar.thumb_bounds.size.height);
    if travel <= 0. {
        return px(0.);
    }
    let position = f32::from(thumb_top - scrollbar.track_bounds.top());
    px(f32::from(scrollbar.max_scroll) * (position / travel).clamp(0., 1.))
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
            editor.visible_row_count().max(1)
        };
        let measured_content_width = self.editor.read(cx).measured_content_width;
        let line_height = window.line_height();
        let mut style = Style::default();
        style.size.width = measured_content_width.max(px(1.)).into();
        style.min_size.width = gpui::relative(1.).into();
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
        let line_count = editor.line_starts_cache.len().max(1);
        let viewport_bounds = editor.scroll_handle.bounds();
        let row_range = painted_row_range(
            bounds,
            viewport_bounds,
            line_height,
            editor.visible_row_count(),
        );
        let first_row = row_range.start;
        let rows = row_range
            .filter_map(|row| editor.line_for_visible_row(row))
            .collect::<Vec<_>>();
        let paint_bounds = if viewport_bounds.size.height > px(0.) {
            bounds.intersect(&viewport_bounds)
        } else {
            Bounds::new(
                bounds.origin,
                size(
                    bounds.size.width,
                    (line_height * rows.len().max(1) as f32).min(bounds.size.height),
                ),
            )
        };
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
                let text = editor.line_text(line_idx);
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
        let measured_content_width = lines
            .iter()
            .map(ShapedLine::width)
            .fold(px(0.), Pixels::max)
            + gutter
            + px(HORIZONTAL_CONTENT_PADDING);

        let mut search_highlights = Vec::new();
        if !empty {
            for (row, &line_idx) in rows.iter().enumerate() {
                let line_start = starts[line_idx];
                let line_end = line_start + editor.line_len(line_idx, starts);
                let first = editor
                    .find_matches
                    .partition_point(|matched| matched.end <= line_start);
                for matched in editor.find_matches[first..]
                    .iter()
                    .take_while(|matched| matched.start < line_end)
                {
                    let seg_start = matched.start.max(line_start);
                    let seg_end = matched.end.min(line_end);
                    if seg_end <= seg_start {
                        continue;
                    }
                    let y = bounds.top() + line_height * (first_row + row) as f32;
                    search_highlights.push(fill(
                        Bounds::from_corners(
                            point(
                                text_origin.x + lines[row].x_for_index(seg_start - line_start),
                                y,
                            ),
                            point(
                                text_origin.x + lines[row].x_for_index(seg_end - line_start),
                                y + line_height,
                            ),
                        ),
                        theme::yellow_soft(),
                    ));
                }
            }
        }

        let mut selections = Vec::new();
        let selection = editor.selected_range.clone();
        if !selection.is_empty() && !empty {
            for (row, &line_idx) in rows.iter().enumerate() {
                let line_start = starts[line_idx];
                let line_end = line_start + editor.line_len(line_idx, starts);
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
                    x1 += px(6.);
                }
                if x1 > x0 {
                    let y = bounds.top() + line_height * (first_row + row) as f32;
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

        let cursor = if !editor.caret_blink.visible() || !editor.selected_range.is_empty() {
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
                let y = bounds.top() + line_height * (first_row + row) as f32;
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
        let gutter_right = text_origin
            .x
            .clamp(paint_bounds.left(), paint_bounds.right());
        let gutter_bounds = Bounds::from_corners(
            paint_bounds.origin,
            point(gutter_right, paint_bounds.bottom()),
        );
        let gutter_hitbox = window.insert_hitbox(gutter_bounds, HitboxBehavior::Normal);
        let text_left = text_origin.x.max(paint_bounds.left());
        let text_hitbox = window.insert_hitbox(
            Bounds::from_corners(
                point(text_left, paint_bounds.top()),
                point(paint_bounds.right(), paint_bounds.bottom()),
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
                        point(
                            bounds.left(),
                            bounds.top() + line_height * (first_row + row) as f32,
                        ),
                        size(gutter, line_height),
                    ),
                    HitboxBehavior::Normal,
                )
            })
            .collect();
        let scrollbar = vertical_scrollbar_geometry(
            viewport_bounds,
            editor.scroll_handle.offset().y,
            editor.scroll_handle.max_offset().y,
        )
        .map(|geometry| ScrollbarPrepaintState {
            hitbox: window.insert_hitbox(geometry.track_bounds, HitboxBehavior::Normal),
            geometry,
        });

        PrepaintState {
            lines,
            rows,
            first_row,
            numbers,
            chevrons,
            search_highlights,
            selections,
            cursor,
            gutter,
            measured_content_width,
            line_height,
            text_origin,
            paint_bounds,
            text_hitbox,
            gutter_hitbox,
            chevron_hitboxes,
            scrollbar,
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
            ElementInputHandler::new(prepaint.paint_bounds, self.editor.clone()),
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
                point(bounds.left(), prepaint.paint_bounds.top()),
                size(prepaint.gutter, prepaint.paint_bounds.size.height),
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
        for (i, line) in prepaint.lines.iter().enumerate() {
            let y = bounds.top() + line_height * (prepaint.first_row + i) as f32;
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

        if let Some(scrollbar) = &prepaint.scrollbar {
            window.set_cursor_style(CursorStyle::Arrow, &scrollbar.hitbox);
            let hovered = scrollbar.hitbox.is_hovered(window);
            let dragging = self.editor.read(cx).scrollbar_drag.is_some();
            window.paint_quad(
                fill(
                    scrollbar.geometry.track_bounds,
                    theme::border().opacity(if hovered || dragging { 0.42 } else { 0.24 }),
                )
                .corner_radii(px(SCROLLBAR_TRACK_WIDTH * 0.5)),
            );
            window.paint_quad(
                fill(
                    scrollbar.geometry.thumb_bounds,
                    theme::muted().opacity(if dragging {
                        0.95
                    } else if hovered {
                        0.82
                    } else {
                        0.62
                    }),
                )
                .corner_radii(px(SCROLLBAR_TRACK_WIDTH * 0.5)),
            );
        }

        let lines = std::mem::take(&mut prepaint.lines);
        let rows = std::mem::take(&mut prepaint.rows);
        let painted_row_start = prepaint.first_row;
        let text_origin = prepaint.text_origin;
        let paint_bounds = prepaint.paint_bounds;
        let scrollbar_geometry = prepaint
            .scrollbar
            .as_ref()
            .map(|scrollbar| scrollbar.geometry);
        let measured_content_width = prepaint.measured_content_width;
        self.editor.update(cx, |editor, cx| {
            let width_grew = measured_content_width > editor.measured_content_width + px(0.5);
            if width_grew {
                editor.measured_content_width = measured_content_width;
            }
            editor.lines = lines;
            editor.rows = rows;
            editor.painted_row_start = painted_row_start;
            editor.text_origin = Some(text_origin);
            editor.line_height = line_height;
            editor.gutter_width = prepaint.gutter;
            editor.last_bounds = Some(paint_bounds);
            editor.scrollbar_geometry = scrollbar_geometry;
            if width_grew {
                cx.notify();
            }
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
        self.set_caret_focus(focused, cx);
        let find_bar = self
            .find_visible
            .then(|| self.find_input.clone())
            .flatten()
            .map(|input| render_find_bar(input, self.find_active, self.find_matches.len()));
        let mut editor_scroll = div()
            .id(("code-editor-scroll", cx.entity_id().as_u64()))
            // GPUI's block layout constrains a direct child to the viewport
            // width, even when that child requests a wider layout. Make the
            // viewport a flex container and keep the content item from
            // shrinking so long logical lines create a real X scroll range.
            .flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .w_full()
            .h_full()
            .overflow_scroll()
            .track_scroll(&self.scroll_handle)
            // Contain wheel gestures: without this, GPUI also scrolls every
            // scrollable ancestor under the pointer (modal body, page), which
            // reads as the whole page lurching.
            .on_scroll_wheel(cx.listener(|this, _event, _window, cx| {
                let max_offset = this.scroll_handle.max_offset();
                if max_offset.x > px(0.) || max_offset.y > px(0.) {
                    cx.stop_propagation();
                }
            }))
            .child(div().flex_none().child(CodeEditorElement {
                editor: cx.entity(),
            }));
        // Regular wheel gestures remain vertical; horizontal trackpad deltas
        // (or Shift + wheel) move X without hijacking the page's Y axis.
        editor_scroll.style().restrict_scroll_to_axis = Some(true);
        div()
            .id("code-editor")
            .role(gpui::Role::TextInput)
            .aria_label(self.placeholder.clone())
            .focusable()
            .tab_stop(true)
            .flex()
            .relative()
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
            .on_action(cx.listener(Self::open_find))
            .on_action(cx.listener(Self::find_next))
            .on_action(cx.listener(Self::find_previous))
            .on_action(cx.listener(Self::close_find))
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
                // The caret/selection already communicate focus; a neutral
                // frame avoids the distracting full-editor blue focus ring.
                theme::border_strong()
            } else {
                theme::border()
            })
            .text_color(theme::text())
            .text_sm()
            .line_height(px(20.))
            .font_family("Menlo")
            .h(px(380.))
            .items_start()
            .overflow_hidden()
            .child(editor_scroll)
            .when_some(find_bar, |element, bar| element.child(bar))
    }
}

impl Focusable for CodeEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        horizontal_scroll_for_caret, line_starts_after_edit, painted_row_range,
        scroll_amount_for_thumb_top, vertical_scrollbar_geometry,
    };
    use gpui::{point, px, size, Bounds};

    #[test]
    fn editor_shapes_only_viewport_rows_with_small_overdraw() {
        let content = Bounds::new(point(px(10.), px(100.)), size(px(600.), px(20_000_000.)));
        let viewport = Bounds::new(point(px(10.), px(300.)), size(px(600.), px(200.)));
        assert_eq!(
            painted_row_range(content, viewport, px(20.), 1_000_000),
            7..23
        );
    }

    #[test]
    fn editor_caps_first_layout_before_scroll_bounds_are_known() {
        let content = Bounds::new(point(px(0.), px(0.)), size(px(600.), px(20_000.)));
        assert_eq!(
            painted_row_range(content, Bounds::default(), px(20.), 1_000),
            0..64
        );
    }

    #[test]
    fn line_index_updates_incrementally_across_insert_replace_and_delete() {
        let starts = vec![0, 2, 4]; // "a\nb\nc"
        assert_eq!(line_starts_after_edit(&starts, &(2..2), "z"), vec![0, 2, 5]);
        assert_eq!(
            line_starts_after_edit(&starts, &(1..4), "\nxx\n"),
            vec![0, 2, 5]
        );
        assert_eq!(line_starts_after_edit(&starts, &(0..2), ""), vec![0, 2]);
    }

    #[test]
    fn horizontal_scroll_keeps_the_code_caret_visible() {
        assert_eq!(
            horizontal_scroll_for_caret(px(0.), px(800.), px(600.), px(400.)),
            px(212.)
        );
        assert_eq!(
            horizontal_scroll_for_caret(px(212.), px(50.), px(600.), px(400.)),
            px(38.)
        );
        assert_eq!(
            horizontal_scroll_for_caret(px(38.), px(300.), px(600.), px(400.)),
            px(38.)
        );
        assert_eq!(
            horizontal_scroll_for_caret(px(38.), px(300.), px(600.), px(0.)),
            px(0.)
        );
    }

    #[test]
    fn scrollbar_thumb_maps_the_full_scroll_range() {
        let viewport = Bounds::new(point(px(20.), px(40.)), size(px(600.), px(400.)));
        let top = vertical_scrollbar_geometry(viewport, px(0.), px(1_600.)).unwrap();
        let middle = vertical_scrollbar_geometry(viewport, px(-800.), px(1_600.)).unwrap();
        let bottom = vertical_scrollbar_geometry(viewport, px(-1_600.), px(1_600.)).unwrap();
        let travel = top.track_bounds.size.height - top.thumb_bounds.size.height;

        assert!((f32::from(top.thumb_bounds.top() - top.track_bounds.top())).abs() < 0.01);
        assert!(
            (f32::from(middle.thumb_bounds.top() - middle.track_bounds.top())
                - f32::from(travel) * 0.5)
                .abs()
                < 0.01
        );
        assert!(
            (f32::from(bottom.thumb_bounds.top() - bottom.track_bounds.top()) - f32::from(travel))
                .abs()
                < 0.01
        );
        assert!(
            f32::from(
                scroll_amount_for_thumb_top(
                    &top,
                    top.track_bounds.bottom() - top.thumb_bounds.size.height,
                ) - px(1_600.)
            )
            .abs()
                < 0.01
        );
    }

    #[test]
    fn scrollbar_is_hidden_when_the_document_fits() {
        let viewport = Bounds::new(point(px(0.), px(0.)), size(px(600.), px(400.)));
        assert!(vertical_scrollbar_geometry(viewport, px(0.), px(0.)).is_none());
    }
}
