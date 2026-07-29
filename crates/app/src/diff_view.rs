//! Side-by-side text diff, for showing a user what a write is about to change.
//!
//! A drift conflict is two versions of the same thing — what the file says and
//! what OcHub is about to write. Printed as two blobs they are unreadable; the
//! only question the user actually has is *which lines differ*, so this lays the
//! two versions out in parallel columns and marks exactly those lines, the way
//! `git diff --side-by-side` does.
//!
//! Runs of identical lines are folded away past a couple of lines of context: a
//! 400-line `config.toml` where one key moved should read as one changed line,
//! not as 400 lines the user has to scan.

use gpui::prelude::*;
use gpui::{Div, SharedString, div, px};

use crate::theme;

/// One line on one side of the diff.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffCell {
    /// 1-based line number in its own version.
    pub number: usize,
    pub text: String,
}

/// What happened to a row, which decides how both cells are tinted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Same,
    Changed,
    /// Only in the old version.
    Removed,
    /// Only in the new version.
    Added,
    /// A run of identical lines that was folded away.
    Folded(usize),
}

/// One rendered row: a line from either side, or from both.
#[derive(Debug, Clone, PartialEq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub old: Option<DiffCell>,
    pub new: Option<DiffCell>,
}

impl DiffRow {
    fn folded(count: usize) -> Self {
        Self {
            kind: RowKind::Folded(count),
            old: None,
            new: None,
        }
    }
}

/// Lines of unchanged text kept on either side of a change, for orientation.
const CONTEXT: usize = 2;

/// Build the side-by-side rows for two versions of a text.
///
/// `max_rows` caps the result; the count of rows that did not fit is returned
/// alongside, because silently showing the first few would read as "that is all
/// of them".
pub fn side_by_side(old: &str, new: &str, max_rows: usize) -> (Vec<DiffRow>, usize) {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(old, new);
    let mut rows: Vec<DiffRow> = Vec::new();

    // `grouped_ops` already drops the runs of unchanged lines between changes;
    // what it hands back is the changes plus their context, group by group.
    let groups = diff.grouped_ops(CONTEXT);
    let mut previous_end = (0usize, 0usize);

    for group in &groups {
        let Some(start_old) = group.first().map(|op| op.old_range().start) else {
            continue;
        };
        let skipped = start_old.saturating_sub(previous_end.0);
        if skipped > 0 {
            rows.push(DiffRow::folded(skipped));
        }

        // Deletions and insertions inside one op are paired up, so a changed
        // line sits opposite the line it replaced instead of below it.
        let mut pending_old: Vec<DiffCell> = Vec::new();
        let mut pending_new: Vec<DiffCell> = Vec::new();

        for op in group {
            for change in diff.iter_changes(op) {
                let text = change
                    .value()
                    .trim_end_matches('\n')
                    .trim_end_matches('\r')
                    .to_string();
                match change.tag() {
                    ChangeTag::Equal => {
                        flush_pending(&mut rows, &mut pending_old, &mut pending_new);
                        rows.push(DiffRow {
                            kind: RowKind::Same,
                            old: change.old_index().map(|index| DiffCell {
                                number: index + 1,
                                text: text.clone(),
                            }),
                            new: change.new_index().map(|index| DiffCell {
                                number: index + 1,
                                text,
                            }),
                        });
                    }
                    ChangeTag::Delete => {
                        if let Some(index) = change.old_index() {
                            pending_old.push(DiffCell {
                                number: index + 1,
                                text,
                            });
                        }
                    }
                    ChangeTag::Insert => {
                        if let Some(index) = change.new_index() {
                            pending_new.push(DiffCell {
                                number: index + 1,
                                text,
                            });
                        }
                    }
                }
            }
            previous_end = (op.old_range().end, op.new_range().end);
        }
        flush_pending(&mut rows, &mut pending_old, &mut pending_new);
    }

    let trailing = diff.old_slices().len().saturating_sub(previous_end.0);
    if trailing > 0 && !rows.is_empty() {
        rows.push(DiffRow::folded(trailing));
    }

    let hidden = rows.len().saturating_sub(max_rows);
    rows.truncate(max_rows);
    (rows, hidden)
}

/// Pair the deleted and inserted lines of one change into rows.
fn flush_pending(rows: &mut Vec<DiffRow>, old: &mut Vec<DiffCell>, new: &mut Vec<DiffCell>) {
    let paired = old.len().min(new.len());
    for (old_cell, new_cell) in old.drain(..paired).zip(new.drain(..paired)) {
        rows.push(DiffRow {
            kind: RowKind::Changed,
            old: Some(old_cell),
            new: Some(new_cell),
        });
    }
    for cell in old.drain(..) {
        rows.push(DiffRow {
            kind: RowKind::Removed,
            old: Some(cell),
            new: None,
        });
    }
    for cell in new.drain(..) {
        rows.push(DiffRow {
            kind: RowKind::Added,
            old: None,
            new: Some(cell),
        });
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

const LINE_HEIGHT: f32 = 18.;
const GUTTER: f32 = 30.;

fn cell_background(kind: RowKind, is_old: bool) -> Option<gpui::Rgba> {
    match kind {
        RowKind::Same | RowKind::Folded(_) => None,
        RowKind::Changed => Some(if is_old {
            theme::red_soft()
        } else {
            theme::green_soft()
        }),
        RowKind::Removed => is_old.then(theme::red_soft),
        RowKind::Added => (!is_old).then(theme::green_soft),
    }
}

fn marker(kind: RowKind, is_old: bool) -> &'static str {
    match kind {
        RowKind::Changed => {
            if is_old {
                "-"
            } else {
                "+"
            }
        }
        RowKind::Removed if is_old => "-",
        RowKind::Added if !is_old => "+",
        _ => " ",
    }
}

/// One side of one row: line number, change marker, and the line itself.
fn render_cell(row: &DiffRow, is_old: bool) -> Div {
    let cell = if is_old { &row.old } else { &row.new };
    let background = cell_background(row.kind, is_old);

    // Lines wrap rather than clip: a value that differs only past the width of
    // the column is exactly the case the user opened this dialog to see.
    let mut side = div()
        .flex()
        .flex_row()
        .flex_1()
        .min_w_0()
        .min_h(px(LINE_HEIGHT))
        .py(px(1.))
        .items_start();
    if let Some(background) = background {
        side = side.bg(background);
    }

    let Some(cell) = cell else {
        // The other version has no counterpart for this line. Left blank on
        // purpose: an empty half is what makes an addition read as an addition.
        return side;
    };

    side.child(
        div()
            .flex_none()
            .w(px(GUTTER))
            .pr_1()
            .text_right()
            .text_color(theme::muted())
            .text_xs()
            .child(SharedString::from(cell.number.to_string())),
    )
    .child(
        div()
            .flex_none()
            .w(px(10.))
            .text_color(theme::muted())
            .text_xs()
            .child(marker(row.kind, is_old)),
    )
    .child(
        div()
            .flex_1()
            .min_w_0()
            .pr_2()
            .font_family("Menlo")
            .text_xs()
            .text_color(theme::text())
            .child(SharedString::from(cell.text.clone())),
    )
}

/// The two column headers, e.g. "In the file" / "Will write".
pub fn header_row(old_label: SharedString, new_label: SharedString) -> Div {
    let label = |text: SharedString, tone: gpui::Rgba| {
        div()
            .flex_1()
            .min_w_0()
            .px_2()
            .py_1()
            .text_color(tone)
            .text_xs()
            .child(text)
    };

    div()
        .flex()
        .flex_row()
        .items_center()
        .border_b_1()
        .border_color(theme::border())
        .bg(theme::inset())
        .child(label(old_label, theme::red()))
        .child(divider())
        .child(label(new_label, theme::green()))
}

fn divider() -> Div {
    div().flex_none().w(px(1.)).h_full().bg(theme::border())
}

/// The rows of one diff, framed. `folded_label` and `truncated_label` render the
/// two ways this view admits to hiding something.
pub fn render(
    rows: &[DiffRow],
    hidden: usize,
    header: Option<Div>,
    folded_label: impl Fn(usize) -> SharedString,
    truncated_label: impl Fn(usize) -> SharedString,
) -> Div {
    let mut body = div().flex().flex_col().w_full();

    for row in rows {
        body = body.child(match row.kind {
            RowKind::Folded(count) => div()
                .flex()
                .flex_row()
                .items_center()
                .h(px(LINE_HEIGHT))
                .px_2()
                .bg(theme::inset())
                .text_color(theme::muted())
                .text_xs()
                .child(folded_label(count)),
            _ => div()
                .flex()
                .flex_row()
                .w_full()
                .child(render_cell(row, true))
                .child(divider())
                .child(render_cell(row, false)),
        });
    }

    if hidden > 0 {
        body = body.child(
            div()
                .px_2()
                .py_1()
                .text_color(theme::muted())
                .text_xs()
                .child(truncated_label(hidden)),
        );
    }

    div()
        .flex()
        .flex_col()
        .w_full()
        .rounded_md()
        .border_1()
        .border_color(theme::border())
        .overflow_hidden()
        .children(header)
        .child(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(rows: &[DiffRow]) -> Vec<(RowKind, Option<&str>, Option<&str>)> {
        rows.iter()
            .map(|row| {
                (
                    row.kind,
                    row.old.as_ref().map(|cell| cell.text.as_str()),
                    row.new.as_ref().map(|cell| cell.text.as_str()),
                )
            })
            .collect()
    }

    #[test]
    fn a_changed_line_sits_opposite_the_line_it_replaces() {
        let (rows, hidden) = side_by_side("a\nb\nc\n", "a\nB\nc\n", 40);

        assert_eq!(hidden, 0);
        assert_eq!(
            texts(&rows),
            vec![
                (RowKind::Same, Some("a"), Some("a")),
                (RowKind::Changed, Some("b"), Some("B")),
                (RowKind::Same, Some("c"), Some("c")),
            ]
        );
    }

    #[test]
    fn an_added_line_leaves_the_other_column_empty() {
        let (rows, _) = side_by_side("a\n", "a\nb\n", 40);

        assert_eq!(
            texts(&rows),
            vec![
                (RowKind::Same, Some("a"), Some("a")),
                (RowKind::Added, None, Some("b")),
            ]
        );
    }

    #[test]
    fn identical_text_produces_no_rows_at_all() {
        let (rows, hidden) = side_by_side("a\nb\n", "a\nb\n", 40);

        assert!(rows.is_empty());
        assert_eq!(hidden, 0);
    }

    #[test]
    fn a_long_run_of_untouched_lines_is_folded_away() {
        let old: String = (0..40).map(|n| format!("line {n}\n")).collect();
        let new = old.replace("line 20", "line twenty");

        let (rows, _) = side_by_side(&old, &new, 40);

        // Two lines of context on each side, one changed line, and one fold
        // marker standing in for everything before and after it.
        assert!(rows.len() < 10, "expected a folded diff, got {rows:?}");
        assert!(matches!(rows[0].kind, RowKind::Folded(18)));
        assert!(rows.iter().any(|row| row.kind == RowKind::Changed
            && row.new.as_ref().is_some_and(|c| c.text == "line twenty")));
        assert!(matches!(
            rows.last().map(|row| row.kind),
            Some(RowKind::Folded(17))
        ));
    }

    #[test]
    fn the_row_cap_reports_what_it_dropped() {
        let old: String = (0..60).map(|n| format!("line {n}\n")).collect();
        let new: String = (0..60).map(|n| format!("changed {n}\n")).collect();

        let (rows, hidden) = side_by_side(&old, &new, 10);

        assert_eq!(rows.len(), 10);
        assert_eq!(hidden, 50);
    }
}
