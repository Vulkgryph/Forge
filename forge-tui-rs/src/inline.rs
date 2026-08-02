// SPDX-License-Identifier: Apache-2.0
//! Inline rendering: the transcript goes into the terminal's own scrollback.
//!
//! This is how the TypeScript client worked and what a terminal chat should be.
//! Finished output is *printed* — it scrolls up into the terminal's history,
//! where it can be scrolled back to with the mouse, selected, and copied, and it
//! survives the program exiting. Only the part still changing is redrawn in
//! place at the bottom.
//!
//! The alternative, which this replaces, was a full-screen viewer on the
//! alternate screen. That has an appealing property — absolute cell addressing
//! cannot go wrong — but it means the terminal's scrollback holds nothing, the
//! transcript is a window you page through with keys, and everything vanishes on
//! exit. For a conversation you want to read back through, that is the wrong
//! trade.
//!
//! **The hazard this brings back, and why it is contained.** Redrawing in place
//! means moving the cursor up over what was drawn and erasing it, so the line
//! count has to be exactly right. Ink got this wrong: it counted logical
//! newlines while the terminal counted display rows, and when a long line
//! wrapped, its cursor-up landed inside text it did not own and messages
//! overlapped. Two things stop that here:
//!
//!  * Lines are wrapped by us, with [`crate::width`], before they are counted.
//!    The number we move up by is the number we printed, measured the same way
//!    the terminal will measure it.
//!  * Autowrap is off, so the terminal cannot silently turn one printed line
//!    into two.
//!
//! And the live region is capped below the window height, so a cursor-up can
//! never reach past the top of the screen into committed history — the failure
//! ink papered over by erasing the whole scrollback with `ESC[3J`, which is the
//! jitter that started all of this. Nothing here emits `3J`.

use std::io::{self, Write};

use crate::markdown::Line;
use crate::screen::{SYNC_BEGIN, SYNC_END};
use crate::width::str_width;

/// Erase from the cursor to the end of the display. Not `ESC[3J`, which also
/// erases the scrollback and would throw away the conversation.
const ERASE_DOWN: &str = "\x1b[0J";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";

pub struct Inline {
    cols: usize,
    rows: usize,
    /// Display rows the live region currently occupies.
    live_rows: usize,
    /// Which row of the block the cursor was left on.
    ///
    /// Not always the last: placing the caret at the prompt leaves it above the
    /// context bar. Erasing has to move up from where the cursor *is*, and
    /// assuming it sat on the final row climbed one row too far on every redraw,
    /// eating a line of committed transcript each time.
    cursor_row: usize,
}

impl Inline {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self { cols, rows, live_rows: 0, cursor_row: 0 }
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Rows the live region may occupy.
    ///
    /// One less than the window, so redrawing can always move up over the whole
    /// block without reaching the top of the screen and erasing committed
    /// history. Anything taller has to be committed instead.
    pub fn live_capacity(&self) -> usize {
        self.rows.saturating_sub(1).max(1)
    }

    /// On resize, what is on screen no longer matches what was printed.
    ///
    /// The live region is forgotten rather than erased: the terminal has
    /// reflowed it in ways we cannot model, so moving up by the old count would
    /// land somewhere arbitrary. Leaving it behind costs a few stale rows once;
    /// guessing could destroy committed output.
    pub fn resized(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.live_rows = 0;
        self.cursor_row = 0;
    }

    /// Print lines permanently. They scroll into the terminal's scrollback and
    /// are never touched again.
    pub fn commit(&mut self, out: &mut impl Write, lines: &[Line]) -> io::Result<()> {
        if lines.is_empty() {
            return Ok(());
        }
        let mut buf = String::new();
        // Clear the live region first, or committed output would be printed
        // underneath it and the prompt would end up above the new text.
        self.erase_live(&mut buf);
        for line in lines {
            encode(line, &mut buf, self.cols);
            buf.push_str("\r\n");
        }
        self.live_rows = 0;
        self.cursor_row = 0;
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Redraw the region at the bottom that is still changing.
    ///
    /// `cursor` is a (row, column) within the block, where the caret belongs.
    pub fn draw_live(
        &mut self,
        out:    &mut impl Write,
        lines:  &[Line],
        cursor: Option<(usize, usize)>,
    ) -> io::Result<()> {
        let mut buf = String::from(SYNC_BEGIN);
        buf.push_str(HIDE_CURSOR);
        self.erase_live(&mut buf);

        for (i, line) in lines.iter().enumerate() {
            encode(line, &mut buf, self.cols);
            // No trailing newline on the last line: it would scroll the screen
            // and leave a blank row below the prompt that grows on every redraw.
            if i + 1 < lines.len() {
                buf.push_str("\r\n");
            }
        }
        self.live_rows = lines.len();
        // Printing ends on the final row; recorded so the next erase knows where
        // to climb from, and corrected below if the caret is moved.
        self.cursor_row = lines.len().saturating_sub(1);

        // Park the caret. Positions are relative to the block, since absolute
        // addressing is meaningless when the block's place on screen moves as
        // the terminal scrolls.
        if let Some((row, col)) = cursor {
            let row = row.min(lines.len().saturating_sub(1));
            let up = lines.len().saturating_sub(1) - row;
            buf.push('\r');
            if up > 0 {
                buf.push_str(&format!("\x1b[{up}A"));
            }
            if col > 0 {
                buf.push_str(&format!("\x1b[{col}C"));
            }
            buf.push_str(SHOW_CURSOR);
            self.cursor_row = row;
        }

        buf.push_str(SYNC_END);
        out.write_all(buf.as_bytes())?;
        out.flush()
    }

    /// Move to the start of the live region and erase it.
    ///
    /// Climbs from where the cursor actually is, which is not necessarily the
    /// block's last row — the caret is parked at the prompt, above the context
    /// bar. Using `live_rows - 1` regardless overshot by however far the caret
    /// had been moved up, and erased that many rows of committed transcript on
    /// every single redraw.
    fn erase_live(&self, buf: &mut String) {
        buf.push('\r');
        if self.live_rows == 0 {
            buf.push_str(ERASE_DOWN);
            return;
        }
        let up = self.cursor_row;
        if up > 0 {
            buf.push_str(&format!("\x1b[{up}A"));
        }
        buf.push_str(ERASE_DOWN);
    }

    /// Leave the cursor below the live region, for exit.
    pub fn finish(&mut self, out: &mut impl Write) -> io::Result<()> {
        let mut buf = String::new();
        buf.push_str(SHOW_CURSOR);
        // Down to the block's last row first, or the shell prompt would be
        // printed over the bottom of what we drew.
        let down = self.live_rows.saturating_sub(1).saturating_sub(self.cursor_row);
        if down > 0 {
            buf.push_str(&format!("\x1b[{down}B"));
        }
        buf.push_str("\r\n");
        self.live_rows = 0;
        self.cursor_row = 0;
        out.write_all(buf.as_bytes())?;
        out.flush()
    }
}

/// Encode one line as styled text, clipped to `cols`.
///
/// Clipped rather than wrapped: the caller has already wrapped to this width,
/// and a line that slipped through longer than the terminal must not be allowed
/// to wrap — that is precisely the miscount that made ink overlap text.
fn encode(line: &Line, buf: &mut String, cols: usize) {
    let mut used = 0;
    for span in &line.spans {
        if used >= cols {
            break;
        }
        let room = cols - used;
        let text = if str_width(&span.text) > room {
            crate::widgets::clip(&span.text, room)
        } else {
            span.text.clone()
        };
        if text.is_empty() {
            continue;
        }
        buf.push_str(&span.style.ansi());
        buf.push_str(&text);
        used += str_width(&text);
    }
    // Reset, so a style cannot leak into the next line or into the shell.
    buf.push_str("\x1b[0m");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::Span;
    use crate::screen::Style;

    fn line(text: &str) -> Line {
        Line { spans: vec![Span { text: text.into(), style: Style::default() }] }
    }

    fn sink() -> Vec<u8> {
        Vec::new()
    }

    fn text(out: &[u8]) -> String {
        String::from_utf8_lossy(out).into_owned()
    }

    /// Committed output must be printed plainly, so it lands in the terminal's
    /// scrollback and can be scrolled back to and copied.
    #[test]
    fn committed_lines_are_printed_with_newlines() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.commit(&mut out, &[line("first"), line("second")]).unwrap();
        let got = text(&out);
        assert!(got.contains("first"), "{got:?}");
        assert!(got.contains("second"));
        assert_eq!(got.matches("\r\n").count(), 2, "one newline per line");
    }

    /// The sequence that destroys scrollback must never be emitted — it is the
    /// original jitter, and here it would throw away the conversation itself.
    #[test]
    fn nothing_ever_erases_the_scrollback() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.commit(&mut out, &[line("history")]).unwrap();
        inline.draw_live(&mut out, &[line("prompt")], Some((0, 2))).unwrap();
        inline.draw_live(&mut out, &[line("prompt"), line("bar")], None).unwrap();
        inline.finish(&mut out).unwrap();
        let got = text(&out);
        assert!(!got.contains("\x1b[3J"), "must not erase scrollback");
        assert!(!got.contains("\x1b[2J"), "nor clear the screen");
    }

    /// The live region is redrawn by moving up exactly as many rows as were
    /// printed. One too many reaches into committed history and erases it.
    #[test]
    fn redrawing_moves_up_exactly_the_rows_it_printed() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();

        inline.draw_live(&mut out, &[line("a"), line("b"), line("c")], None).unwrap();
        out.clear();

        inline.draw_live(&mut out, &[line("x")], None).unwrap();
        let got = text(&out);
        // Three rows were printed, so the cursor moves up two from the last.
        assert!(got.contains("\x1b[2A"), "expected an up-2, got {got:?}");
        assert!(got.contains(ERASE_DOWN), "and an erase-to-end");
    }

    /// Redrawing after the caret was parked above the block's last row must
    /// climb from where the caret *is*, not from the last row.
    ///
    /// This is the bug that let a keypress eat the transcript: the prompt sits
    /// above the context bar, so the caret ends one row up, and erasing as though
    /// it were on the final row overshot by one and erased a committed line —
    /// every redraw, so holding a key walked up through the conversation.
    #[test]
    fn redrawing_climbs_from_the_caret_not_the_last_row() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();

        // Three rows, caret parked on the middle one — as the prompt is, with a
        // context bar beneath it.
        inline
            .draw_live(&mut out, &[line("a"), line("prompt"), line("bar")], Some((1, 2)))
            .unwrap();
        out.clear();

        inline.draw_live(&mut out, &[line("a"), line("prompt"), line("bar")], Some((1, 2)))
            .unwrap();
        let got = text(&out);
        assert!(
            got.contains("\x1b[1A"),
            "must climb one row, from the caret: {got:?}",
        );
        assert!(
            !got.contains("\x1b[2A"),
            "climbing two would erase a committed line: {got:?}",
        );
    }

    /// The same, with the caret on the first row of a tall block.
    #[test]
    fn a_caret_on_the_first_row_climbs_nothing() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline
            .draw_live(&mut out, &[line("prompt"), line("b"), line("c")], Some((0, 0)))
            .unwrap();
        out.clear();

        inline.draw_live(&mut out, &[line("prompt")], None).unwrap();
        let got = text(&out);
        assert!(
            !got.contains("A"),
            "the caret was already at the top; nothing to climb: {got:?}",
        );
    }

    /// Exiting must first come back down past the caret, or the shell's prompt
    /// prints over the bottom of what was drawn.
    #[test]
    fn finishing_moves_below_the_block_first() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline
            .draw_live(&mut out, &[line("a"), line("prompt"), line("bar")], Some((1, 0)))
            .unwrap();
        out.clear();

        inline.finish(&mut out).unwrap();
        let got = text(&out);
        assert!(got.contains("\x1b[1B"), "descends past the context bar: {got:?}");
    }

    #[test]
    fn the_first_live_draw_moves_up_nothing() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("only")], None).unwrap();
        let got = text(&out);
        assert!(!got.contains("A"), "nothing to move up over yet: {got:?}");
    }

    /// A single-row block needs no cursor-up at all; emitting `ESC[0A` is
    /// harmless but emitting `ESC[1A` would climb into committed output.
    #[test]
    fn a_one_row_block_does_not_climb() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("one")], None).unwrap();
        out.clear();
        inline.draw_live(&mut out, &[line("two")], None).unwrap();
        let got = text(&out);
        assert!(!got.contains("\x1b[1A"), "no climb for one row: {got:?}");
    }

    /// Committing while a live region is up must erase it first, or the prompt
    /// is left stranded above the text that follows it.
    #[test]
    fn committing_clears_the_live_region_first() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("prompt"), line("bar")], None).unwrap();
        out.clear();

        inline.commit(&mut out, &[line("settled output")]).unwrap();
        let got = text(&out);
        assert!(got.contains("\x1b[1A"), "moved up over the two-row block: {got:?}");
        assert!(got.contains(ERASE_DOWN));
        assert!(got.find(ERASE_DOWN).unwrap() < got.find("settled output").unwrap(),
                "erased before printing");
    }

    /// After committing, the live count resets — the region is gone, so the next
    /// redraw must not try to move up over it.
    #[test]
    fn committing_resets_the_live_row_count() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("a"), line("b"), line("c")], None).unwrap();
        inline.commit(&mut out, &[line("settled")]).unwrap();
        out.clear();

        inline.draw_live(&mut out, &[line("fresh")], None).unwrap();
        let got = text(&out);
        assert!(!got.contains("A"), "nothing above to climb over: {got:?}");
    }

    /// The last live line must not end in a newline: that scrolls the screen and
    /// leaves a blank row below the prompt, which grows on every redraw.
    #[test]
    fn the_live_region_does_not_end_with_a_newline() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("a"), line("b")], None).unwrap();
        let got = text(&out);
        let body = got.strip_suffix(SYNC_END).unwrap_or(&got);
        assert!(!body.ends_with("\r\n"), "no trailing newline: {body:?}");
        assert_eq!(body.matches("\r\n").count(), 1, "one separator for two lines");
    }

    #[test]
    fn a_live_frame_is_one_synchronized_update() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("x")], None).unwrap();
        let got = text(&out);
        assert_eq!(got.matches(SYNC_BEGIN).count(), 1);
        assert_eq!(got.matches(SYNC_END).count(), 1);
        assert!(got.starts_with(SYNC_BEGIN) && got.ends_with(SYNC_END));
    }

    /// The caret is placed relative to the block, since the block's absolute
    /// position moves as the terminal scrolls.
    #[test]
    fn the_cursor_is_placed_within_the_block() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline
            .draw_live(&mut out, &[line("aaa"), line("prompt here")], Some((1, 7)))
            .unwrap();
        let got = text(&out);
        assert!(got.contains("\x1b[7C"), "moved right to the column: {got:?}");
        assert!(got.contains(SHOW_CURSOR), "and shown");
    }

    #[test]
    fn a_cursor_on_an_earlier_row_moves_up_to_it() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline
            .draw_live(&mut out, &[line("first"), line("second"), line("third")], Some((0, 3)))
            .unwrap();
        let got = text(&out);
        assert!(got.contains("\x1b[2A"), "up two rows from the last: {got:?}");
    }

    /// A cursor row past the block must be clamped, not emitted as a move that
    /// would leave the caret somewhere arbitrary.
    #[test]
    fn an_out_of_range_cursor_row_is_clamped() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("only")], Some((99, 0))).unwrap();
        let got = text(&out);
        assert!(!got.contains("\x1b[99A"), "no absurd climb: {got:?}");
    }

    /// The cap exists so a redraw can never climb past the top of the window
    /// into committed history.
    #[test]
    fn the_live_capacity_leaves_room_above_itself() {
        assert_eq!(Inline::new(80, 24).live_capacity(), 23);
        // Even a degenerate window reports something usable rather than zero,
        // which would make the caller divide by it or draw nothing at all.
        assert_eq!(Inline::new(80, 1).live_capacity(), 1);
        assert_eq!(Inline::new(80, 0).live_capacity(), 1);
    }

    /// After a resize the terminal has reflowed the live region, so moving up by
    /// the old count would land somewhere arbitrary. Forget it instead.
    #[test]
    fn a_resize_forgets_the_live_region_rather_than_guessing() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.draw_live(&mut out, &[line("a"), line("b"), line("c")], None).unwrap();

        inline.resized(60, 20);
        out.clear();
        inline.draw_live(&mut out, &[line("after")], None).unwrap();
        let got = text(&out);
        assert!(
            !got.contains("\x1b[2A"),
            "must not climb over rows the terminal has reflowed: {got:?}",
        );
        assert_eq!(inline.cols(), 60);
        assert_eq!(inline.rows(), 20);
    }

    // ── Encoding ──────────────────────────────────────────────────────────

    #[test]
    fn styles_are_emitted_and_reset_per_line() {
        let mut buf = String::new();
        encode(
            &Line {
                spans: vec![Span { text: "red".into(), style: Style::fg(9) }],
            },
            &mut buf,
            40,
        );
        assert!(buf.contains("38;5;9"), "the colour: {buf:?}");
        assert!(buf.ends_with("\x1b[0m"), "reset so it cannot leak: {buf:?}");
    }

    /// A line longer than the terminal must be clipped, never allowed to wrap —
    /// a wrap turns one printed row into two and the redraw count goes wrong.
    #[test]
    fn an_overlong_line_is_clipped_not_wrapped() {
        let mut buf = String::new();
        encode(&line(&"x".repeat(200)), &mut buf, 20);
        let visible: String = buf.replace("\x1b[0m", "");
        assert!(str_width(&visible) <= 20, "clipped to the width: {}", str_width(&visible));
    }

    /// Wide characters count by display width, not by character count.
    #[test]
    fn clipping_measures_display_width() {
        let mut buf = String::new();
        encode(&line("日本語日本語日本語日本語"), &mut buf, 10);
        let visible: String = buf.replace("\x1b[0m", "");
        assert!(str_width(&visible) <= 10, "got {}", str_width(&visible));
    }

    #[test]
    fn an_empty_line_still_produces_a_row() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.commit(&mut out, &[Line::default(), line("after")]).unwrap();
        let got = text(&out);
        assert_eq!(got.matches("\r\n").count(), 2, "the blank row is kept: {got:?}");
    }

    #[test]
    fn committing_nothing_writes_nothing() {
        let mut inline = Inline::new(40, 10);
        let mut out = sink();
        inline.commit(&mut out, &[]).unwrap();
        assert!(out.is_empty());
    }
}
