// SPDX-License-Identifier: Apache-2.0
//! Drawing primitives above the cell grid.
//!
//! [`Screen`] knows only about cells; these are the shapes built from them —
//! rectangles, borders, selectable rows. Kept separate so the dialogs describe
//! what they want rather than repeating border arithmetic, and so that
//! arithmetic is tested once rather than in each dialog.
//!
//! Everything here clips rather than panicking. A dialog that does not fit is a
//! normal condition — a terminal can be three rows tall — and the alternative to
//! clipping is a crash in the middle of asking the user to approve something.

use crate::screen::{Screen, Style};
use crate::width::str_width;

/// A rectangle in cell coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub row:  usize,
    pub col:  usize,
    pub rows: usize,
    pub cols: usize,
}

impl Rect {
    pub fn new(row: usize, col: usize, rows: usize, cols: usize) -> Self {
        Self { row, col, rows, cols }
    }

    /// The area inside a single-cell border.
    ///
    /// Saturates rather than underflowing: a 1×1 rect has no inside, and the
    /// answer is an empty rect, not a panic.
    pub fn inset(&self, by: usize) -> Rect {
        Rect {
            row:  self.row + by,
            col:  self.col + by,
            rows: self.rows.saturating_sub(by * 2),
            cols: self.cols.saturating_sub(by * 2),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0 || self.cols == 0
    }

    /// The bottom `rows` of a screen, full width — where dialogs live.
    pub fn bottom(screen: &Screen, rows: usize) -> Rect {
        let rows = rows.min(screen.rows());
        Rect {
            row:  screen.rows().saturating_sub(rows),
            col:  0,
            rows,
            cols: screen.cols(),
        }
    }
}

/// Draw a single-cell border, returning the area inside it.
///
/// A rect under 2×2 has no room for both borders and content; nothing is drawn
/// and an empty rect comes back, so callers skip their content too.
pub fn frame(screen: &mut Screen, area: Rect, style: Style) -> Rect {
    if area.rows < 2 || area.cols < 2 {
        return Rect::new(area.row, area.col, 0, 0);
    }

    let last_row = area.row + area.rows - 1;
    let last_col = area.col + area.cols - 1;
    let horizontal = "─".repeat(area.cols.saturating_sub(2));

    screen.put(area.row, area.col, "╭", style);
    screen.put(area.row, area.col + 1, &horizontal, style);
    screen.put(area.row, last_col, "╮", style);

    for row in (area.row + 1)..last_row {
        screen.put(row, area.col, "│", style);
        screen.put(row, last_col, "│", style);
    }

    screen.put(last_row, area.col, "╰", style);
    screen.put(last_row, area.col + 1, &horizontal, style);
    screen.put(last_row, last_col, "╯", style);

    area.inset(1)
}

/// Draw a title into the top border of a framed area.
pub fn title(screen: &mut Screen, area: Rect, text: &str, style: Style) {
    if area.cols < 6 || text.is_empty() {
        return;
    }
    // Leave the corners and a space either side of the text.
    let budget = area.cols.saturating_sub(4);
    let text = clip(text, budget);
    screen.put(area.row, area.col + 2, &format!(" {text} "), style);
}

/// One row of a selectable list.
pub struct Row<'a> {
    pub label:       &'a str,
    /// Shown dimmed after the label, when there is room.
    pub description: &'a str,
    pub selected:    bool,
    /// Rendered as a checkbox when the list is multi-select.
    pub checked:     Option<bool>,
}

/// Draw a list of selectable rows into `area`, one per row.
///
/// Returns how many rows were drawn, which is fewer than `rows.len()` when the
/// area is too short.
pub fn list(screen: &mut Screen, area: Rect, rows: &[Row<'_>], accent: u8) -> usize {
    if area.is_empty() {
        return 0;
    }
    let mut drawn = 0;

    for (i, row) in rows.iter().enumerate() {
        if i >= area.rows {
            break;
        }
        let y = area.row + i;

        let marker = if row.selected { "❯ " } else { "  " };
        let style = if row.selected {
            Style::fg(accent).bold()
        } else {
            Style::default()
        };
        let mut col = screen.put(y, area.col, marker, Style::fg(accent));

        if let Some(checked) = row.checked {
            col = screen.put(y, col, if checked { "[x] " } else { "[ ] " }, style);
        }

        col = screen.put(y, col, row.label, style);

        // The description is the first thing to go when space runs short.
        if !row.description.is_empty() {
            let used = col.saturating_sub(area.col);
            let left = area.cols.saturating_sub(used).saturating_sub(2);
            if left > 4 {
                let text = clip(row.description, left);
                screen.put(y, col + 2, &text, Style::fg(245).dim());
            }
        }
        drawn += 1;
    }

    drawn
}

/// Draw wrapped text into `area`, returning how many rows it used.
pub fn text_block(screen: &mut Screen, area: Rect, text: &str, style: Style) -> usize {
    if area.is_empty() {
        return 0;
    }
    let mut used = 0;
    for line in crate::width::wrap(text, area.cols) {
        if used >= area.rows {
            break;
        }
        screen.put(area.row + used, area.col, &line, style);
        used += 1;
    }
    used
}

/// A one-row proportional bar, e.g. context use.
pub fn gauge(screen: &mut Screen, row: usize, col: usize, cols: usize, fraction: f32, style: Style) {
    if cols == 0 {
        return;
    }
    // Clamp defensively: a NaN fraction would make `as usize` unpredictable.
    let f = if fraction.is_finite() { fraction.clamp(0.0, 1.0) } else { 0.0 };
    let filled = ((cols as f32) * f).round() as usize;
    let filled = filled.min(cols);
    screen.put(row, col, &"█".repeat(filled), style);
    screen.put(row, col + filled, &"░".repeat(cols - filled), Style::fg(238));
}

/// Truncate to `cols` cells without splitting a glyph, adding an ellipsis when
/// something was removed.
pub fn clip(text: &str, cols: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    if str_width(text) <= cols {
        return text.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    // Reserve a cell for the ellipsis so the result still fits.
    let budget = cols.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for cluster in text.graphemes(true) {
        let cw = crate::width::cluster_width(cluster);
        if w + cw > budget {
            break;
        }
        out.push_str(cluster);
        w += cw;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered(screen: &mut Screen) -> String {
        let mut sink = Vec::new();
        screen.flush(&mut sink).unwrap();
        String::from_utf8_lossy(&sink).into_owned()
    }

    /// What the user would see. The renderer emits only changed cells, so a
    /// substring search on its output misses text split by an unchanged blank.
    fn grid(screen: &Screen) -> String {
        (0..screen.rows()).map(|r| screen.row_text(r)).collect::<Vec<_>>().join("\n")
    }

    // ── Rect ──────────────────────────────────────────────────────────────

    #[test]
    fn inset_saturates_rather_than_underflowing() {
        let tiny = Rect::new(0, 0, 1, 1).inset(1);
        assert!(tiny.is_empty(), "a 1x1 rect has no inside");
        let none = Rect::new(0, 0, 0, 0).inset(5);
        assert!(none.is_empty());
    }

    #[test]
    fn inset_shrinks_by_two_per_axis() {
        let inner = Rect::new(2, 3, 10, 20).inset(1);
        assert_eq!(inner, Rect::new(3, 4, 8, 18));
    }

    #[test]
    fn bottom_is_clamped_to_the_screen() {
        let screen = Screen::new(20, 5);
        let area = Rect::bottom(&screen, 100);
        assert_eq!(area.row, 0, "cannot start above the screen");
        assert_eq!(area.rows, 5);
        assert_eq!(area.cols, 20);
    }

    #[test]
    fn bottom_sits_against_the_last_row() {
        let screen = Screen::new(20, 10);
        let area = Rect::bottom(&screen, 4);
        assert_eq!(area.row, 6);
        assert_eq!(area.row + area.rows, 10, "flush with the bottom");
    }

    // ── Frame ─────────────────────────────────────────────────────────────

    #[test]
    fn a_frame_draws_corners_and_returns_its_inside() {
        let mut screen = Screen::new(10, 4);
        screen.begin_frame();
        let inner = frame(&mut screen, Rect::new(0, 0, 4, 10), Style::default());
        assert_eq!(inner, Rect::new(1, 1, 2, 8));
        let out = grid(&screen);
        for corner in ["╭", "╮", "╰", "╯"] {
            assert!(out.contains(corner), "missing {corner}");
        }
    }

    /// A terminal can be tiny; a dialog that does not fit must not panic.
    #[test]
    fn a_frame_too_small_draws_nothing_and_reports_no_room() {
        for (rows, cols) in [(0usize, 0usize), (1, 1), (1, 10), (10, 1)] {
            let mut screen = Screen::new(cols.max(1), rows.max(1));
            screen.begin_frame();
            let inner = frame(&mut screen, Rect::new(0, 0, rows, cols), Style::default());
            assert!(inner.is_empty(), "{rows}x{cols} should report no room");
        }
    }

    #[test]
    fn a_title_lands_in_the_top_border() {
        let mut screen = Screen::new(20, 4);
        screen.begin_frame();
        let area = Rect::new(0, 0, 4, 20);
        frame(&mut screen, area, Style::default());
        title(&mut screen, area, "Approve", Style::default());
        assert!(grid(&screen).contains("Approve"));
    }

    #[test]
    fn a_title_too_long_for_the_border_is_clipped() {
        let mut screen = Screen::new(12, 3);
        screen.begin_frame();
        let area = Rect::new(0, 0, 3, 12);
        frame(&mut screen, area, Style::default());
        title(&mut screen, area, "a very long title indeed", Style::default());
        // Nothing may spill onto the row below the border.
        let out = grid(&screen);
        assert!(out.contains("…"), "clipped with an ellipsis: {out:?}");
    }

    #[test]
    fn a_title_is_skipped_when_there_is_no_border_room() {
        let mut screen = Screen::new(5, 3);
        screen.begin_frame();
        title(&mut screen, Rect::new(0, 0, 3, 5), "hello", Style::default());
        assert_eq!(rendered(&mut screen).len(), 0, "nothing drawn");
    }

    // ── List ──────────────────────────────────────────────────────────────

    #[test]
    fn the_selected_row_is_marked() {
        let mut screen = Screen::new(30, 3);
        screen.begin_frame();
        let rows = [
            Row { label: "Yes", description: "", selected: true, checked: None },
            Row { label: "No", description: "", selected: false, checked: None },
        ];
        list(&mut screen, Rect::new(0, 0, 3, 30), &rows, 75);
        let out = grid(&screen);
        assert!(out.contains("❯ Yes"), "selected row marked: {out:?}");
    }

    #[test]
    fn multi_select_rows_show_checkboxes() {
        let mut screen = Screen::new(30, 3);
        screen.begin_frame();
        let rows = [
            Row { label: "One", description: "", selected: true, checked: Some(true) },
            Row { label: "Two", description: "", selected: false, checked: Some(false) },
        ];
        list(&mut screen, Rect::new(0, 0, 3, 30), &rows, 75);
        let out = grid(&screen);
        assert!(out.contains("[x] One"));
        assert!(out.contains("[ ] Two"));
    }

    /// More options than rows must not draw outside the area.
    #[test]
    fn a_list_longer_than_its_area_is_truncated() {
        let mut screen = Screen::new(20, 6);
        screen.begin_frame();
        let rows: Vec<Row> = (0..10)
            .map(|_| Row { label: "opt", description: "", selected: false, checked: None })
            .collect();
        let drawn = list(&mut screen, Rect::new(0, 0, 3, 20), &rows, 75);
        assert_eq!(drawn, 3, "only what fits");
    }

    /// The description is the first thing sacrificed, and it must never push
    /// the row past its width.
    #[test]
    fn a_description_is_dropped_when_the_row_is_narrow() {
        let mut screen = Screen::new(12, 2);
        screen.begin_frame();
        let rows = [Row {
            label: "Label",
            description: "a description far too long to fit",
            selected: false,
            checked: None,
        }];
        list(&mut screen, Rect::new(0, 0, 2, 12), &rows, 75);
        let out = grid(&screen);
        assert!(out.contains("Label"), "the label survives");
        assert!(!out.contains("far too long"), "the description does not");
    }

    #[test]
    fn an_empty_area_draws_no_rows() {
        let mut screen = Screen::new(10, 2);
        screen.begin_frame();
        let rows = [Row { label: "x", description: "", selected: true, checked: None }];
        assert_eq!(list(&mut screen, Rect::new(0, 0, 0, 0), &rows, 75), 0);
    }

    // ── Text block ────────────────────────────────────────────────────────

    #[test]
    fn a_text_block_wraps_and_reports_its_height() {
        let mut screen = Screen::new(10, 6);
        screen.begin_frame();
        let used = text_block(
            &mut screen,
            Rect::new(0, 0, 6, 10),
            "one two three four five six",
            Style::default(),
        );
        assert!(used > 1, "wrapped onto several rows");
        assert!(used <= 6);
    }

    #[test]
    fn a_text_block_stops_at_the_bottom_of_its_area() {
        let mut screen = Screen::new(10, 10);
        screen.begin_frame();
        let used = text_block(
            &mut screen,
            Rect::new(0, 0, 2, 10),
            &"word ".repeat(50),
            Style::default(),
        );
        assert_eq!(used, 2, "clipped to the area");
    }

    // ── Gauge ─────────────────────────────────────────────────────────────

    #[test]
    fn a_gauge_fills_proportionally() {
        let mut screen = Screen::new(12, 1);
        screen.begin_frame();
        gauge(&mut screen, 0, 0, 10, 0.5, Style::default());
        let out = grid(&screen);
        assert_eq!(out.matches('█').count(), 5);
        assert_eq!(out.matches('░').count(), 5);
    }

    #[test]
    fn a_gauge_clamps_out_of_range_and_non_finite_values() {
        for (fraction, expect_full) in [(1.5f32, true), (-1.0, false), (f32::NAN, false)] {
            let mut screen = Screen::new(12, 1);
            screen.begin_frame();
            gauge(&mut screen, 0, 0, 10, fraction, Style::default());
            let out = grid(&screen);
            let filled = out.matches('█').count();
            assert_eq!(
                filled, if expect_full { 10 } else { 0 },
                "fraction {fraction} gave {filled} filled cells",
            );
        }
    }

    // ── Clip ──────────────────────────────────────────────────────────────

    #[test]
    fn clip_leaves_short_text_alone() {
        assert_eq!(clip("short", 10), "short");
    }

    #[test]
    fn clipped_text_fits_including_the_ellipsis() {
        let out = clip("this is far too long", 10);
        assert!(str_width(&out) <= 10, "{out:?} is {} cells", str_width(&out));
        assert!(out.ends_with('…'));
    }

    /// Clipping must not split a wide glyph in half.
    #[test]
    fn clip_does_not_split_wide_glyphs() {
        let out = clip("日本語日本語日本語", 7);
        assert!(str_width(&out) <= 7);
        assert!(!out.contains('\u{FFFD}'), "no torn codepoints");
    }

    #[test]
    fn clip_to_zero_is_empty() {
        assert_eq!(clip("anything", 0), "");
    }
}
