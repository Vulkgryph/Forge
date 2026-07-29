// SPDX-License-Identifier: Apache-2.0
//! A double-buffered cell grid that emits the smallest update it can.
//!
//! Two decisions here, both taken because of specific bugs in the previous TUI.
//!
//! **Absolute addressing.** Every write is preceded by a cursor-position escape
//! naming the exact row and column. The old renderer worked relatively — draw N
//! lines, then move the cursor up N to redraw — which requires knowing how many
//! rows the content occupied. Get that wrong by one and the redraw lands inside
//! text it does not own, and messages overlap. Absolute positioning has no such
//! quantity to get wrong.
//!
//! **One synchronized update per frame.** Everything a frame changes is written
//! between `ESC[?2026h` and `ESC[?2026l`, so the terminal presents the finished
//! frame or the previous one, never a half-drawn mixture. Terminals without the
//! mode parse and ignore it.
//!
//! Widths come from [`crate::width`], the same module layout wraps with, so a
//! cluster is never measured one way when placed and another way when wrapped.

use std::fmt::Write as _;
use std::io::Write;

use unicode_segmentation::UnicodeSegmentation;

use crate::width::{cluster_width, str_width};

/// Begin synchronized update: hold the current frame until told otherwise.
pub const SYNC_BEGIN: &str = "\x1b[?2026h";
/// End synchronized update: present.
pub const SYNC_END: &str = "\x1b[?2026l";

/// A 256-colour foreground index, or the terminal default.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg:   Option<u8>,
    pub bold: bool,
    pub dim:  bool,
}

impl Style {
    pub fn fg(idx: u8) -> Self { Self { fg: Some(idx), ..Self::default() } }
    pub fn bold(mut self) -> Self { self.bold = true; self }
    pub fn dim(mut self) -> Self { self.dim = true; self }

    /// The escape sequence that moves the terminal's pen from `from` to `self`.
    ///
    /// Always resets first. Turning attributes *off* individually needs more
    /// codes than starting clean does, and a reset is one sequence.
    fn sgr(&self, from: Option<&Style>) -> String {
        if from == Some(self) {
            return String::new();
        }
        if *self == Style::default() {
            return "\x1b[0m".to_string();
        }
        let mut s = String::from("\x1b[0");
        if self.bold { s.push_str(";1"); }
        if self.dim  { s.push_str(";2"); }
        if let Some(fg) = self.fg { let _ = write!(s, ";38;5;{fg}"); }
        s.push('m');
        s
    }
}

/// One terminal cell.
///
/// `text` is a whole grapheme cluster, so a family emoji lives in a single cell
/// rather than being split across several. The cell after a wide cluster is a
/// [`Cell::continuation`] — it holds no text and is never drawn, it only
/// reserves the space the glyph physically covers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub text:  Box<str>,
    pub style: Style,
}

impl Cell {
    pub fn blank() -> Self {
        Self { text: " ".into(), style: Style::default() }
    }
    /// The reserved second half of a wide cluster.
    pub fn continuation() -> Self {
        Self { text: "".into(), style: Style::default() }
    }
    fn is_continuation(&self) -> bool {
        self.text.is_empty()
    }
}

pub struct Screen {
    cols:  usize,
    rows:  usize,
    /// What the terminal is currently showing.
    front: Vec<Cell>,
    /// What it should show next.
    back:  Vec<Cell>,
    /// Where the cursor should be left, if it should be visible at all.
    cursor: Option<(usize, usize)>,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let n = cols * rows;
        Self {
            cols,
            rows,
            front:  vec![Cell::blank(); n],
            back:   vec![Cell::blank(); n],
            cursor: None,
        }
    }

    pub fn cols(&self) -> usize { self.cols }
    pub fn rows(&self) -> usize { self.rows }

    /// Resize, discarding both buffers.
    ///
    /// The front buffer is invalidated deliberately: after a resize the terminal
    /// has reflowed its own contents in ways we cannot model, so any belief we
    /// held about what is on screen is now wrong. Filling with a sentinel that
    /// cannot equal a real cell forces the next frame to redraw everything.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        let n = cols * rows;
        self.back  = vec![Cell::blank(); n];
        self.front = vec![Cell { text: "\u{0}".into(), style: Style::default() }; n];
        self.cursor = None;
    }

    /// Reset the frame being built.
    pub fn begin_frame(&mut self) {
        for cell in &mut self.back {
            *cell = Cell::blank();
        }
        self.cursor = None;
    }

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        self.cursor = Some((row, col));
    }

    fn idx(&self, row: usize, col: usize) -> Option<usize> {
        (row < self.rows && col < self.cols).then(|| row * self.cols + col)
    }

    /// Draw `text` starting at `(row, col)`, clipped to the row.
    ///
    /// Returns the column just past what was written. Advances by measured
    /// width, so a wide cluster moves two columns and reserves the second.
    pub fn put(&mut self, row: usize, col: usize, text: &str, style: Style) -> usize {
        let mut col = col;
        for cluster in text.graphemes(true) {
            let w = cluster_width(cluster);
            if w == 0 {
                continue; // occupies nothing; drawing it would desync our count
            }
            if col + w > self.cols {
                break; // clipped rather than wrapped: callers wrap deliberately
            }
            let Some(i) = self.idx(row, col) else { break };
            self.back[i] = Cell { text: cluster.into(), style };
            if w == 2 {
                if let Some(j) = self.idx(row, col + 1) {
                    self.back[j] = Cell::continuation();
                }
            }
            col += w;
        }
        col
    }

    /// Fill a row span with a background style — used for selected rows and bars.
    pub fn fill_row(&mut self, row: usize, style: Style) {
        for col in 0..self.cols {
            if let Some(i) = self.idx(row, col) {
                self.back[i] = Cell { text: " ".into(), style };
            }
        }
    }

    /// Diff against what is on screen and write only what changed.
    ///
    /// Groups consecutive changed cells into runs so a changed word costs one
    /// cursor move rather than one per cell.
    pub fn flush(&mut self, out: &mut impl Write) -> std::io::Result<()> {
        let mut body = String::new();
        let mut pen: Option<Style> = None;

        for row in 0..self.rows {
            let mut col = 0;
            while col < self.cols {
                let i = row * self.cols + col;
                if self.back[i] == self.front[i] {
                    col += 1;
                    continue;
                }

                // Start of a changed run. Position once, then emit until the
                // cells agree again.
                let _ = write!(body, "\x1b[{};{}H", row + 1, col + 1);
                while col < self.cols {
                    let i = row * self.cols + col;
                    if self.back[i] == self.front[i] {
                        break;
                    }
                    let cell = self.back[i].clone();
                    if cell.is_continuation() {
                        // Space already claimed by the wide cluster before it.
                        self.front[i] = cell;
                        col += 1;
                        continue;
                    }
                    let sgr = cell.style.sgr(pen.as_ref());
                    if !sgr.is_empty() {
                        body.push_str(&sgr);
                        pen = Some(cell.style);
                    }
                    body.push_str(&cell.text);
                    self.front[i] = cell;
                    col += 1;
                }
            }
        }

        if body.is_empty() && self.cursor.is_none() {
            return Ok(()); // nothing changed; emitting a frame would be noise
        }

        // Leave the pen clean so anything written outside the TUI is unstyled.
        if pen.is_some() {
            body.push_str("\x1b[0m");
        }
        match self.cursor {
            Some((row, col)) => {
                let _ = write!(body, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1);
            }
            None => body.push_str("\x1b[?25l"),
        }

        out.write_all(SYNC_BEGIN.as_bytes())?;
        out.write_all(body.as_bytes())?;
        out.write_all(SYNC_END.as_bytes())?;
        out.flush()
    }

    /// Row contents as a string, for tests and debugging.
    #[cfg(test)]
    fn row_text(&self, row: usize) -> String {
        (0..self.cols)
            .filter_map(|c| self.idx(row, c))
            .map(|i| self.back[i].text.as_ref())
            .collect::<String>()
    }
}

/// Convenience for drawing a wrapped paragraph, using the shared wrap.
pub fn put_wrapped(
    screen: &mut Screen,
    row:    usize,
    text:   &str,
    style:  Style,
) -> usize {
    let cols = screen.cols();
    let mut row = row;
    for line in crate::width::wrap(text, cols) {
        if row >= screen.rows() {
            break;
        }
        debug_assert!(
            str_width(&line) <= cols,
            "wrap produced a line wider than the screen; width and layout disagree",
        );
        screen.put(row, 0, &line, style);
        row += 1;
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test sink that records exactly what the renderer wrote.
    #[derive(Default)]
    struct Sink(Vec<u8>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    impl Sink {
        fn text(&self) -> String { String::from_utf8_lossy(&self.0).into_owned() }
        fn frames(&self) -> usize { self.text().matches(SYNC_BEGIN).count() }
    }

    fn render(screen: &mut Screen) -> Sink {
        let mut sink = Sink::default();
        screen.flush(&mut sink).unwrap();
        sink
    }

    #[test]
    fn a_frame_is_wrapped_in_one_synchronized_update() {
        let mut s = Screen::new(20, 3);
        s.begin_frame();
        s.put(0, 0, "hello", Style::default());
        let out = render(&mut s);

        assert_eq!(out.frames(), 1, "exactly one frame");
        assert!(out.text().starts_with(SYNC_BEGIN));
        assert!(out.text().ends_with(SYNC_END));
    }

    /// The whole point of diffing: an unchanged frame costs nothing. Without
    /// this the renderer would repaint on every tick, which is what made the
    /// old TUI churn.
    #[test]
    fn an_unchanged_frame_emits_nothing() {
        let mut s = Screen::new(20, 3);
        s.begin_frame();
        s.put(0, 0, "steady", Style::default());
        render(&mut s);

        s.begin_frame();
        s.put(0, 0, "steady", Style::default());
        let out = render(&mut s);
        assert_eq!(out.0.len(), 0, "identical frame must produce no bytes");
    }

    #[test]
    fn only_the_changed_run_is_redrawn() {
        let mut s = Screen::new(20, 1);
        s.begin_frame();
        s.put(0, 0, "aaaaaaaaaa", Style::default());
        render(&mut s);

        s.begin_frame();
        s.put(0, 0, "aaaaXXaaaa", Style::default());
        let out = render(&mut s);

        assert!(out.text().contains("XX"), "the change is written");
        assert!(!out.text().contains("aaaa"), "unchanged cells are not rewritten");
        // Positioned at column 5 (1-indexed), where the run starts.
        assert!(out.text().contains("\x1b[1;5H"), "one absolute move to the run");
    }

    /// Absolute positioning is the property that makes overlap impossible, so
    /// assert it directly: every write is preceded by a row;col move.
    #[test]
    fn writes_are_absolutely_positioned() {
        let mut s = Screen::new(20, 4);
        s.begin_frame();
        s.put(2, 3, "here", Style::default());
        let out = render(&mut s);
        assert!(out.text().contains("\x1b[3;4H"), "row 3, col 4, 1-indexed");
        // No relative cursor motion anywhere.
        for seq in ["\x1b[A", "\x1b[B", "\x1b[C", "\x1b[D"] {
            assert!(!out.text().contains(seq), "no relative cursor movement");
        }
    }

    /// Nothing may emit the scrollback erase. On the alternate screen it is
    /// pointless, and on the main screen it destroys the user's history — the
    /// original jitter.
    #[test]
    fn never_erases_scrollback() {
        let mut s = Screen::new(20, 3);
        s.begin_frame();
        s.put(0, 0, "text", Style::default());
        let out = render(&mut s);
        assert!(!out.text().contains("\x1b[3J"), "must never erase scrollback");
        assert!(!out.text().contains("\x1b[2J"), "no full clears either");
    }

    #[test]
    fn a_wide_cluster_occupies_two_cells() {
        let mut s = Screen::new(10, 1);
        s.begin_frame();
        let next = s.put(0, 0, "日", Style::default());
        assert_eq!(next, 2, "cursor advanced two columns");
        assert_eq!(s.row_text(0), "日        ", "second cell reserved, not drawn");
    }

    #[test]
    fn an_emoji_sequence_occupies_one_cell_pair() {
        let mut s = Screen::new(10, 1);
        s.begin_frame();
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let next = s.put(0, 0, family, Style::default());
        assert_eq!(next, 2, "one glyph, two columns — not six");
    }

    #[test]
    fn a_wide_cluster_is_clipped_rather_than_split_at_the_edge() {
        let mut s = Screen::new(3, 1);
        s.begin_frame();
        // Two cells fit, the third would need cols 2..4.
        let next = s.put(0, 0, "日日", Style::default());
        assert_eq!(next, 2, "second wide cluster does not fit and is dropped whole");
    }

    #[test]
    fn styles_are_emitted_once_per_run_not_per_cell() {
        let mut s = Screen::new(20, 1);
        s.begin_frame();
        s.put(0, 0, "colour", Style::fg(42));
        let out = render(&mut s);
        assert_eq!(
            out.text().matches("38;5;42").count(), 1,
            "one SGR for a run of identically styled cells",
        );
    }

    #[test]
    fn a_style_change_alone_redraws_the_cell() {
        let mut s = Screen::new(10, 1);
        s.begin_frame();
        s.put(0, 0, "x", Style::default());
        render(&mut s);

        s.begin_frame();
        s.put(0, 0, "x", Style::fg(9));
        let out = render(&mut s);
        assert!(out.text().contains("38;5;9"), "same text, new style, must redraw");
    }

    #[test]
    fn the_pen_is_reset_before_the_frame_ends() {
        let mut s = Screen::new(10, 1);
        s.begin_frame();
        s.put(0, 0, "x", Style::fg(9).bold());
        let out = render(&mut s);
        let body = out.text();
        let end = body.find(SYNC_END).unwrap();
        assert!(body[..end].ends_with("\x1b[0m") || body[..end].contains("\x1b[0m"),
                "pen reset so later output is unstyled");
    }

    #[test]
    fn resize_forces_a_full_redraw() {
        let mut s = Screen::new(10, 2);
        s.begin_frame();
        s.put(0, 0, "before", Style::default());
        render(&mut s);

        s.resize(12, 3);
        s.begin_frame();
        s.put(0, 0, "before", Style::default());
        let out = render(&mut s);
        assert!(
            out.text().contains("before"),
            "after a resize the terminal reflowed; we cannot trust the old front buffer",
        );
    }

    #[test]
    fn clearing_a_cell_is_a_change() {
        let mut s = Screen::new(10, 1);
        s.begin_frame();
        s.put(0, 0, "gone", Style::default());
        render(&mut s);

        s.begin_frame(); // draw nothing
        let out = render(&mut s);
        assert_eq!(out.frames(), 1, "the erase is a frame");
        assert!(out.text().contains("    "), "cells blanked");
    }

    #[test]
    fn cursor_is_hidden_unless_placed() {
        let mut s = Screen::new(10, 1);
        s.begin_frame();
        s.put(0, 0, "x", Style::default());
        assert!(render(&mut s).text().contains("\x1b[?25l"));

        s.begin_frame();
        s.put(0, 0, "y", Style::default());
        s.set_cursor(0, 1);
        let out = render(&mut s);
        assert!(out.text().contains("\x1b[?25h"), "shown when placed");
        assert!(out.text().contains("\x1b[1;2H"), "and positioned");
    }

    #[test]
    fn zero_width_clusters_do_not_desync_the_column_count() {
        let mut s = Screen::new(10, 1);
        s.begin_frame();
        // A lone combining mark occupies nothing and must not consume a cell.
        let next = s.put(0, 0, "a\u{301}b", Style::default());
        assert_eq!(next, 2, "two visible clusters, two columns");
    }

    #[test]
    fn wrapped_paragraphs_stay_inside_the_screen() {
        let mut s = Screen::new(12, 6);
        s.begin_frame();
        let text = "日本語のテキスト and some ordinary words \u{26A0}\u{FE0F} here";
        put_wrapped(&mut s, 0, text, Style::default());
        // put_wrapped debug_asserts the invariant; this confirms it draws.
        let out = render(&mut s);
        assert_eq!(out.frames(), 1);
    }

    #[test]
    fn drawing_out_of_bounds_is_ignored() {
        let mut s = Screen::new(5, 2);
        s.begin_frame();
        s.put(99, 0, "nope", Style::default());
        s.put(0, 99, "nope", Style::default());
        let out = render(&mut s);
        assert_eq!(out.0.len(), 0, "nothing drawn, nothing emitted");
    }
}
