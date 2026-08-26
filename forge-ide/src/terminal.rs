use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::path::Path;

// ── Color helpers ─────────────────────────────────────────────────────────────

fn default_fg() -> egui::Color32 { egui::Color32::from_gray(204) }

fn ansi_color(n: u8, bright: bool) -> egui::Color32 {
    let dark = [
        (  0,   0,   0), (194,  54,  33), ( 37, 188,  36), (173, 173,  39),
        ( 73,  46, 225), (211,  56, 211), ( 51, 187, 200), (203, 204, 205),
    ];
    let brt  = [
        (129, 131, 131), (252,  57,  31), ( 49, 231,  34), (234, 236,  35),
        ( 88,  51, 255), (249,  53, 248), ( 20, 240, 240), (233, 235, 235),
    ];
    let (r, g, b) = if bright { brt[n as usize] } else { dark[n as usize] };
    egui::Color32::from_rgb(r, g, b)
}

fn color256(n: u8) -> egui::Color32 {
    match n {
        0..=7   => ansi_color(n,       false),
        8..=15  => ansi_color(n - 8,   true),
        16..=231 => {
            let n = n - 16;
            let b = n % 6; let g = (n / 6) % 6; let r = n / 36;
            let f = |v: u8| if v == 0 { 0u8 } else { 55 + v * 40 };
            egui::Color32::from_rgb(f(r), f(g), f(b))
        }
        _ => { let v = 8 + (n - 232) * 10; egui::Color32::from_rgb(v, v, v) }
    }
}

/// SGR 2 (faint/decreased intensity) — e.g. Claude Code's grey hint text.
/// Halves each channel, the same simple approximation most lightweight
/// terminal emulators use for "dim" (real xterm does something a bit more
/// nuanced, but this is the same class of effect and reads correctly).
fn dim_color(c: egui::Color32) -> egui::Color32 {
    egui::Color32::from_rgb(c.r() / 2, c.g() / 2, c.b() / 2)
}

// ── Cell ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    fg: egui::Color32,
    /// `None` = transparent (the terminal's own background shows through).
    /// Used for SGR 40-49/100-107 (background color) — e.g. Claude Code
    /// highlighting added/removed lines with a green/red background band,
    /// which previously had no effect at all (background SGR codes were
    /// silently skipped, so diffs rendered with no highlight whatsoever).
    bg: Option<egui::Color32>,
}

impl Cell {
    fn blank() -> Self { Self { ch: ' ', fg: default_fg(), bg: None } }
}

/// Nothing printed on this row. A background colour counts as something: a shell
/// that paints an empty highlighted band has drawn it deliberately.
fn row_is_blank(row: &[Cell]) -> bool {
    row.iter().all(|c| c.ch == ' ' && c.bg.is_none())
}

/// Serializable form of a `Cell` — `egui::Color32` itself isn't `Serialize`
/// without enabling egui's own "serde" feature, so this stores plain
/// `[u8; 4]` RGBA arrays instead of pulling that in for one small struct.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct CellSnap {
    ch: char,
    fg: [u8; 4],
    bg: Option<[u8; 4]>,
}

impl From<&Cell> for CellSnap {
    fn from(c: &Cell) -> Self {
        Self { ch: c.ch, fg: c.fg.to_array(), bg: c.bg.map(|b| b.to_array()) }
    }
}

impl CellSnap {
    fn to_cell(&self) -> Cell {
        let [r, g, b, a] = self.fg;
        Cell {
            ch: self.ch,
            fg: egui::Color32::from_rgba_premultiplied(r, g, b, a),
            bg: self.bg.map(|[r, g, b, a]| egui::Color32::from_rgba_premultiplied(r, g, b, a)),
        }
    }
}

/// A snapshot of a `Grid`'s visible viewport, cursor state, and scrollback
/// history. Taken right before "Reload Window" replaces this process and
/// restored into the fresh `Grid` built for the reattached terminal on the
/// other side, so it shows what was actually on screen — and everything you
/// could still scroll back to — instead of sitting blank/history-less
/// until new output arrives.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct GridSnapshot {
    rows: usize,
    cols: usize,
    cur_row: usize,
    cur_col: usize,
    cursor_visible: bool,
    cells: Vec<Vec<CellSnap>>,
    /// Scrollback lines, oldest first — same content and cap as the live
    /// `Grid::scrollback` (see `MAX_SCROLLBACK`).
    scrollback: Vec<Vec<CellSnap>>,
}

// ── Grid ─────────────────────────────────────────────────────────────────────
//
// Modeled as a real terminal is: a fixed-size *viewport* that all cursor
// addressing, erase, and scrolling operate on, plus a separate *scrollback*
// of lines that have scrolled off the top. This distinction matters for
// TUIs like Claude Code (built on ink) that redraw a live status region by
// moving the cursor up N lines and erasing to end-of-screen — that erase is
// relative to the *current visible screen*, not "everything ever printed".
// The previous implementation had no viewport concept at all (one
// ever-growing list of rows, hardcoded to 120 columns regardless of the
// actual panel size or PTY size) — cursor-relative redraws could land on
// the wrong absolute row once the real terminal size didn't match what was
// assumed, leaving stale duplicate content behind after a resize.

const DEFAULT_ROWS:    usize = 24;
const DEFAULT_COLS:    usize = 80;
const MAX_SCROLLBACK:  usize = 5000;

/// How many scrollback lines a persisted `GridSnapshot` keeps. Far smaller than
/// `MAX_SCROLLBACK`: this is only restored for visual continuity on reattach,
/// and it is what `session.json` pays for on every save/load.
const SNAPSHOT_SCROLLBACK: usize = 200;

/// Main-screen state parked while the alternate screen is active.
struct AltSaved {
    viewport: Vec<Vec<Cell>>,
    /// Wrap flags for the parked viewport, so returning to the main screen
    /// restores which of its lines were continuations.
    wrapped:  Vec<bool>,
    cur_row:  usize,
    cur_col:  usize,
}

/// How long a synchronized update may withhold presentation before we give up
/// and draw anyway. An application that sets `?2026h` and then dies — or simply
/// forgets the matching `l` — must not freeze the terminal; real emulators use a
/// comparable bail-out.
const SYNC_UPDATE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(150);

#[derive(PartialEq, Clone, Copy)]
enum PState { Ground, Esc, Csi, Osc, Charset }

pub struct Grid {
    /// Lines scrolled off the top of the viewport, oldest first. Never
    /// touched by cursor movement/erase — only by scrolling and resize.
    scrollback:   VecDeque<Vec<Cell>>,
    /// Whether each scrollback row is the continuation of the line above it,
    /// i.e. produced by autowrap rather than a newline.
    ///
    /// This is what makes a resize able to *reflow* rather than truncate. Without
    /// it a narrower window simply cut every line's tail off, destroying text
    /// that had already been printed, and a wider one left the old ragged breaks
    /// in place. Kept parallel to the rows rather than embedded in them, which
    /// keeps the change small; the two are moved together at every point that
    /// alters row structure, and `debug_assert`s below catch a drift.
    sb_wrapped:   VecDeque<bool>,
    /// The current fixed-size screen; what cursor addressing/erase target.
    viewport:     Vec<Vec<Cell>>,
    /// Wrap flags for the viewport, as [`Grid::sb_wrapped`] is for scrollback.
    wrapped:      Vec<bool>,
    rows:         usize,
    cols:         usize,
    cur_row:      usize, // viewport-relative (0..rows)
    cur_col:      usize, // viewport-relative (0..cols)
    /// DECTCEM (`ESC[?25h`/`l`) state. TUI frameworks like ink (which
    /// Claude Code is built on) hide the real cursor and draw their own
    /// in-content indicator, showing the real one only at an actual
    /// text-input point — previously unhandled entirely, so our cursor
    /// stayed keyed only to focus/blink and ignored the app's own intent.
    cursor_visible: bool,
    current_fg:   egui::Color32,
    /// `None` = transparent/default background (most text). SGR 40-49 and
    /// 100-107 — e.g. Claude Code's red/green highlight bands for
    /// removed/added lines — previously fell into a silent no-op catch-all,
    /// so diffs rendered with no highlight at all.
    current_bg:   Option<egui::Color32>,
    /// SGR 2 (faint/decreased intensity) — e.g. Claude Code's hint text.
    /// Tracked separately from `current_fg` rather than baked directly into
    /// it, since dim needs to correctly combine with *whatever* color is
    /// active (including ones set after the dim attribute), not just
    /// whatever the color happened to be at the moment dim was turned on.
    /// Previously this SGR code hit a catch-all no-op, so dimmed text
    /// rendered at full brightness — indistinguishable from normal text.
    dim:          bool,
    parse_st:     PState,
    parse_params: String,
    /// Main-screen contents saved while the alternate screen (`ESC[?1049h`) is
    /// active. `Some` means we are on the alternate screen.
    ///
    /// Full-screen programs — vim, htop, less, and any TUI that takes over the
    /// display — switch to this buffer precisely so their repaints do not end up
    /// in the scrollback. Without it their frames scrolled into history as if
    /// they were output, so leaving one left thousands of junk lines behind.
    alt_screen:   Option<AltSaved>,
    /// Set between `ESC[?2026h` and `ESC[?2026l` — synchronized update.
    ///
    /// The contract is "do not present anything until I say I am done", which
    /// lets an application emit a whole frame without the user seeing it drawn
    /// piecewise. Implemented by withholding `version` bumps: the renderer keys
    /// its cached galley on `version`, so an unchanged version means it keeps
    /// presenting the last *complete* frame while this one is assembled.
    sync_update:  Option<std::time::Instant>,
    /// Autowrap (DECAWM, `ESC[?7h`/`l`). On by default, as a real terminal is.
    ///
    /// Full-screen programs turn it off because they position every cell
    /// themselves and cannot have the terminal moving their output: with it on,
    /// writing the bottom-right cell wraps, which line-feeds, which at the
    /// bottom of the screen scrolls everything up a row. Ignoring the request
    /// meant any such program — vim, htop, our own TUI — could have the frame it
    /// had just drawn shifted out from under it while writing the next one,
    /// leaving two frames on screen at once.
    autowrap:     bool,
    /// Whether the program has asked for pasted text to be bracketed
    /// (`CSI ?2004h`).
    ///
    /// Unhonoured, a paste is indistinguishable from typing: every newline in it
    /// arrives as Enter. Pasting a multi-line message into a program that submits
    /// on Enter therefore sent it a line at a time — observed with the Rust TUI,
    /// which asks for this mode precisely so that cannot happen.
    bracketed_paste: bool,
    /// Bumped on every content-affecting mutation (`process`, `resize`,
    /// `restore_snapshot`). Lets `Terminal::draw_sized` cache its rendered
    /// viewport `Galley` and skip rebuilding + re-shaping it on every single
    /// frame when nothing changed — see `scrollback_version` below for why
    /// this alone wasn't enough once a process is actively producing output.
    version:      u64,
    /// Bumped only when `scrollback` itself actually changes (a line
    /// scrolls off the viewport, a resize reflows it, a snapshot restores
    /// it) — deliberately *not* on every `process()` call like `version` is,
    /// since scrollback is history that, once written, never changes again.
    /// `version` bumping on every byte processed made sense for idle vs.
    /// active detection, but it meant that *while a process is actively
    /// producing output* (a build running, or Claude Code's own UI
    /// redrawing many times a second) the cached viewport Galley was
    /// invalidated constantly — and rebuilding included re-shaping the
    /// *entire* scrollback (thousands of lines on a long session) every
    /// single time, even though none of that history had changed. Caching
    /// the scrollback Galley separately, keyed on this instead, means an
    /// active frame only re-shapes the ~50 rows actually changing.
    scrollback_version: u64,
}

impl Grid {
    pub fn new() -> Self {
        Self::with_size(DEFAULT_ROWS, DEFAULT_COLS)
    }

    pub fn with_size(rows: usize, cols: usize) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        Self {
            scrollback: VecDeque::new(),
            sb_wrapped: VecDeque::new(),
            viewport:   vec![vec![Cell::blank(); cols]; rows],
            wrapped:    vec![false; rows],
            rows, cols, cur_row: 0, cur_col: 0,
            cursor_visible: true,
            current_fg: default_fg(),
            current_bg: None,
            dim: false,
            parse_st: PState::Ground, parse_params: String::new(),
            alt_screen: None, sync_update: None,
            autowrap: true,
            bracketed_paste: false,
            version: 0,
            scrollback_version: 0,
        }
    }

    /// Cheap change signal for `Terminal::draw_sized`'s viewport Galley
    /// cache — see the field doc comment on `version`.
    pub fn version(&self) -> u64 { self.version }

    /// Cheap change signal for the *scrollback* Galley cache — see the
    /// field doc comment on `scrollback_version`.
    pub fn scrollback_version(&self) -> u64 { self.scrollback_version }

    /// Whether the app wants its cursor shown (`ESC[?25h`, the default) or
    /// hidden (`ESC[?25l`) — independent of whether this terminal panel
    /// currently has keyboard focus.
    pub fn cursor_visible(&self) -> bool { self.cursor_visible }

    pub fn snapshot(&self) -> GridSnapshot {
        // Only the tail of the scrollback is persisted. Snapshots exist purely
        // so a reattached terminal shows recent context instead of sitting
        // blank, and every cell serializes as a JSON object — a full
        // `MAX_SCROLLBACK` history produced a 132 MB `session.json` for a
        // single terminal, which then had to be written and re-parsed
        // synchronously on the event-loop thread at every save and restore.
        let keep = self.scrollback.len().saturating_sub(SNAPSHOT_SCROLLBACK);
        GridSnapshot {
            rows: self.rows,
            cols: self.cols,
            cur_row: self.cur_row,
            cur_col: self.cur_col,
            cursor_visible: self.cursor_visible,
            cells: self.viewport.iter()
                .map(|row| row.iter().map(CellSnap::from).collect())
                .collect(),
            scrollback: self.scrollback.iter().skip(keep)
                .map(|row| row.iter().map(CellSnap::from).collect())
                .collect(),
        }
    }

    /// Applies a snapshot taken from a (possibly differently-sized) prior
    /// `Grid` onto this freshly-created one — viewport cells within
    /// whatever overlap exists (a size mismatch, i.e. the window resized
    /// since the snapshot, just means the excess is left blank / the
    /// overflow is dropped, rather than refusing to restore at all), plus
    /// the full scrollback so you can still scroll back into pre-reload
    /// history instead of it just vanishing.
    pub fn restore_snapshot(&mut self, snap: &GridSnapshot) {
        self.version += 1;
        self.scrollback_version += 1;
        for (r, row) in snap.cells.iter().enumerate() {
            if r >= self.viewport.len() { break; }
            for (c, cell) in row.iter().enumerate() {
                if c >= self.viewport[r].len() { break; }
                self.viewport[r][c] = cell.to_cell();
            }
        }
        self.cur_row = snap.cur_row.min(self.rows.saturating_sub(1));
        self.cur_col = snap.cur_col.min(self.cols.saturating_sub(1));
        self.cursor_visible = snap.cursor_visible;
        self.scrollback = snap.scrollback.iter()
            .map(|row| row.iter().map(CellSnap::to_cell).collect())
            .collect();
        self.trim_scrollback();
    }

    /// Resize the viewport to match the actual panel size. Real terminals
    /// don't reflow text on resize — rows are padded/truncated in place,
    /// and the newly (un)available height pulls lines back from / pushes
    /// lines into scrollback, the same way a real terminal emulator does.
    pub fn resize(&mut self, new_rows: usize, new_cols: usize) {
        let new_rows = new_rows.max(1);
        let new_cols = new_cols.max(1);
        if new_rows == self.rows && new_cols == self.cols { return; }
        self.version += 1;
        self.scrollback_version += 1;

        if new_cols != self.cols {
            self.reflow(new_cols, new_rows);
            self.cols = new_cols;
            self.cur_col = self.cur_col.min(self.cols - 1);
        }

        if new_rows > self.rows {
            let grow = new_rows - self.rows;
            // Pull real history back from scrollback where it exists (a
            // real terminal reveals more of what was already scrolled off
            // when you make it taller). Once scrollback runs out — the
            // common case for a terminal that just started — there's
            // nothing above to reveal, so any remaining growth adds blank
            // rows at the *bottom* instead. Doing it at the top unconditionally
            // shoved fresh content down the screen, leaving a large empty
            // gap above it instead of anchoring it at the top like a real
            // terminal does.
            let mut pulled = 0;
            for _ in 0..grow {
                let Some(row) = self.scrollback.pop_back() else { break };
                let flag = self.sb_wrapped.pop_back().unwrap_or(false);
                self.viewport.insert(0, row);
                self.wrapped.insert(0, flag);
                pulled += 1;
            }
            self.cur_row += pulled;
            for _ in 0..(grow - pulled) {
                self.viewport.push(vec![Cell::blank(); self.cols]);
                self.wrapped.push(false);
            }
        } else if new_rows < self.rows {
            let mut shrink = self.rows - new_rows;

            // Give up the empty space below the cursor first. A terminal made
            // shorter loses the blank rows under the prompt, not the prompt —
            // and this used to scroll the top away unconditionally, which threw
            // whatever had been printed into scrollback and left a viewport of
            // nothing. Most visible on a freshly opened remote terminal: its
            // grid starts taller than the panel, the shell prints its prompt at
            // the top, then the fit-to-panel resize pushed that prompt out of
            // sight and the terminal opened apparently blank, scrolled to the
            // bottom of its own empty space.
            while shrink > 0 && self.viewport.len() > self.cur_row + 1 {
                let last = self.viewport.len() - 1;
                if !row_is_blank(&self.viewport[last]) { break; }
                self.viewport.pop();
                self.wrapped.pop();
                shrink -= 1;
            }

            // Only then take from the top, which is what keeps the cursor in
            // view when there is real content in the way.
            let scrolled = shrink;
            for _ in 0..shrink {
                if self.viewport.is_empty() { break; }
                let row = self.viewport.remove(0);
                let flag = if self.wrapped.is_empty() { false } else { self.wrapped.remove(0) };
                self.scrollback.push_back(row);
                self.sb_wrapped.push_back(flag);
            }
            self.cur_row = self.cur_row.saturating_sub(scrolled);
        }
        self.rows = new_rows;
        self.cur_row = self.cur_row.min(self.rows - 1);
        self.trim_scrollback();
    }

    fn trim_scrollback(&mut self) {
        let before = self.scrollback.len();
        while self.scrollback.len() > MAX_SCROLLBACK {
            self.scrollback.pop_front();
            self.sb_wrapped.pop_front();
        }
        if self.scrollback.len() != before { self.scrollback_version += 1; }
    }

    /// Re-break every line for a new width.
    ///
    /// This used to resize each row in place, which truncated text on the way
    /// narrower and left the old breaks behind on the way wider — a terminal
    /// destroying output that had already been printed. Reflowing is what a
    /// terminal is supposed to do, and owning the emulator is what makes it
    /// possible: joined-then-resplit lines need to know which rows were
    /// continuations, which is what the wrap flags record.
    ///
    /// The cursor is clamped rather than tracked through the reflow. Applications
    /// redraw on `SIGWINCH`, so its exact column afterwards matters far less than
    /// the text surviving — and guessing at it could place it inside a line it
    /// does not belong to.
    fn reflow(&mut self, new_cols: usize, new_rows: usize) {
        debug_assert_eq!(self.viewport.len(), self.wrapped.len(), "viewport flags drifted");
        debug_assert_eq!(self.scrollback.len(), self.sb_wrapped.len(), "scrollback flags drifted");

        // Every row in order, oldest first, with its continuation flag.
        let mut rows: Vec<(Vec<Cell>, bool)> = Vec::with_capacity(
            self.scrollback.len() + self.viewport.len(),
        );
        rows.extend(self.scrollback.drain(..).zip(self.sb_wrapped.drain(..)));
        rows.extend(
            std::mem::take(&mut self.viewport)
                .into_iter()
                .zip(std::mem::take(&mut self.wrapped)),
        );

        // Join continuations back into logical lines.
        let mut logical: Vec<Vec<Cell>> = Vec::new();
        for (cells, is_continuation) in rows {
            match logical.last_mut() {
                Some(last) if is_continuation => last.extend(cells),
                _ => logical.push(cells),
            }
        }

        // Trailing blanks are padding to the old width, not content, and keeping
        // them would leave every reflowed line stretched to the previous size.
        for line in &mut logical {
            while line.last().is_some_and(|c| c.ch == ' ' && c.bg.is_none()) {
                line.pop();
            }
        }

        // Re-break at the new width.
        let mut fresh: Vec<(Vec<Cell>, bool)> = Vec::new();
        for line in logical {
            if line.is_empty() {
                fresh.push((vec![Cell::blank(); new_cols], false));
                continue;
            }
            for (i, chunk) in line.chunks(new_cols).enumerate() {
                let mut cells = chunk.to_vec();
                cells.resize(new_cols, Cell::blank());
                fresh.push((cells, i > 0));
            }
        }

        // The last screenful becomes the viewport; the rest is history. An empty
        // grid still needs a full viewport, so it is padded.
        let keep = new_rows.min(fresh.len());
        let split = fresh.len() - keep;
        for (cells, flag) in fresh.drain(..split) {
            self.scrollback.push_back(cells);
            self.sb_wrapped.push_back(flag);
        }
        for (cells, flag) in fresh {
            self.viewport.push(cells);
            self.wrapped.push(flag);
        }
        while self.viewport.len() < new_rows {
            self.viewport.push(vec![Cell::blank(); new_cols]);
            self.wrapped.push(false);
        }

        self.rows = self.viewport.len();
        self.cur_row = self.cur_row.min(self.rows.saturating_sub(1));
        self.trim_scrollback();
    }

    /// Switch to the alternate screen: park the main viewport, hand the
    /// application a cleared one. `save_cursor` distinguishes `1049` (which also
    /// saves the cursor) from the older `47`/`1047`.
    ///
    /// Re-entering while already on the alternate screen is a no-op rather than
    /// stacking saves — otherwise a program that sets the mode twice would lose
    /// the real main screen behind an alternate one.
    fn enter_alt_screen(&mut self, save_cursor: bool) {
        if self.alt_screen.is_some() { return; }
        self.alt_screen = Some(AltSaved {
            viewport: std::mem::replace(
                &mut self.viewport,
                vec![vec![Cell::blank(); self.cols]; self.rows],
            ),
            wrapped: std::mem::replace(&mut self.wrapped, vec![false; self.rows]),
            cur_row: if save_cursor { self.cur_row } else { 0 },
            cur_col: if save_cursor { self.cur_col } else { 0 },
        });
        self.cur_row = 0;
        self.cur_col = 0;
        self.version += 1;
    }

    /// Return to the main screen, restoring what was parked. Anything the
    /// application drew on the alternate screen is discarded, which is the whole
    /// point: it never belonged in the scrollback.
    fn leave_alt_screen(&mut self, restore_cursor: bool) {
        let Some(saved) = self.alt_screen.take() else { return };
        self.viewport = saved.viewport;
        self.wrapped = saved.wrapped;
        // A resize while on the alternate screen can leave the parked copy the
        // wrong shape; normalise rather than trust it.
        self.viewport.truncate(self.rows);
        self.wrapped.truncate(self.rows);
        while self.viewport.len() < self.rows {
            self.viewport.push(vec![Cell::blank(); self.cols]);
            self.wrapped.push(false);
        }
        for row in &mut self.viewport {
            row.truncate(self.cols);
            while row.len() < self.cols { row.push(Cell::blank()); }
        }
        if restore_cursor {
            self.cur_row = saved.cur_row.min(self.rows.saturating_sub(1));
            self.cur_col = saved.cur_col.min(self.cols.saturating_sub(1));
        }
        self.version += 1;
    }

    /// Shift the viewport up by one line, pushing the departing top line
    /// into scrollback. Does not move the cursor (matches real terminals'
    /// "scroll" primitive, used both by line-feed-at-bottom and ESC[S).
    ///
    /// On the alternate screen the departing line is dropped instead: that
    /// buffer has no scrollback, which is why full-screen programs use it.
    fn scroll_up_one(&mut self) {
        let top = self.viewport.remove(0);
        let top_wrapped = if self.wrapped.is_empty() { false } else { self.wrapped.remove(0) };
        if self.alt_screen.is_none() {
            self.scrollback.push_back(top);
            self.sb_wrapped.push_back(top_wrapped);
            self.scrollback_version += 1;
            self.trim_scrollback();
        }
        self.viewport.push(vec![Cell::blank(); self.cols]);
        // The fresh row is a new line, not a continuation — `put_char` marks it
        // otherwise when autowrap is what brought us here.
        self.wrapped.push(false);
    }

    fn line_feed(&mut self) {
        if self.cur_row + 1 >= self.rows {
            self.scroll_up_one();
        } else {
            self.cur_row += 1;
        }
    }

    fn put_char(&mut self, c: char) {
        if self.cur_col >= self.cols {
            if self.autowrap {
                self.cur_col = 0;
                self.line_feed();
                // The row the cursor has landed on continues the one above. This
                // is the only place a continuation is created, and recording it
                // here is what lets a resize put the line back together.
                if let Some(flag) = self.wrapped.get_mut(self.cur_row) {
                    *flag = true;
                }
            } else {
                // DECAWM off: the cursor stays put at the last column and
                // further characters overwrite it. Crucially it does not
                // line-feed, so nothing scrolls.
                self.cur_col = self.cols.saturating_sub(1);
            }
        }
        let fg = if self.dim { dim_color(self.current_fg) } else { self.current_fg };
        self.viewport[self.cur_row][self.cur_col] = Cell { ch: c, fg, bg: self.current_bg };
        self.cur_col += 1;
    }

    fn erase_line(&mut self, mode: usize) {
        match mode {
            0 => { for i in self.cur_col..self.cols { self.viewport[self.cur_row][i] = Cell::blank(); } }
            1 => { for i in 0..=self.cur_col.min(self.cols - 1) { self.viewport[self.cur_row][i] = Cell::blank(); } }
            2 => { self.viewport[self.cur_row] = vec![Cell::blank(); self.cols]; }
            _ => {}
        }
    }

    /// Erase-in-display, relative to the *viewport* — this is the crucial
    /// piece for correct in-place redraws: an ink-style TUI clearing
    /// "cursor to end of screen" mid-redraw must only ever clear the
    /// current visible rows, never touch scrollback, and (matching real
    /// terminal behavior) never move the cursor itself.
    fn erase_display(&mut self, mode: usize) {
        match mode {
            0 => {
                self.erase_line(0);
                for r in (self.cur_row + 1)..self.rows {
                    self.viewport[r] = vec![Cell::blank(); self.cols];
                }
            }
            1 => {
                self.erase_line(1);
                for r in 0..self.cur_row {
                    self.viewport[r] = vec![Cell::blank(); self.cols];
                }
            }
            2 | 3 => {
                for r in self.viewport.iter_mut() { *r = vec![Cell::blank(); self.cols]; }
                if mode == 3 {
                    self.scrollback.clear();
                    self.sb_wrapped.clear();
                }
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &str) {
        if params.is_empty() || params == "0" {
            self.current_fg = default_fg();
            self.current_bg = None;
            self.dim = false;
            return;
        }
        let ns: Vec<u8> = params.split(';').filter_map(|s| s.parse().ok()).collect();
        let mut i = 0;
        while i < ns.len() {
            match ns[i] {
                0       => { self.current_fg = default_fg(); self.current_bg = None; self.dim = false; }
                1       => {} // bold — could brighten, skip for now
                2       => self.dim = true,
                3..=21  => {} // italic, underline, etc — skip
                22      => self.dim = false, // normal intensity — resets bold(1) and dim(2)
                23..=29 => {} // remaining text-decoration attributes — skip
                30..=37 => self.current_fg = ansi_color(ns[i] - 30, false),
                38 => {
                    if ns.get(i+1) == Some(&2) && i+4 < ns.len() {
                        self.current_fg = egui::Color32::from_rgb(ns[i+2], ns[i+3], ns[i+4]);
                        i += 4;
                    } else if ns.get(i+1) == Some(&5) && i+2 < ns.len() {
                        self.current_fg = color256(ns[i+2]);
                        i += 2;
                    }
                }
                39      => self.current_fg = default_fg(),
                // Background color — e.g. Claude Code's red/green
                // highlight bands for removed/added lines. Previously fell
                // into a single silent no-op catch-all covering 40..=49, so
                // diffs rendered with no highlight at all.
                40..=47 => self.current_bg = Some(ansi_color(ns[i] - 40, false)),
                48 => {
                    if ns.get(i+1) == Some(&2) && i+4 < ns.len() {
                        self.current_bg = Some(egui::Color32::from_rgb(ns[i+2], ns[i+3], ns[i+4]));
                        i += 4;
                    } else if ns.get(i+1) == Some(&5) && i+2 < ns.len() {
                        self.current_bg = Some(color256(ns[i+2]));
                        i += 2;
                    }
                }
                49      => self.current_bg = None,
                90..=97 => self.current_fg = ansi_color(ns[i] - 90, true),
                100..=107 => self.current_bg = Some(ansi_color(ns[i] - 100, true)),
                _       => {}
            }
            i += 1;
        }
    }

    fn dispatch_csi(&mut self, cmd: char, params: &str) {
        // DEC private modes (`ESC [ ? <n> h`/`l`) — TUI frameworks like ink
        // (which Claude Code is built on) hide the real terminal cursor and
        // draw their own in-content indicator, showing the real one only
        // at an actual text-input point. `?25` is cursor visibility
        // (DECTCEM). Params here look like "?25", which plain `.parse()`
        // as done below for ordinary CSI params would just silently fail
        // and drop — this was previously unhandled entirely, so our
        // cursor stayed keyed only to focus/blink and never respected the
        // app explicitly hiding or showing it.
        if let Some(rest) = params.strip_prefix('?') {
            // Private modes are often batched together in one sequence
            // (e.g. `?1004;25h` — bracketed paste + cursor visibility at
            // once), so this has to check membership across all
            // semicolon-separated values, not require an exact "25" match.
            let set = cmd == 'h';
            for mode in rest.split(';') {
                match mode {
                    "25" => self.cursor_visible = set,
                    // Alternate screen. 1049 also saves/restores the cursor;
                    // 47 and 1047 are the older variants without that.
                    "1049" | "1047" | "47" => {
                        if set { self.enter_alt_screen(mode == "1049"); }
                        else   { self.leave_alt_screen(mode == "1049"); }
                    }
                    // Autowrap — see `autowrap`.
                    "7" => self.autowrap = set,
                    // Bracketed paste — see `paste`.
                    "2004" => self.bracketed_paste = set,
                    // Synchronized update — see `sync_update`.
                    "2026" => {
                        if set {
                            self.sync_update = Some(std::time::Instant::now());
                        } else {
                            // Cleared here; the single atomic bump for the whole
                            // frame happens at the end of `process`.
                            self.sync_update = None;
                        }
                    }
                    _ => {}
                }
            }
            return;
        }
        let ns: Vec<usize> = params.split(';').filter_map(|s| s.parse().ok()).collect();
        let p1  = ns.first().copied().unwrap_or(1).max(1);
        let p1z = ns.first().copied().unwrap_or(0);
        match cmd {
            'A' => { self.cur_row = self.cur_row.saturating_sub(p1); }
            'B' => { self.cur_row = (self.cur_row + p1).min(self.rows - 1); }
            'C' => { self.cur_col = (self.cur_col + p1).min(self.cols - 1); }
            'D' => { self.cur_col = self.cur_col.saturating_sub(p1); }
            'G' => { self.cur_col = p1.saturating_sub(1).min(self.cols - 1); }
            'H' | 'f' => {
                let row = ns.first().copied().unwrap_or(1).saturating_sub(1);
                let col = ns.get(1).copied().unwrap_or(1).saturating_sub(1);
                self.cur_row = row.min(self.rows - 1);
                self.cur_col = col.min(self.cols - 1);
            }
            'J' => self.erase_display(p1z),
            'K' => self.erase_line(p1z),
            'P' => {
                let row = &mut self.viewport[self.cur_row];
                let end = (self.cur_col + p1).min(self.cols);
                row.drain(self.cur_col..end);
                while row.len() < self.cols { row.push(Cell::blank()); }
            }
            'S' => { for _ in 0..p1 { self.scroll_up_one(); } }
            'm' => self.apply_sgr(params),
            _ => {}
        }
    }

    pub fn process(&mut self, data: &str) {
        for c in data.chars() {
            match self.parse_st {
                PState::Ground => match c {
                    '\x1b' => self.parse_st = PState::Esc,
                    '\r'   => { self.cur_col = 0; }
                    '\n'   => { self.line_feed(); }
                    '\x08' => { if self.cur_col > 0 { self.cur_col -= 1; } }
                    '\x09' => { self.cur_col = (((self.cur_col / 8) + 1) * 8).min(self.cols - 1); }
                    '\x07' => {}
                    c if (c as u32) >= 0x20 => { self.put_char(c); }
                    _ => {}
                },
                PState::Esc => match c {
                    '[' => { self.parse_params.clear(); self.parse_st = PState::Csi; }
                    ']' => { self.parse_params.clear(); self.parse_st = PState::Osc; }
                    'M' => { self.cur_row = self.cur_row.saturating_sub(1); self.parse_st = PState::Ground; }
                    // Designate G0/G1 character set (`ESC ( B` = ASCII, `ESC
                    // ( 0` = DEC line-drawing, etc.) — a real 3-byte
                    // sequence with one more byte still to come. Previously
                    // fell into the catch-all below, which reset straight
                    // to Ground without consuming that final byte — so it
                    // printed literally instead of being absorbed as part
                    // of the sequence. `ESC ( B` is exactly what a Claude
                    // Code/Ink-based TUI sends after using a special glyph
                    // (its spinner) to switch back to plain ASCII, so a
                    // stray "B" would appear right at the cursor the next
                    // time that happened — which reads as "randomly
                    // replaces whatever I just typed" since it lands
                    // wherever the cursor already was.
                    '(' | ')' => { self.parse_st = PState::Charset; }
                    _   => { self.parse_st = PState::Ground; }
                },
                // The one designator byte after `ESC (`/`ESC )` — Forge IDE
                // doesn't track alternate character sets, so it's simply
                // consumed and discarded rather than printed.
                PState::Charset => { self.parse_st = PState::Ground; }
                PState::Csi => {
                    if c.is_ascii_alphabetic() || c == '~' {
                        let ps = std::mem::take(&mut self.parse_params);
                        self.dispatch_csi(c, &ps);
                        self.parse_st = PState::Ground;
                    } else if c == '\x1b' {
                        self.parse_params.clear(); self.parse_st = PState::Esc;
                    } else {
                        self.parse_params.push(c);
                    }
                }
                PState::Osc => match c {
                    '\x07' | '\x1b' => { self.parse_st = PState::Ground; }
                    _ => {}
                },
            }
        }

        // Present at most once per chunk, and only when no synchronized update is
        // open — the renderer keys its cached galley on `version`, so leaving it
        // unchanged keeps the last *complete* frame on screen while this one is
        // still being written. Decided here, after parsing, so the chunk that
        // *opens* the update doesn't present, and the one that closes it presents
        // exactly once.
        //
        // The deadline is a safety valve: an application that opens a
        // synchronized update and then dies must not freeze the display.
        match self.sync_update {
            Some(started) if started.elapsed() < SYNC_UPDATE_TIMEOUT => {}
            Some(_) => { self.sync_update = None; self.version += 1; }
            None    => self.version += 1,
        }
    }

    /// Cursor position within the *viewport* — the cursor is always in the
    /// viewport, never in scrollback history, so this indexes directly into
    /// `to_viewport_layout_job`'s own rows (no scrollback offset needed).
    pub fn cursor(&self) -> (usize, usize) { (self.cur_row, self.cur_col) }

    /// Full contents (scrollback + current viewport) as plain text, one
    /// trimmed line per row — used by the "Copy" context-menu action.
    /// One viewport row as text, trailing blanks trimmed.
    ///
    /// Exists for tests that need to see how a line was broken, which `all_text`
    /// flattens away.
    #[cfg(test)]
    fn row_text(&self, row: usize) -> String {
        self.viewport
            .get(row)
            .map(|cells| cells.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .unwrap_or_default()
    }

    pub fn all_text(&self) -> String {
        self.scrollback.iter().chain(self.viewport.iter())
            .map(|r| r.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>().join("\n")
    }

    /// Build a colored LayoutJob for rendering in egui from an arbitrary
    /// set of rows — shared by `to_scrollback_layout_job`/
    /// `to_viewport_layout_job`, which are cached *separately* (see
    /// `scrollback_version`) rather than as one combined job, since
    /// scrollback is immutable history that's expensive to keep re-shaping
    /// on every viewport update while a process is actively producing output.
    fn rows_to_layout_job(rows: &[&Vec<Cell>], font_id: egui::FontId) -> egui::text::LayoutJob {
        let dfg = default_fg();
        let fmt = |color: egui::Color32, bg: Option<egui::Color32>| egui::text::TextFormat {
            font_id: font_id.clone(),
            color,
            background: bg.unwrap_or(egui::Color32::TRANSPARENT),
            ..Default::default()
        };

        let mut job = egui::text::LayoutJob {
            break_on_newline: true,
            ..Default::default()
        };

        let row_count = rows.len();
        for (ri, row) in rows.iter().enumerate() {
            // Trim trailing blank cells for performance — but a highlighted
            // (non-default background) trailing run isn't actually blank
            // even where it's just spaces, e.g. Claude Code's diff
            // highlight bands extending to the end of a line with nothing
            // else on it. Trimming those would silently cut the visible
            // highlight short.
            let end = row.iter().rposition(|c| c.ch != ' ' || c.fg != dfg || c.bg.is_some())
                .map(|i| i + 1).unwrap_or(0);

            // Group consecutive same-(fg,bg) cells into spans
            let mut si = 0;
            while si < end {
                let fg  = row[si].fg;
                let bg  = row[si].bg;
                let mut ei = si + 1;
                while ei < end && row[ei].fg == fg && row[ei].bg == bg { ei += 1; }
                let text: String = row[si..ei].iter().map(|c| c.ch).collect();
                job.append(&text, 0.0, fmt(fg, bg));
                si = ei;
            }

            if ri < row_count - 1 {
                job.append("\n", 0.0, fmt(dfg, None));
            }
        }
        job
    }

    /// Scrollback only — cache this on `scrollback_version`, since it never
    /// changes except when a line actually scrolls into history.
    pub fn to_scrollback_layout_job(&self, font_id: egui::FontId) -> egui::text::LayoutJob {
        let rows: Vec<&Vec<Cell>> = self.scrollback.iter().collect();
        Self::rows_to_layout_job(&rows, font_id)
    }

    /// Viewport only (~terminal panel height, not the whole history) —
    /// cache this on `version`, which bumps on every content-affecting
    /// change and is cheap to re-shape since it's always small.
    pub fn to_viewport_layout_job(&self, font_id: egui::FontId) -> egui::text::LayoutJob {
        let rows: Vec<&Vec<Cell>> = self.viewport.iter().collect();
        Self::rows_to_layout_job(&rows, font_id)
    }
}

// ── Terminal ──────────────────────────────────────────────────────────────────

/// Feeds bytes from any source (a raw PTY reader thread, or a pty-host
/// daemon's push-notification channel) into `grid`, holding back an
/// incomplete trailing UTF-8 sequence across reads instead of decoding it
/// prematurely — a multi-byte character (box-drawing, spinner glyphs,
/// heavily used by TUIs like Claude Code) can straddle a read boundary,
/// and decoding each read independently via `from_utf8_lossy` would mangle
/// the split character into replacement chars on *both* sides, corrupting
/// column tracking for everything printed afterward.
fn feed_grid(
    grid: &Arc<Mutex<Grid>>, leftover: &mut Vec<u8>, bytes: &[u8],
    last_output: &Arc<Mutex<Option<std::time::Instant>>>,
) {
    leftover.extend_from_slice(bytes);
    let valid_len = match std::str::from_utf8(leftover) {
        Ok(_) => leftover.len(),
        Err(e) => e.valid_up_to(),
    };
    if valid_len > 0 {
        let s = String::from_utf8_lossy(&leftover[..valid_len]).into_owned();
        if let Ok(mut g) = grid.lock() { g.process(&s); }
        leftover.drain(..valid_len);
        *last_output.lock().unwrap() = Some(std::time::Instant::now());
        // Shell output arrives with no user input behind it; the event loop is
        // asleep and has to be told. Both PTY reader threads funnel through
        // here, so this is the single place that needs it.
        crate::wake::wake();
    }
    // A valid UTF-8 sequence is at most 4 bytes; anything longer than that
    // left over is genuinely invalid, not just incomplete — drop it so a
    // malformed byte can't wedge the stream forever.
    if leftover.len() > 4 { leftover.clear(); }
}

/// Drains a pty-host push-notification channel into `grid` on a dedicated
/// thread for as long as the session lives — deliberately *not* gated on
/// this terminal's tab being the visible one, since the shared client's
/// single reader thread demuxes push notifications for every terminal's
/// session through one bounded channel per session; a backgrounded tab
/// that stopped draining would fill its channel and block that shared
/// reader thread, stalling delivery to every other terminal too.
fn spawn_feeder(
    grid: Arc<Mutex<Grid>>, rx: std::sync::mpsc::Receiver<Vec<u8>>,
    last_output: Arc<Mutex<Option<std::time::Instant>>>,
) {
    std::thread::spawn(move || {
        let mut leftover: Vec<u8> = Vec::new();
        while let Ok(bytes) = rx.recv() {
            feed_grid(&grid, &mut leftover, &bytes, &last_output);
        }
    });
}

/// How a `Terminal` actually talks to its shell process.
enum PtyBackend {
    /// Preferred: a session owned by the local pty-host daemon
    /// (`forge-server --listen`), which survives Forge IDE's own process
    /// restarting (see `ptyhost` module docs for why that matters).
    Daemon { client: Arc<crate::ptyhost::PtyHostClient>, id: u32 },
    /// Fallback when the daemon can't be reached (binary missing,
    /// platform without Unix sockets, spawn failure, …) — the PTY owned
    /// directly, exactly as before this feature existed. Fully
    /// functional; just doesn't survive a Reload Window.
    Direct { master: Box<dyn portable_pty::MasterPty + Send>, writer: Box<dyn Write + Send> },
}

/// Hands out a distinct scroll id per terminal.
///
/// Monotonic rather than reusing freed values: an id reused by a new terminal
/// would inherit the previous occupant's remembered scroll offset.
fn next_scroll_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub struct Terminal {
    // Deferred until first draw() when we know the real panel size.
    // `None` for a terminal constructed via `reattach` — already running.
    pending:   Option<std::path::PathBuf>,
    backend:   Option<PtyBackend>,
    /// This session's id in the pty-host daemon, if backed by one — `None`
    /// for the direct-PTY fallback, which has no cross-restart identity.
    pty_id:    Option<u32>,
    /// Distinguishes this terminal's scroll position from every other one's.
    ///
    /// egui keys a scroll area's remembered offset by id, and every terminal was
    /// using the same literal — so all of them shared one offset. Switching to
    /// another shell overwrote it, and switching back applied the other shell's
    /// position against different content, which jumped the view to the top.
    /// A per-instance value keeps each shell's place.
    scroll_id: u64,
    pub grid:  Arc<Mutex<Grid>>,
    /// Kept for the terminal's lifetime (not just transiently for spawn) so
    /// session persistence can record which directory each tab was in.
    cwd:       std::path::PathBuf,
    focused:   bool,
    last_rows: u16,
    last_cols: u16,
    /// A newly observed (rows, cols) and when it was first seen — only
    /// committed once it's held steady for `RESIZE_DEBOUNCE`. A frame-count
    /// debounce isn't enough: during an actual drag-resize, the mouse can
    /// easily move slower than the frame rate, so an intermediate size can
    /// render identically for several frames in a row without the drag
    /// actually being over — each of those was getting committed as a real
    /// resize (a real SIGWINCH, which a shell sitting at an idle prompt
    /// answers by redrawing it), producing a burst of duplicate prompt
    /// lines partway through a single drag gesture. Debouncing by elapsed
    /// time instead collapses the whole gesture into one commit at the end,
    /// regardless of how many intermediate frames it passes through.
    pending_size: Option<((u16, u16), std::time::Instant)>,
    /// Keystrokes queued this frame — drained by app.rs to forward to SSH shell.
    pub pending_input: Vec<Vec<u8>>,
    /// When a keystroke was last sent — the cursor stays solid (no blink)
    /// for a moment after typing, so it's easy to keep track of while
    /// actively typing, then resumes its normal blink rhythm once idle.
    last_input_at: Option<std::time::Instant>,
    /// Whether the view should follow new output. See the `stick_to_bottom`
    /// call in `draw_sized` for why this isn't left to egui.
    stick_bottom: bool,
    /// True while the primary button is held after being pressed inside the
    /// terminal viewport — i.e. a text-selection drag is in progress.
    ///
    /// Needed because egui's label text selection does not scroll its container:
    /// dragging past the top or bottom edge just stops extending the selection.
    /// Tracking the gesture ourselves lets `draw_sized` scroll while it runs.
    /// Gated on "press began inside" so a drag that started elsewhere (a panel
    /// splitter, say) doesn't move the terminal.
    selecting: bool,
    /// When the feeder thread(s) last actually decoded new PTY output —
    /// `None` if none yet. Read by the app to decide whether this terminal
    /// is worth polling at a fast cadence right now (a live command
    /// producing output) versus an idle shell prompt, which doesn't need
    /// to be repainted at 20Hz just because its tab happens to be visible.
    last_output: Arc<Mutex<Option<std::time::Instant>>>,
    /// Last *viewport* Galley built by `draw_sized`, keyed by `Grid::version`
    /// and font — a hit is a cheap `Arc` clone. Kept separate from
    /// `cached_scrollback_galley` so an active viewport update doesn't force
    /// re-shaping the (often much larger, unchanging) scrollback too.
    cached_galley: Option<(u64, egui::FontId, std::sync::Arc<egui::Galley>)>,
    /// Last *scrollback* Galley, keyed by `Grid::scrollback_version` — see
    /// that field's doc comment for why this is cached separately.
    cached_scrollback_galley: Option<(u64, egui::FontId, std::sync::Arc<egui::Galley>)>,
}

/// The width of one column, measured the way the text is actually laid out.
///
/// Not `glyph_width(' ')`. That is the font's own advance for a space, and egui
/// snaps a glyph's advance when it lays a line out — so the two disagree by a
/// fraction of a pixel per character, which is invisible at one character and
/// three quarters of a character wide by the end of a shell prompt. The remote
/// terminal put its cursor at `column × glyph_width`, and the gap between the
/// end of the prompt and the cursor was that error, accumulated. At a different
/// font size the same error runs the other way and the cursor sits inside the
/// text.
///
/// Measured over a run so the division averages out the rounding of any single
/// advance, and in one place so the cursor, the column count and the text cannot
/// hold different opinions about where column *n* is.
pub fn mono_advance(ui: &egui::Ui, font: &egui::FontId) -> f32 {
    ui.fonts(|f| {
        f.layout_no_wrap("a".repeat(40), font.clone(), egui::Color32::WHITE)
            .rect.width() / 40.0
    }).max(1.0)
}

impl Terminal {
    /// True while this terminal has keyboard focus (clicked into). Used by
    /// the app to suppress global keyboard shortcuts so typing shell
    /// commands/readline chords (Ctrl+N, Ctrl+P, Ctrl+F, …) doesn't also
    /// trigger IDE actions like New File or Quick Open.
    pub fn is_focused(&self) -> bool { self.focused }

    /// The directory this terminal was opened in.
    pub fn cwd(&self) -> &Path { &self.cwd }

    /// Kills the current shell and respawns a fresh one in `new_cwd` — used
    /// when the whole workspace switches to a different folder, so existing
    /// terminal tabs land in the new folder instead of quietly staying
    /// behind in the one they were opened in (deliberately not matching
    /// VS Code's "leave existing terminals where they were" behavior here,
    /// per explicit preference).
    pub fn restart_in(&mut self, new_cwd: &Path) {
        match self.backend.take() {
            // The daemon keeps a session alive independent of this Terminal
            // value (that's the whole point, for Reload Window) — so it has
            // to be told explicitly to close, or the old shell just leaks
            // there forever with nothing left pointing at it.
            Some(PtyBackend::Daemon { client, id }) => { let _ = client.pty_close(id); }
            // Dropping `master`/`writer` closes the underlying fds, which
            // delivers EOF/HUP to the shell — it exits on its own.
            Some(PtyBackend::Direct { .. }) | None => {}
        }
        self.pty_id = None;
        self.cwd = new_cwd.to_path_buf();
        // Deferred spawn, exactly like a brand-new tab — `draw_sized` calls
        // `spawn()` (with the panel's *current* size) the next time it runs,
        // which also resets the grid and `last_output`.
        self.pending = Some(new_cwd.to_path_buf());
        self.cached_galley = None;
        self.cached_scrollback_galley = None;
    }

    /// This session's id in the pty-host daemon — `Some` for a normal
    /// terminal, `None` for the direct-PTY fallback. Recorded in session
    /// state so a future reload can reattach to the same running shell
    /// instead of opening a new one.
    pub fn pty_id(&self) -> Option<u32> { self.pty_id }

    /// Snapshot of what's currently on screen — see `GridSnapshot`/`reattach`.
    pub fn snapshot_viewport(&self) -> Option<GridSnapshot> {
        self.grid.lock().ok().map(|g| g.snapshot())
    }

    /// True if this terminal's feeder thread decoded new output within
    /// `within` — i.e. a command is actively producing output right now,
    /// as opposed to a shell just sitting idle at its prompt. Used to
    /// decide whether this terminal is worth a fast repaint cadence.
    pub fn recently_active(&self, within: std::time::Duration) -> bool {
        self.last_output.lock().ok()
            .and_then(|g| *g)
            .is_some_and(|at| at.elapsed() < within)
    }

    /// Stores spawn info only — actual PTY is created on the first draw()
    /// call so the shell starts with the exact panel dimensions.
    pub fn new(cwd: &Path) -> Self {
        Self {
            pending:   Some(cwd.to_path_buf()),
            backend:   None,
            pty_id:    None,
            scroll_id: next_scroll_id(),
            grid:          Arc::new(Mutex::new(Grid::new())),
            cwd:           cwd.to_path_buf(),
            focused:       false,
            last_rows:     0,
            last_cols:     0,
            pending_size:  None,
            pending_input: Vec::new(),
            last_input_at: None,
            stick_bottom: true,
            selecting: false,
            last_output:   Arc::new(Mutex::new(None)),
            cached_galley: None,
            cached_scrollback_galley: None,
        }
    }

    /// Reattaches to a session already running in the pty-host daemon —
    /// used when restoring a previous run's terminals after "Reload
    /// Window" instead of spawning fresh shells. `cols`/`rows` should be
    /// the daemon's last-known size for this session (from `pty/list`) so
    /// there's no jarring resize the instant this renders; the normal
    /// resize-debounce logic corrects it to the real panel size shortly
    /// after, same as any other terminal.
    ///
    /// `snapshot`, if present, is what the *previous* process's `Grid`
    /// looked like right before it was replaced (see `snapshot_viewport`) —
    /// restored onto the fresh grid before the live feed thread starts, so
    /// the reattached terminal shows what was actually on screen instead of
    /// sitting blank until new output happens to arrive. Applied before
    /// `spawn_feeder` sees the grid at all, so there's no race between the
    /// restore and any bytes that arrive in the meantime.
    pub fn reattach(
        client: Arc<crate::ptyhost::PtyHostClient>, id: u32, cwd: &Path, cols: u16, rows: u16,
        snapshot: Option<GridSnapshot>,
    ) -> Self {
        let mut grid0 = Grid::with_size(rows as usize, cols as usize);
        let had_snapshot = snapshot.is_some();
        if let Some(snap) = &snapshot {
            grid0.restore_snapshot(snap);
        }
        let grid = Arc::new(Mutex::new(grid0));
        let last_output = Arc::new(Mutex::new(None));
        let rx = client.reattach(id);
        spawn_feeder(grid.clone(), rx, last_output.clone());
        // Without a snapshot to restore, the freshly-created grid has no
        // history (the daemon doesn't buffer output either — nothing to
        // replay), and the shell has no idea a new client just connected, so
        // it has no reason to print anything on its own — the terminal would
        // otherwise render as an empty box until you happen to type
        // something. Ctrl+L is `clear-screen` in both bash/readline and
        // zsh/zle by default, making the shell immediately redraw its
        // current prompt. Skipped when a snapshot *was* restored: the screen
        // already shows the right thing, and forwarding a raw Ctrl+L to
        // whatever's in the foreground (not necessarily the shell's own
        // prompt — could be vim, a REPL, anything) risks it being consumed
        // as ordinary input rather than a redraw request.
        if !had_snapshot {
            let _ = client.pty_write(id, b"\x0c");
        }
        Self {
            pending: None,
            backend: Some(PtyBackend::Daemon { client, id }),
            pty_id:  Some(id),
            scroll_id: next_scroll_id(),
            grid,
            cwd:       cwd.to_path_buf(),
            focused:       false,
            last_rows: rows,
            last_cols: cols,
            pending_size:  None,
            pending_input: Vec::new(),
            last_input_at: None,
            stick_bottom: true,
            selecting: false,
            last_output,
            cached_galley: None,
            cached_scrollback_galley: None,
        }
    }

    fn spawn(&mut self, rows: u16, cols: u16) {
        let Some(cwd) = self.pending.take() else { return };
        *self.grid.lock().unwrap() = Grid::with_size(rows as usize, cols as usize);
        *self.last_output.lock().unwrap() = None;

        if let Some(client) = crate::ptyhost::shared() {
            match client.pty_open(cols, rows, &cwd.to_string_lossy()) {
                Ok((id, rx)) => {
                    spawn_feeder(self.grid.clone(), rx, self.last_output.clone());
                    self.pty_id  = Some(id);
                    self.backend = Some(PtyBackend::Daemon { client, id });
                    self.last_rows = rows;
                    self.last_cols = cols;
                    return;
                }
                Err(e) => eprintln!("ptyhost: pty/open failed, falling back to direct PTY: {e}"),
            }
        }

        // Fallback: own the PTY directly (daemon unreachable/unavailable).
        // This terminal won't survive a Reload Window, but is otherwise
        // fully functional — exactly how terminals worked before this
        // feature existed.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let pty = NativePtySystem::default();
        let pair = match pty.openpty(PtySize { rows, cols,
                                               pixel_width: 0, pixel_height: 0 }) {
            Ok(p) => p,
            Err(e) => { eprintln!("pty: {e}"); return; }
        };
        let mut cmd = CommandBuilder::new(&shell);
        cmd.cwd(&cwd);
        cmd.env("TERM", "xterm-256color");
        // Each terminal tab is a genuinely new shell, not a resumed one —
        // but `TERM_SESSION_ID` is otherwise inherited unchanged from
        // Forge IDE's own process environment, so every tab shares the
        // *same* id as whatever terminal Forge IDE itself was launched
        // from. macOS's built-in Terminal.app session-restore feature
        // (`/etc/zshrc_Apple_Terminal`) keys its saved-on-exit restore
        // script off that id — so an unrelated shell elsewhere exiting
        // with the same inherited id leaves behind a "restore" script
        // that a *new* tab here then sources, printing a misleading
        // "Restored session: <date>" even though nothing was restored.
        cmd.env_remove("TERM_SESSION_ID");
        if let Err(e) = pair.slave.spawn_command(cmd) {
            eprintln!("spawn: {e}"); return;
        }
        let writer = match pair.master.take_writer() {
            Ok(w) => w,
            Err(e) => { eprintln!("writer: {e}"); return; }
        };
        let mut reader = match pair.master.try_clone_reader() {
            Ok(r) => r,
            Err(e) => { eprintln!("reader: {e}"); return; }
        };
        let grid_clone = self.grid.clone();
        let last_output_clone = self.last_output.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            let mut leftover: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => feed_grid(&grid_clone, &mut leftover, &buf[..n], &last_output_clone),
                }
            }
        });
        self.backend   = Some(PtyBackend::Direct { master: pair.master, writer });
        self.last_rows = rows;
        self.last_cols = cols;
    }

    /// Send pasted text to the program.
    ///
    /// Two things a paste is not allowed to be mistaken for. Line breaks go as CR,
    /// which is what Enter transmits — a bare LF means "down one row" to a terminal
    /// program, not "new line". And when the program has asked for bracketed paste,
    /// the block is wrapped in the markers that tell it this was pasted rather than
    /// typed; without them a pasted multi-line message is submitted a line at a
    /// time by anything that sends on Enter.
    fn paste(&mut self, text: &str) {
        let bracketed = self.grid.lock().map(|g| g.bracketed_paste).unwrap_or(false);
        let bytes = paste_bytes(text, bracketed);
        self.pending_input.push(bytes.clone());
        self.write_bytes(&bytes);
    }

    fn write_bytes(&mut self, data: &[u8]) {
        self.last_input_at = Some(std::time::Instant::now());
        match &mut self.backend {
            Some(PtyBackend::Daemon { client, id }) => { let _ = client.pty_write(*id, data); }
            Some(PtyBackend::Direct { writer, .. }) => { let _ = writer.write_all(data); }
            None => {}
        }
    }

    fn resize_backend(&mut self, cols: u16, rows: u16) {
        match &mut self.backend {
            Some(PtyBackend::Daemon { client, id }) => { let _ = client.pty_resize(*id, cols, rows); }
            Some(PtyBackend::Direct { master, .. }) => {
                let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            }
            None => {}
        }
    }

    pub fn draw(&mut self, ui: &mut egui::Ui) { self.draw_sized(ui, 13.0); }
    pub fn draw_sized(&mut self, ui: &mut egui::Ui, font_size: f32) {
        let font_id = egui::FontId::monospace(font_size);
        let row_h   = ui.fonts(|f| f.row_height(&font_id));
        let char_w  = mono_advance(ui, &font_id);

        let visible_rows = (ui.available_height() / row_h).floor().max(1.0) as u16;
        let visible_cols = (ui.available_width()  / char_w).floor().max(1.0) as u16;

        // First draw: spawn shell with exact panel dimensions — no resize needed
        const RESIZE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(400);
        if self.pending.is_some() {
            self.spawn(visible_rows, visible_cols);
        } else if visible_rows != self.last_rows || visible_cols != self.last_cols {
            let candidate = (visible_rows, visible_cols);
            let settled = match self.pending_size {
                Some((c, since)) if c == candidate => since.elapsed() >= RESIZE_DEBOUNCE,
                _ => { self.pending_size = Some((candidate, std::time::Instant::now())); false }
            };
            if settled {
                self.resize_backend(visible_cols, visible_rows);
                if let Ok(mut g) = self.grid.lock() {
                    g.resize(visible_rows as usize, visible_cols as usize);
                }
                self.last_rows = visible_rows;
                self.last_cols = visible_cols;
                self.pending_size = None;
            } else {
                // Still changing (or not yet held long enough) — keep
                // repainting so the debounce timer actually gets checked
                // again even if nothing else is animating.
                ui.ctx().request_repaint_after(RESIZE_DEBOUNCE);
            }
        } else {
            self.pending_size = None;
        }

        let (version, scrollback_version, cur_row, cur_col, cursor_visible) = {
            let g = self.grid.lock().unwrap();
            let (r, c) = g.cursor();
            (g.version(), g.scrollback_version(), r, c, g.cursor_visible())
        };

        let focus_id   = ui.id().with("term_focus");
        let menu_id    = focus_id.with("ctx_menu");
        let panel_rect = ui.max_rect();

        // Remove egui's default scroll-area rounding/background so the
        // terminal fills the panel edge-to-edge as a flat rectangle.
        let term_bg = egui::Color32::from_rgb(14, 14, 14);
        ui.painter().rect_filled(panel_rect, 0.0, term_bg);
        ui.visuals_mut().extreme_bg_color               = term_bg;
        ui.visuals_mut().widgets.noninteractive.rounding = egui::Rounding::ZERO;
        ui.visuals_mut().widgets.inactive.rounding       = egui::Rounding::ZERO;

        // Detect right-click on the container (VS Code's approach: one listener
        // on the whole panel, not on individual widgets inside it).
        let right_clicked = ui.ctx().input(|i| {
            i.pointer.secondary_clicked()
                && i.pointer.interact_pos().map_or(false, |p| panel_rect.contains(p))
        });
        if right_clicked {
            // Snapshot the cursor position now — the popup must stay fixed
            let pos = ui.ctx().pointer_interact_pos().unwrap_or(panel_rect.center());
            ui.ctx().data_mut(|d| d.insert_temp(menu_id, pos));
            ui.ctx().memory_mut(|m| m.open_popup(menu_id));
        }

        // Computed once here and handed to the `Label`s below instead of
        // letting them build their own, which gives direct access to the
        // viewport galley's *actual* per-row rects for cursor positioning
        // (the cursor was rendering about a line low when its Y was instead
        // computed from an independently-derived `row_h`, which doesn't
        // necessarily match the real line height text layout produces for
        // this font/job).
        //
        // Scrollback and viewport are cached — and rendered — separately
        // (`Grid::scrollback_version`/`version`) rather than as one combined
        // job: scrollback is immutable history, so re-shaping the *entire*
        // thing (thousands of lines on a long session) on every viewport
        // update, which happens constantly while a process is actively
        // producing output, dominated frame time on its own. Only the small
        // viewport actually needs to be cheap to re-shape often.
        let scrollback_galley = match &self.cached_scrollback_galley {
            Some((v, fid, g)) if *v == scrollback_version && *fid == font_id => g.clone(),
            _ => {
                let job = { let g = self.grid.lock().unwrap(); g.to_scrollback_layout_job(font_id.clone()) };
                let galley = ui.fonts(|f| f.layout_job(job));
                self.cached_scrollback_galley = Some((scrollback_version, font_id.clone(), galley.clone()));
                galley
            }
        };
        let galley = match &self.cached_galley {
            Some((v, fid, g)) if *v == version && *fid == font_id => g.clone(),
            _ => {
                let job = { let g = self.grid.lock().unwrap(); g.to_viewport_layout_job(font_id.clone()) };
                let galley = ui.fonts(|f| f.layout_job(job));
                self.cached_galley = Some((version, font_id.clone(), galley.clone()));
                galley
            }
        };

        let scroll_resp = egui::ScrollArea::vertical()
            // Per-terminal, so each shell keeps its own scroll position.
            .id_salt(("term_scroll", self.scroll_id))
            .auto_shrink([false, false])
            // Stickiness is tracked here rather than left to egui's own
            // `scroll_stuck_to_end`, because that flag is private and is only
            // cleared by egui's *own* interaction paths. Programmatic scrolling
            // — which is how drag-select auto-scroll works below — would be
            // snapped straight back to the bottom every frame. Recomputed after
            // the scroll area from the real offset, so it behaves like every
            // other terminal: follow new output while you are at the bottom,
            // stay put once you have scrolled away.
            .stick_to_bottom(self.stick_bottom)
            // egui's default `drag_to_scroll(true)` is a touch-screen-style
            // pan gesture that claims a click-drag on its contents *before*
            // the selectable text inside gets a chance to interpret the same
            // drag as text selection — on a desktop app with a mouse, that
            // means click-drag-to-select terminal output never actually
            // worked, it always scrolled instead. Wheel/trackpad scroll and
            // the scrollbar are untouched by this.
            .drag_to_scroll(false)
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                if !scrollback_galley.rows.is_empty() {
                    ui.add(
                        egui::Label::new(scrollback_galley.clone())
                            .extend()
                            .sense(egui::Sense::click()),
                    );
                }
                ui.add(
                    egui::Label::new(galley.clone())
                        .extend()
                        .sense(egui::Sense::click()),
                )
            });

        // Follow new output only while parked at the bottom — the rule every
        // terminal uses. A couple of pixels of slack absorbs rounding.
        {
            let max_off = (scroll_resp.content_size.y - scroll_resp.inner_rect.height()).max(0.0);
            self.stick_bottom = scroll_resp.state.offset.y >= max_off - 2.0;
        }

        // ── auto-scroll while drag-selecting past an edge ─────────────────────
        // egui's label text selection doesn't scroll its container: drag to the
        // top or bottom edge and the selection simply stops growing, so anything
        // off-screen is unselectable. Track the gesture and scroll for it.
        {
            let view = scroll_resp.inner_rect;
            let pointer = ui.ctx().input(|i| i.pointer.hover_pos());
            let primary_down = ui.ctx().input(|i| i.pointer.primary_down());

            if !primary_down {
                self.selecting = false;
            } else if !self.selecting {
                // Only adopt drags that began in the viewport, so dragging a
                // panel splitter or a scrollbar doesn't move the terminal.
                if ui.ctx().input(|i| i.pointer.primary_pressed())
                    && pointer.is_some_and(|p| view.contains(p))
                {
                    self.selecting = true;
                }
            }

            if self.selecting {
                // Unconditional while the gesture runs: egui updates the
                // selection from pointer position, so it needs frames even when
                // the pointer is parked outside the viewport and not scrolling.
                ui.ctx().request_repaint();
                if let Some(p) = pointer {
                    // Rate is per *second*, scaled by frame time — not a fixed
                    // per-frame step. A per-frame step is a rate multiplied by
                    // however fast the machine happens to render: at the ~120fps
                    // the repaint request below produces, 40px/frame is nearly
                    // 5000px/s, which reaches the end of the scrollback before
                    // you can react and so selects everything below the anchor
                    // no matter where you meant to stop.
                    const MAX_RATE: f32 = 450.0; // px/s at full deflection
                    let dt = ui.ctx().input(|i| i.stable_dt).clamp(0.001, 0.1);
                    let over = if p.y < view.top() {
                        p.y - view.top()          // negative → scroll up
                    } else if p.y > view.bottom() {
                        p.y - view.bottom()       // positive → scroll down
                    } else {
                        0.0
                    };
                    if over != 0.0 {
                        // Ramp over the first ~60px past the edge, then hold.
                        let deflection = (over / 60.0).clamp(-1.0, 1.0);
                        let step = deflection * MAX_RATE * dt;
                        let max_off = (scroll_resp.content_size.y - view.height()).max(0.0);
                        let mut st = scroll_resp.state.clone();
                        let target = (st.offset.y + step).clamp(0.0, max_off);
                        if target != st.offset.y {
                            st.offset.y = target;
                            st.store(ui.ctx(), scroll_resp.id);
                        }
                        // Release stickiness as soon as we scroll up, or the
                        // stored offset above is overridden on the next frame
                        // and the view never leaves the bottom.
                        if step < 0.0 { self.stick_bottom = false; }
                        // Keep frames coming while the pointer sits outside —
                        // the loop is otherwise idle and would stop scrolling.
                        ui.ctx().request_repaint();
                    }
                }
            }
        }

        // ── block cursor ──────────────────────────────────────────────────────
        {
            let scroll_y     = scroll_resp.state.offset.y;
            let content_rect = scroll_resp.inner_rect;
            let row_y = scrollback_galley.size().y + galley.rows.get(cur_row).map_or(
                cur_row as f32 * row_h,
                |r| r.rect.min.y,
            );
            let cx = content_rect.min.x + cur_col as f32 * char_w;
            let cy = content_rect.min.y  + row_y - scroll_y;

            // Solid (no blink) for a moment after typing, so the cursor is
            // easy to keep track of while actively typing — then back to
            // its normal blink rhythm once idle.
            const SOLID_AFTER_INPUT: std::time::Duration = std::time::Duration::from_millis(500);
            let recently_typed = self.last_input_at
                .is_some_and(|at| at.elapsed() < SOLID_AFTER_INPUT);
            let t       = ui.ctx().input(|i| i.time);
            let blinking = (t * 2.0) as u64 % 2 == 0;
            let visible = self.focused && cursor_visible && (recently_typed || blinking);
            if recently_typed {
                ui.ctx().request_repaint_after(SOLID_AFTER_INPUT);
            }
            if visible && cy >= content_rect.min.y && cy + row_h <= content_rect.max.y + row_h {
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(egui::pos2(cx, cy), egui::vec2(char_w, row_h)),
                    0.0,
                    egui::Color32::from_rgba_unmultiplied(200, 200, 200, 180),
                );
            }
        }

        // ── terminal context menu popup ───────────────────────────────────────
        if ui.ctx().memory(|m| m.is_popup_open(menu_id)) {
            let cursor = ui.ctx().data(|d| d.get_temp::<egui::Pos2>(menu_id))
                .unwrap_or(panel_rect.center());
            let mut close = false;

            egui::Area::new(menu_id)
                .order(egui::Order::Foreground)
                .fixed_pos(cursor)
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(160.0);

                        if ui.button("Copy").clicked() {
                            let text = self.grid.lock().map(|g| g.all_text()).unwrap_or_default();
                            ui.output_mut(|o| o.copied_text = text);
                            close = true;
                        }
                        if ui.button("Paste").clicked() {
                            // egui surfaces paste via Event::Paste; grab the latest one
                            let clip = ui.ctx().input(|i| {
                                i.events.iter().rev().find_map(|e| {
                                    if let egui::Event::Paste(s) = e { Some(s.clone()) } else { None }
                                })
                            });
                            if let Some(text) = clip {
                                self.paste(&text);
                            }
                            close = true;
                        }
                        ui.separator();
                        if ui.button("Clear").clicked() {
                            // Keep the current size — resetting to `Grid::new()`'s
                            // default would visibly snap the viewport to 24x80
                            // for a frame until the next resize check corrects it.
                            if let Ok(mut g) = self.grid.lock() {
                                *g = Grid::with_size(self.last_rows.max(1) as usize,
                                                      self.last_cols.max(1) as usize);
                            }
                            close = true;
                        }
                        ui.separator();
                        if ui.button(
                            egui::RichText::new("Kill Terminal")
                                .color(egui::Color32::from_rgb(240, 80, 80))
                        ).clicked() {
                            self.write_bytes(b"exit\n");
                            close = true;
                        }

                        // Close on left-click outside (not right-click — that's what opened us)
                        if ui.input(|i| i.pointer.primary_clicked())
                            && !ui.rect_contains_pointer(ui.min_rect())
                        {
                            close = true;
                        }
                        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                            close = true;
                        }
                    });
                });

            if close {
                ui.ctx().memory_mut(|m| m.close_popup());
            }
        }

        // ── focus ─────────────────────────────────────────────────────────────
        let panel_resp = ui.interact(panel_rect, focus_id.with("panel"), egui::Sense::click());
        if panel_resp.clicked() || scroll_resp.inner.clicked() {
            ui.memory_mut(|m| m.request_focus(focus_id));
            self.focused = true;
        }
        if ui.input(|i| i.pointer.any_click()) && !panel_rect.contains(
            ui.input(|i| i.pointer.interact_pos().unwrap_or_default())
        ) {
            self.focused = false;
        }
        if self.focused {
            // No border — focus is indicated by the blinking cursor only.
        }

        // ── drag-and-drop: insert file path ─────────────────────────────────
        // Matches real terminals (Terminal.app, iTerm2): dropping a file
        // types out its path rather than doing anything with the file
        // itself — e.g. dragging a screenshot in lets you tell a CLI tool
        // running here (Claude Code included) to go look at that path,
        // which is how "posting a screenshot into a terminal" actually
        // works anywhere, not just here.
        let dropped = ui.ctx().input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() && panel_rect.contains(
            ui.ctx().input(|i| i.pointer.interact_pos().unwrap_or_default())
        ) {
            self.focused = true;
            ui.memory_mut(|m| m.request_focus(focus_id));
            let paths: Vec<String> = dropped.iter()
                .filter_map(|f| f.path.as_ref())
                .map(|p| {
                    // Single-quote for shell-safety (spaces, parens, etc.).
                    format!("'{}'", p.to_string_lossy().replace('\'', r"'\''"))
                })
                .collect();
            if !paths.is_empty() {
                let bytes = paths.join(" ").into_bytes();
                self.pending_input.push(bytes.clone());
                self.write_bytes(&bytes);
            }
        }

        // ── keyboard → PTY ────────────────────────────────────────────────────
        if self.focused {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            let events = ui.input(|i| i.events.clone());
            for event in &events {
                match event {
                    // egui surfaces the system paste shortcut (Cmd+V) as its
                    // own event with the clipboard text already resolved —
                    // this was never handled here at all, so pasting
                    // silently did nothing (only the right-click "Paste"
                    // menu item worked, which reads the clipboard itself).
                    egui::Event::Paste(s) => {
                        let text = s.clone();
                        self.paste(&text);
                    }
                    // Cmd+C (not Ctrl+C, which is SIGINT) — copy. There's no
                    // partial mouse-selection yet, so this matches the
                    // right-click "Copy" menu item: the whole terminal's
                    // text. Previously Cmd was treated the same as Ctrl for
                    // control codes, so this sent SIGINT instead.
                    egui::Event::Key { key: egui::Key::C, pressed: true, modifiers, .. }
                        if modifiers.mac_cmd && !modifiers.ctrl =>
                    {
                        let text = self.grid.lock().map(|g| g.all_text()).unwrap_or_default();
                        ui.output_mut(|o| o.copied_text = text);
                    }
                    egui::Event::Text(t) => {
                        let bytes = t.as_bytes().to_vec();
                        self.pending_input.push(bytes.clone());
                        self.write_bytes(&bytes);
                    }
                    egui::Event::Key { key, pressed: true, modifiers, .. } => {
                        if let Some(b) = key_to_pty(*key, *modifiers) {
                            self.pending_input.push(b.clone());
                            self.write_bytes(&b);
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// The bytes a paste sends.
///
/// Kept out of `Terminal::paste` so it can be tested without a pty behind it.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r");
    let mut bytes = Vec::with_capacity(body.len() + 12);
    if bracketed {
        bytes.extend_from_slice(b"\x1b[200~");
    }
    bytes.extend_from_slice(body.as_bytes());
    if bracketed {
        bytes.extend_from_slice(b"\x1b[201~");
    }
    bytes
}

pub fn key_to_pty(key: egui::Key, m: egui::Modifiers) -> Option<Vec<u8>> {
    // Alt/Option+Enter — the standard "insert a literal newline instead of
    // submitting" convention most readline-based line editors (including
    // ink, which Claude Code is built on) respond to. Previously
    // unhandled: Enter always sent plain `\r` regardless of modifiers, so
    // Option+Enter behaved identically to plain Enter.
    /// The control code for Ctrl+<letter>, or `None` for anything else.
    fn ctrl_code(key: egui::Key) -> Option<u8> {
        use egui::Key::*;
        let letter = match key {
            A => b'a', B => b'b', C => b'c', D => b'd', E => b'e', F => b'f',
            G => b'g', H => b'h', I => b'i', J => b'j', K => b'k', L => b'l',
            M => b'm', N => b'n', O => b'o', P => b'p', Q => b'q', R => b'r',
            S => b's', T => b't', U => b'u', V => b'v', W => b'w', X => b'x',
            Y => b'y', Z => b'z',
            _ => return None,
        };
        // 'a' is 0x61; masking off 0x60 leaves 0x01.
        Some(letter - 0x60)
    }

    if m.alt && key == egui::Key::Enter {
        return Some(b"\x1b\r".to_vec());
    }
    // Terminal control codes (SIGINT, etc.) are a *physical Control key*
    // convention on every platform, including macOS — Ctrl+C means SIGINT
    // in Terminal.app and iTerm2 too. Cmd was previously treated the same
    // as Ctrl here, which meant Cmd+C sent SIGINT instead of copying and
    // Cmd+V had no case at all (and wasn't handled anywhere else either —
    // see the `Paste` event handling below), breaking the two most basic
    // macOS clipboard shortcuts in the terminal entirely.
    // Shift+Tab is a sequence of its own, not Tab with a flag: terminals send
    // `CSI Z` for it. Sending a plain Tab means a program that binds Shift+Tab
    // sees an ordinary Tab and does whatever Tab does — the Rust TUI cycles
    // permission modes with it, and inside this terminal that keypress instead
    // completed a slash command.
    if m.shift && key == egui::Key::Tab {
        return Some(b"\x1b[Z".to_vec());
    }

    if m.ctrl {
        // Every letter, computed rather than listed. Ctrl+<letter> is the
        // letter's position in the alphabet as a control code: Ctrl+A is 0x01
        // through Ctrl+Z at 0x1A.
        //
        // This used to be a hand-written whitelist of thirteen letters, which
        // silently dropped the rest — Ctrl+X, Ctrl+G, Ctrl+F, Ctrl+O and the
        // others simply never reached the program. That is not an edge case:
        // emacs is built on Ctrl+X, vim scrolls with Ctrl+F and Ctrl+B, and
        // any full-screen application is free to bind any of them. A letter
        // missing from the list was indistinguishable from a key that did
        // nothing.
        if let Some(code) = ctrl_code(key) {
            return Some(vec![code]);
        }
        return match key {
            // The handful of non-letter control codes worth carrying.
            egui::Key::Space           => Some(b"\x00".to_vec()), // NUL
            egui::Key::OpenBracket     => Some(b"\x1b".to_vec()), // Ctrl+[ is ESC
            egui::Key::Backslash       => Some(b"\x1c".to_vec()),
            egui::Key::CloseBracket    => Some(b"\x1d".to_vec()),
            _ => None,
        };
    }
    match key {
        egui::Key::Enter      => Some(b"\r".to_vec()),
        egui::Key::Backspace  => Some(b"\x7f".to_vec()),
        egui::Key::Tab        => Some(b"\t".to_vec()),
        egui::Key::Escape     => Some(b"\x1b".to_vec()),
        egui::Key::ArrowUp    => Some(b"\x1b[A".to_vec()),
        egui::Key::ArrowDown  => Some(b"\x1b[B".to_vec()),
        egui::Key::ArrowRight => Some(b"\x1b[C".to_vec()),
        egui::Key::ArrowLeft  => Some(b"\x1b[D".to_vec()),
        egui::Key::Home       => Some(b"\x1b[H".to_vec()),
        egui::Key::End        => Some(b"\x1b[F".to_vec()),
        egui::Key::PageUp     => Some(b"\x1b[5~".to_vec()),
        egui::Key::PageDown   => Some(b"\x1b[6~".to_vec()),
        egui::Key::Delete     => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

#[cfg(test)]
mod ink_redraw_tests {
    use super::{Grid, MAX_SCROLLBACK};

    /// One ink-style repaint: move the cursor up over the previously drawn
    /// block, erase to end of screen, then redraw it. This is how ink (and so
    /// forge-tui, and Claude Code) animates a live region — no alternate
    /// screen, everything inline.
    fn repaint(g: &mut Grid, lines: usize, tick: usize) {
        if tick > 0 {
            g.process(&format!("\x1b[{lines}A"));  // cursor up over the block
            g.process("\x1b[J");                    // erase to end of screen
        }
        for i in 0..lines {
            g.process(&format!("status line {i} tick {tick}"));
            if i + 1 < lines { g.process("\r\n"); }
        }
    }

    /// Repainting a live region in place must not grow scrollback. If it does,
    /// total content height climbs every frame and a bottom-pinned view is
    /// re-pinned to a moving target — which reads as the terminal jittering.
    #[test]
    fn in_place_repaint_does_not_grow_scrollback() {
        let mut g = Grid::with_size(24, 80);
        let block = 6;

        repaint(&mut g, block, 0);
        let after_first = g.scrollback.len();

        for tick in 1..200 {
            repaint(&mut g, block, tick);
        }

        let grown = g.scrollback.len() - after_first;
        assert_eq!(
            grown, 0,
            "199 in-place repaints added {grown} scrollback lines \
             (before {after_first}, after {}); a live region redrawn in place \
             should add none",
            g.scrollback.len()
        );
        assert!(g.scrollback.len() < MAX_SCROLLBACK);
    }

    /// Sanity check the opposite case: genuinely new output *should* scroll.
    #[test]
    fn real_output_does_scroll_into_scrollback() {
        let mut g = Grid::with_size(24, 80);
        for i in 0..100 {
            g.process(&format!("line {i}\r\n"));
        }
        assert!(g.scrollback.len() > 50,
                "100 lines of real output on a 24-row screen should scroll, got {}",
                g.scrollback.len());
    }
}

#[cfg(test)]
mod private_mode_tests {
    use super::{Grid, SYNC_UPDATE_TIMEOUT};

    fn filled(g: &mut Grid, lines: usize, tag: &str) {
        let mut s = String::new();
        for i in 0..lines { s.push_str(&format!("{tag} {i}\r\n")); }
        g.process(&s);
    }

    /// The point of the alternate screen: a full-screen program's repaints must
    /// not end up in the scrollback. Without this, leaving vim/htop left
    /// thousands of junk lines behind.
    #[test]
    fn alt_screen_scrolling_does_not_touch_scrollback() {
        let mut g = Grid::with_size(10, 40);
        filled(&mut g, 30, "history");
        let before = g.scrollback.len();
        assert!(before > 0, "precondition: real output scrolled into history");

        g.process("\x1b[?1049h");
        filled(&mut g, 200, "tui frame");
        assert_eq!(g.scrollback.len(), before,
                   "alternate screen must not add scrollback");

        g.process("\x1b[?1049l");
        assert_eq!(g.scrollback.len(), before, "leaving must not add any either");
    }

    /// Leaving restores exactly what the main screen had.
    #[test]
    fn alt_screen_restores_the_main_viewport() {
        let mut g = Grid::with_size(6, 40);
        g.process("main content here");
        let main_before = g.all_text();

        g.process("\x1b[?1049h");
        g.process("totally different full-screen thing");
        assert_ne!(g.all_text(), main_before, "alt screen shows its own content");

        g.process("\x1b[?1049l");
        assert_eq!(g.all_text(), main_before, "main screen came back intact");
    }

    /// Entering twice must not park the alternate screen as if it were the main
    /// one — that would lose the real main screen permanently.
    #[test]
    fn re_entering_alt_screen_is_idempotent() {
        let mut g = Grid::with_size(6, 40);
        g.process("the real main screen");
        let main = g.all_text();
        g.process("\x1b[?1049h");
        g.process("alt one");
        g.process("\x1b[?1049h");   // again — must be a no-op
        g.process("alt two");
        g.process("\x1b[?1049l");
        assert_eq!(g.all_text(), main);
    }

    /// 1049 saves and restores the cursor; 47 does not.
    #[test]
    fn cursor_save_differs_between_1049_and_47() {
        let mut g = Grid::with_size(8, 40);
        g.process("\x1b[5;10H");                 // row 5, col 10 (1-based)
        let pos = g.cursor();
        g.process("\x1b[?1049h");
        assert_eq!(g.cursor(), (0, 0), "alt screen starts at home");
        g.process("\x1b[?1049l");
        assert_eq!(g.cursor(), pos, "1049 restores the cursor");

        g.process("\x1b[3;7H");
        g.process("\x1b[?47h");
        g.process("\x1b[?47l");
        assert_eq!(g.cursor(), (0, 0), "47 does not restore it");
    }

    /// Synchronized update withholds presentation: `version` drives the renderer's
    /// galley cache, so an unchanged version means the last complete frame stays
    /// on screen while this one is written.
    #[test]
    fn synchronized_update_defers_presentation() {
        let mut g = Grid::with_size(10, 40);
        g.process("settled");
        let v = g.version();

        g.process("\x1b[?2026h");
        for i in 0..20 { g.process(&format!("frame piece {i}\r\n")); }
        assert_eq!(g.version(), v, "nothing should be presented mid-frame");

        g.process("\x1b[?2026l");
        assert_eq!(g.version(), v + 1, "exactly one atomic bump for the frame");
    }

    /// An application that opens a synchronized update and never closes it must
    /// not freeze the display.
    #[test]
    fn synchronized_update_times_out() {
        let mut g = Grid::with_size(10, 40);
        g.process("\x1b[?2026h");
        g.process("held back");
        let held = g.version();

        // Backdate the open so the next write is past the deadline.
        g.sync_update = Some(std::time::Instant::now() - SYNC_UPDATE_TIMEOUT * 2);
        g.process("this must get through");
        assert!(g.version() > held, "presentation resumed after the timeout");
        assert!(g.sync_update.is_none(), "and the stuck state was cleared");
    }

    /// Batched private modes must all apply — `?2026;25h` sets both.
    #[test]
    fn batched_private_modes_all_apply() {
        let mut g = Grid::with_size(6, 40);
        g.process("\x1b[?25l");
        assert!(!g.cursor_visible());
        g.process("\x1b[?2026;25h");
        assert!(g.cursor_visible(), "cursor visibility applied from a batch");
        assert!(g.sync_update.is_some(), "and so was the synchronized update");
        g.process("\x1b[?2026l");
    }

    /// Narrowing the window must re-break lines, not cut their tails off.
    ///
    /// The old resize called `row.resize(new_cols, blank)` on every row, which
    /// destroyed text that had already been printed — a terminal losing output
    /// because the window got smaller.
    #[test]
    fn narrowing_reflows_instead_of_truncating() {
        let mut g = Grid::with_size(6, 40);
        let text = "the quick brown fox jumps over the lazy dog";
        g.process(text);

        g.resize(6, 20);
        let after = g.all_text();
        let joined: String = after.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            joined.contains("lazy dog"),
            "the tail survived the narrowing: {joined:?}",
        );
        for word in ["quick", "brown", "jumps"] {
            assert!(joined.contains(word), "{word:?} was lost: {joined:?}");
        }
    }

    /// And widening must put the line back together rather than leaving the old
    /// breaks frozen in place.
    #[test]
    fn widening_rejoins_a_wrapped_line() {
        let mut g = Grid::with_size(8, 20);
        g.process("abcdefghijklmnopqrstuvwxyz0123456789");

        g.resize(8, 60);
        let rows: Vec<String> = (0..8).map(|r| g.row_text(r)).collect();
        let first: &String = rows.iter().find(|r| r.contains("abc")).expect("the line");
        assert!(
            first.contains("xyz0123456789"),
            "rejoined onto one row at the wider size: {first:?}",
        );
    }

    /// A round trip must not corrupt anything: narrow, then widen back.
    #[test]
    fn a_narrow_then_widen_round_trip_preserves_the_text() {
        let mut g = Grid::with_size(10, 60);
        let lines = ["first line of output", "second line here", "third and last"];
        for line in lines {
            g.process(line);
            g.process("\r\n");
        }

        g.resize(10, 15);
        g.resize(10, 60);

        let text = g.all_text();
        for line in lines {
            let squashed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let want: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(squashed.contains(&want), "{want:?} lost: {squashed:?}");
        }
    }

    /// Hard newlines are not continuations, so separate lines must stay separate
    /// however the window is resized.
    #[test]
    fn separate_lines_are_never_joined_by_a_resize() {
        let mut g = Grid::with_size(8, 40);
        g.process("alpha\r\nbeta\r\ngamma");

        g.resize(8, 100);
        let rows: Vec<String> = (0..8).map(|r| g.row_text(r).trim_end().to_string()).collect();
        assert!(rows.iter().any(|r| r == "alpha"), "alpha stands alone: {rows:?}");
        assert!(rows.iter().any(|r| r == "beta"), "beta stands alone: {rows:?}");
        assert!(rows.iter().any(|r| r == "gamma"), "gamma stands alone: {rows:?}");
    }

    /// With autowrap off nothing is a continuation, so a resize must not join
    /// lines a full-screen application drew as separate rows.
    #[test]
    fn lines_drawn_with_autowrap_off_are_not_joined() {
        let mut g = Grid::with_size(4, 10);
        g.process("\x1b[?7l");
        g.process("\x1b[1;1Habcdefghij");   // exactly fills row 1
        g.process("\x1b[2;1Hklmnopqrst");   // exactly fills row 2

        g.resize(4, 30);
        let rows: Vec<String> = (0..4).map(|r| g.row_text(r).trim_end().to_string()).collect();
        assert!(
            rows.iter().any(|r| r == "abcdefghij"),
            "the first row was not absorbed into a longer line: {rows:?}",
        );
    }

    /// The flags must stay the same length as the rows they describe, through
    /// scrolling and the alternate screen.
    #[test]
    fn the_wrap_flags_stay_in_step_with_the_rows() {
        let mut g = Grid::with_size(5, 20);
        filled(&mut g, 40, "line");
        assert_eq!(g.viewport.len(), g.wrapped.len(), "after scrolling");
        assert_eq!(g.scrollback.len(), g.sb_wrapped.len());

        g.process("\x1b[?1049h");
        filled(&mut g, 20, "alt");
        assert_eq!(g.viewport.len(), g.wrapped.len(), "on the alternate screen");
        g.process("\x1b[?1049l");
        assert_eq!(g.viewport.len(), g.wrapped.len(), "back on the main screen");
        assert_eq!(g.scrollback.len(), g.sb_wrapped.len());

        g.resize(8, 35);
        assert_eq!(g.viewport.len(), g.wrapped.len(), "after a resize");
        assert_eq!(g.scrollback.len(), g.sb_wrapped.len());
    }

    /// Resizing an empty grid must still leave a full viewport.
    #[test]
    fn resizing_an_empty_grid_keeps_a_full_viewport() {
        let mut g = Grid::with_size(5, 20);
        g.resize(9, 7);
        assert_eq!(g.viewport.len(), 9);
        assert!(g.viewport.iter().all(|r| r.len() == 7));
    }

    /// Every size must be survivable, including degenerate ones.
    #[test]
    fn no_resize_sequence_panics() {
        let mut g = Grid::with_size(10, 40);
        filled(&mut g, 30, "content that is long enough to wrap at small widths");
        for (rows, cols) in [(1usize, 1usize), (2, 3), (60, 200), (5, 1), (1, 200), (10, 40)] {
            g.resize(rows, cols);
            assert_eq!(g.viewport.len(), g.rows);
            assert_eq!(g.viewport.len(), g.wrapped.len());
        }
    }

    /// Each terminal must have its own scroll identity.
    ///
    /// egui remembers a scroll area's offset by id, and every terminal shared one
    /// literal — so switching from shell 1 to shell 2 and back applied shell 2's
    /// offset to shell 1's content, which jumped the view to the top.
    #[test]
    fn terminals_get_distinct_scroll_ids() {
        let dir = std::env::temp_dir();
        let a = super::Terminal::new(&dir);
        let b = super::Terminal::new(&dir);
        let c = super::Terminal::new(&dir);
        assert_ne!(a.scroll_id, b.scroll_id, "two terminals, two positions");
        assert_ne!(b.scroll_id, c.scroll_id);
        assert_ne!(a.scroll_id, c.scroll_id);
    }

    /// Ids must not be reused, or a new terminal inherits the remembered offset
    /// of whichever one previously held that id.
    #[test]
    fn scroll_ids_are_not_reused_after_a_terminal_is_dropped() {
        let dir = std::env::temp_dir();
        let first = super::Terminal::new(&dir).scroll_id;
        // Create and drop several.
        for _ in 0..3 {
            let _ = super::Terminal::new(&dir);
        }
        let later = super::Terminal::new(&dir).scroll_id;
        assert!(later > first, "ids only move forwards: {first} then {later}");
    }

    /// Every Ctrl+letter has to reach the program.
    ///
    /// This was a whitelist of thirteen letters, and the rest were silently
    /// dropped — indistinguishable, from inside the program, from a key that did
    /// nothing. Ctrl+X alone breaks emacs; Ctrl+F and Ctrl+B break scrolling in
    /// vim; Ctrl+T meant forge's own TUI could not open its menu.
    #[test]
    fn every_ctrl_letter_maps_to_its_control_code() {
        use egui::Key::*;
        let letters = [
            (A, 0x01), (B, 0x02), (C, 0x03), (D, 0x04), (E, 0x05), (F, 0x06),
            (G, 0x07), (H, 0x08), (I, 0x09), (J, 0x0a), (K, 0x0b), (L, 0x0c),
            (M, 0x0d), (N, 0x0e), (O, 0x0f), (P, 0x10), (Q, 0x11), (R, 0x12),
            (S, 0x13), (T, 0x14), (U, 0x15), (V, 0x16), (W, 0x17), (X, 0x18),
            (Y, 0x19), (Z, 0x1a),
        ];
        let ctrl = egui::Modifiers { ctrl: true, ..Default::default() };
        for (key, expected) in letters {
            assert_eq!(
                super::key_to_pty(key, ctrl),
                Some(vec![expected]),
                "Ctrl+{key:?} must send {expected:#04x}",
            );
        }
    }

    /// The three that were missing and mattered most.
    #[test]
    fn the_previously_dropped_chords_now_reach_the_program() {
        let ctrl = egui::Modifiers { ctrl: true, ..Default::default() };
        assert_eq!(super::key_to_pty(egui::Key::T, ctrl), Some(vec![0x14]), "menu");
        assert_eq!(super::key_to_pty(egui::Key::X, ctrl), Some(vec![0x18]), "interrupt");
        assert_eq!(super::key_to_pty(egui::Key::G, ctrl), Some(vec![0x07]));
    }

    /// Ctrl+C must still be SIGINT, and Cmd must still not be treated as Ctrl —
    /// that regression broke copy and paste once already.
    #[test]
    fn ctrl_c_is_still_sigint_and_cmd_is_not_ctrl() {
        let ctrl = egui::Modifiers { ctrl: true, ..Default::default() };
        assert_eq!(super::key_to_pty(egui::Key::C, ctrl), Some(vec![0x03]));

        let cmd = egui::Modifiers { mac_cmd: true, command: true, ..Default::default() };
        assert_ne!(
            super::key_to_pty(egui::Key::C, cmd),
            Some(vec![0x03]),
            "Cmd+C copies; it must not send SIGINT",
        );
    }

    /// Shift+Tab has its own sequence. Sending a plain Tab means a program that
    /// binds it never sees it — the Rust TUI cycles permission modes with
    /// Shift+Tab, and inside this terminal the keypress completed a slash command
    /// instead.
    #[test]
    fn shift_tab_sends_back_tab_not_a_plain_tab() {
        let shift = egui::Modifiers { shift: true, ..Default::default() };
        assert_eq!(super::key_to_pty(egui::Key::Tab, shift), Some(b"\x1b[Z".to_vec()));

        // Plain Tab is unchanged.
        let none = egui::Modifiers::default();
        assert_eq!(super::key_to_pty(egui::Key::Tab, none), Some(b"\t".to_vec()));
    }

    /// The reported bug: pasting a multi-line message into the Rust TUI submitted
    /// it a line at a time. This terminal ignored `CSI ?2004h`, so a paste was
    /// indistinguishable from typing and every newline in it was an Enter.
    #[test]
    fn a_paste_is_bracketed_when_the_program_asked_for_it() {
        let mut g = Grid::with_size(6, 20);
        assert!(!g.bracketed_paste, "off until asked for");

        g.process("\x1b[?2004h");
        assert!(g.bracketed_paste, "CSI ?2004h turns it on");
        let bytes = super::paste_bytes("one\ntwo", true);
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.starts_with('\u{1b}'), "starts with the marker: {text:?}");
        assert!(text.contains("[200~") && text.contains("[201~"), "bracketed: {text:?}");

        g.process("\x1b[?2004l");
        assert!(!g.bracketed_paste, "and off again");
        let plain = String::from_utf8_lossy(&super::paste_bytes("one\ntwo", false)).to_string();
        assert!(!plain.contains("[200~"), "not bracketed when unasked: {plain:?}");
    }

    /// Line breaks travel as CR either way. A bare LF means "down one row" to a
    /// terminal program, not "new line", so a pasted block would come out as a
    /// staircase.
    #[test]
    fn pasted_line_breaks_become_carriage_returns() {
        for (name, pasted) in [("LF", "a\nb"), ("CRLF", "a\r\nb"), ("CR", "a\rb")] {
            let bytes = super::paste_bytes(pasted, false);
            assert_eq!(
                String::from_utf8_lossy(&bytes), "a\rb",
                "{name} should travel as CR",
            );
        }
    }

    /// The keys the Rust TUI binds, and the bytes this terminal owes it.
    ///
    /// The pairing is what matters: `forge-tui-rs` has
    /// `the_bindings_survive_forge_ides_terminal`, which feeds these same bytes
    /// through its decoder. Pinning both halves is what catches a key going quiet,
    /// which is otherwise indistinguishable from a key that does nothing.
    #[test]
    fn the_keys_the_tui_binds_are_all_encoded() {
        let ctrl  = egui::Modifiers { ctrl: true, ..Default::default() };
        let shift = egui::Modifiers { shift: true, ..Default::default() };
        let none  = egui::Modifiers::default();
        let cases: &[(&str, egui::Key, egui::Modifiers, &[u8])] = &[
            ("Ctrl-C",    egui::Key::C, ctrl,          &[0x03]),
            ("Ctrl-D",    egui::Key::D, ctrl,          &[0x04]),
            ("Ctrl-X",    egui::Key::X, ctrl,          &[0x18]),
            ("Ctrl-N",    egui::Key::N, ctrl,          &[0x0e]),
            ("Ctrl-T",    egui::Key::T, ctrl,          &[0x14]),
            ("Ctrl-O",    egui::Key::O, ctrl,          &[0x0f]),
            ("Ctrl-U",    egui::Key::U, ctrl,          &[0x15]),
            ("Ctrl-G",    egui::Key::G, ctrl,          &[0x07]),
            ("Enter",     egui::Key::Enter, none,      &[0x0d]),
            ("Backspace", egui::Key::Backspace, none,  &[0x7f]),
            ("Escape",    egui::Key::Escape, none,     &[0x1b]),
            ("Up",        egui::Key::ArrowUp, none,    b"\x1b[A"),
            ("Down",      egui::Key::ArrowDown, none,  b"\x1b[B"),
            ("PageUp",    egui::Key::PageUp, none,     b"\x1b[5~"),
            ("PageDown",  egui::Key::PageDown, none,   b"\x1b[6~"),
            ("Home",      egui::Key::Home, none,       b"\x1b[H"),
            ("End",       egui::Key::End, none,        b"\x1b[F"),
            ("Tab",       egui::Key::Tab, none,        &[0x09]),
            ("Shift-Tab", egui::Key::Tab, shift,       b"\x1b[Z"),
        ];
        for (name, key, m, want) in cases {
            assert_eq!(
                super::key_to_pty(*key, *m).as_deref(),
                Some(*want),
                "{name} is not encoded as the TUI expects",
            );
        }
    }

    /// With autowrap off, filling the last row must not scroll.
    ///
    /// This is the bug behind two frames appearing on screen at once: a
    /// full-screen renderer writes the bottom-right cell, the terminal wraps and
    /// line-feeds, everything shifts up a row, and the next frame lands at
    /// absolute positions over the top of the displaced one.
    #[test]
    fn with_autowrap_off_filling_the_last_cell_does_not_scroll() {
        let mut g = Grid::with_size(4, 10);
        filled(&mut g, 20, "history");
        let scrollback_before = g.scrollback.len();

        g.process("\x1b[?7l");
        // Park on the last row and write exactly enough to fill it.
        g.process("\x1b[4;1H");
        g.process(&"x".repeat(10));

        assert_eq!(
            g.scrollback.len(), scrollback_before,
            "filling the last row must not scroll with autowrap off",
        );
        // One more character overwrites the last cell rather than wrapping.
        g.process("y");
        assert_eq!(
            g.scrollback.len(), scrollback_before,
            "and neither does writing past it",
        );
    }

    /// With autowrap on — the default — wrapping still works, or ordinary
    /// shell output would stop flowing.
    #[test]
    fn autowrap_on_by_default_still_wraps() {
        let mut g = Grid::with_size(4, 10);
        g.process("\x1b[1;1H");
        g.process(&"x".repeat(15));
        // 15 characters into a 10-column row must have moved to a second row.
        assert!(g.cursor().0 > 0, "wrapped onto the next row");
    }

    #[test]
    fn autowrap_can_be_turned_back_on() {
        let mut g = Grid::with_size(4, 10);
        g.process("\x1b[?7l");
        g.process("\x1b[?7h");
        g.process("\x1b[1;1H");
        g.process(&"x".repeat(15));
        assert!(g.cursor().0 > 0, "wrapping restored");
    }

    /// The exact bytes forge-tui's frame writer emits, captured from a real pty.
    ///
    /// This is the jitter the user reported: ink erases scrollback (`3J`) and
    /// reprints the transcript, and the scrollbar jumps as history shrinks and
    /// regrows. Wrapped in a synchronized update, the wipe and the reprint must
    /// present as a single frame — exactly one version bump for the whole
    /// sequence, so the renderer never shows the emptied intermediate state.
    #[test]
    fn forge_tui_repaint_presents_as_one_frame() {
        let mut g = Grid::with_size(10, 40);
        filled(&mut g, 30, "transcript");
        let before = g.version();

        g.process("\x1b[?2026h\x1b[2J\x1b[3J\x1b[HREPAINT\x1b[?2026l");

        assert_eq!(g.version(), before + 1,
                   "the wipe and reprint must present once, not twice");
        assert!(g.sync_update.is_none(), "update closed");
    }

    /// The same frame split across pty reads — the common case, since a repaint
    /// of a long transcript exceeds a single chunk. Nothing may present until
    /// the closing `2026l` arrives.
    #[test]
    fn a_split_repaint_withholds_until_complete() {
        let mut g = Grid::with_size(10, 40);
        filled(&mut g, 30, "transcript");
        let before = g.version();

        g.process("\x1b[?2026h\x1b[2J\x1b[3J\x1b[H");
        assert_eq!(g.version(), before, "nothing may present mid-frame");
        g.process("first half ");
        assert_eq!(g.version(), before, "still nothing");
        g.process("second half\x1b[?2026l");
        assert_eq!(g.version(), before + 1, "one frame once complete");
    }
}

#[cfg(test)]
mod shrink_tests {
    use super::Grid;

    /// What the viewport says, top to bottom, trailing blanks trimmed.
    fn viewport(g: &Grid) -> Vec<String> {
        g.viewport.iter()
            .map(|r| r.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect()
    }

    fn scrollback(g: &Grid) -> Vec<String> {
        g.scrollback.iter()
            .map(|r| r.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect()
    }

    /// The reported bug. A remote terminal's grid opens taller than the panel it
    /// is drawn in, the shell prints its prompt at the top, and then the grid is
    /// fitted to the panel. Shrinking used to scroll the top away regardless, so
    /// the prompt went into scrollback and the terminal opened showing nothing
    /// at all — you had to scroll up to find your own shell.
    #[test]
    fn fitting_a_tall_grid_to_a_short_panel_keeps_what_was_printed() {
        let mut g = Grid::with_size(50, 80);
        g.process("sysadmin@spark:~$ ");

        g.resize(20, 80);

        assert!(viewport(&g).iter().any(|l| l.contains("sysadmin@spark")),
                "the prompt left the viewport: {:?}", viewport(&g));
        assert!(scrollback(&g).is_empty(),
                "blank space was filed as history: {:?}", scrollback(&g));
        // And the cursor is still on the prompt's line, so what is typed next
        // appears where the prompt is.
        assert_eq!(g.cursor().0, 0);
    }

    /// Content that genuinely does not fit still scrolls off the top, which is
    /// what keeps the cursor in view — the old behaviour, for the case it was
    /// right for.
    #[test]
    fn content_taller_than_the_new_size_still_scrolls_off() {
        let mut g = Grid::with_size(10, 40);
        for i in 0..10 { g.process(&format!("line {i}\r\n")); }
        // Ten lines printed into ten rows: the grid has already scrolled once,
        // so the cursor sits on the last row with content above it.
        let before = g.scrollback.len();

        g.resize(4, 40);

        assert!(g.scrollback.len() > before, "nothing scrolled off a grid that had to shrink");
        let view = viewport(&g);
        assert_eq!(view.len(), 4);
        assert!(view.iter().any(|l| l.contains("line 9")),
                "the newest line is not visible: {view:?}");
    }

    /// Shrinking to fit, then growing back, restores what was hidden rather than
    /// leaving a gap — the two halves have to agree about where rows go.
    #[test]
    fn shrinking_then_growing_is_not_lossy() {
        let mut g = Grid::with_size(30, 40);
        for i in 0..8 { g.process(&format!("out {i}\r\n")); }
        let before = viewport(&g).into_iter().filter(|l| !l.is_empty()).collect::<Vec<_>>();

        g.resize(6, 40);
        g.resize(30, 40);

        let after = viewport(&g).into_iter().filter(|l| !l.is_empty()).collect::<Vec<_>>();
        assert_eq!(after, before, "a round trip through a smaller size lost lines");
    }

    /// A blank row a program painted a background onto is not empty space — a
    /// highlighted band with no text in it was drawn on purpose.
    #[test]
    fn a_coloured_blank_row_is_not_free_space() {
        let mut g = Grid::with_size(6, 20);
        // Green background, one blank line, back to normal.
        g.process("first\r\n\x1b[42m     \x1b[0m\r\n");
        let filled_rows = g.viewport.iter()
            .filter(|r| r.iter().any(|c| c.bg.is_some())).count();
        assert_eq!(filled_rows, 1, "precondition: one row carries a background");

        g.resize(3, 20);

        assert!(
            g.viewport.iter().any(|r| r.iter().any(|c| c.bg.is_some())),
            "a deliberately painted row was discarded as blank",
        );
    }
}

#[cfg(test)]
mod column_width_tests {
    use super::mono_advance;

    /// A shell prompt's worth of text has to end where the cursor is drawn.
    ///
    /// The reported symptom was a gap between the end of a remote prompt and the
    /// block cursor. It was the terminal placing the cursor at `column ×
    /// glyph_width(' ')` while the text beside it was placed by the galley: at
    /// 13pt those differ by about a quarter of a pixel per character, which is
    /// nine pixels — three quarters of a character — by the end of
    /// `sysadmin@spark-9f0d:~/Biophysical_NN$ `. At 14pt the error runs the other
    /// way and the cursor sits inside the text instead.
    #[test]
    fn a_column_is_the_width_the_text_is_actually_drawn_at() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});

        for size in [11.0_f32, 12.0, 13.0, 14.0, 16.0] {
            let font = egui::FontId::monospace(size);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0), egui::vec2(900.0, 300.0),
                )),
                ..Default::default()
            };
            let mut advance = 0.0_f32;
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    advance = mono_advance(ui, &font);
                });
            });

            let text = "sysadmin@spark-9f0d:~/Biophysical_NN$ ";
            let drawn = ctx.fonts(|f| {
                f.layout_no_wrap(text.to_string(), font.clone(), egui::Color32::WHITE)
            }).size().x;
            let by_column = text.chars().count() as f32 * advance;

            // Within a pixel over a whole prompt: the cursor lands where the text
            // ends, which is all this has to be.
            assert!(
                (drawn - by_column).abs() < 1.0,
                "at {size}pt a {}-character prompt drifts {:.2}px \
                 (text ends at {drawn:.2}, column maths says {by_column:.2})",
                text.chars().count(), drawn - by_column,
            );
        }
    }

    /// And the naive measurement really is wrong, so this is testing something.
    #[test]
    fn the_glyph_advance_alone_would_drift() {
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |_| {});
        let font = egui::FontId::monospace(13.0);
        let text = "sysadmin@spark-9f0d:~/Biophysical_NN$ ";

        let space = ctx.fonts(|f| f.glyph_width(&font, ' '));
        let drawn = ctx.fonts(|f| {
            f.layout_no_wrap(text.to_string(), font.clone(), egui::Color32::WHITE)
        }).size().x;
        let drift = drawn - text.chars().count() as f32 * space;
        assert!(
            drift.abs() > 2.0,
            "the space advance no longer drifts ({drift:.2}px) — if egui stopped \
             snapping advances, this test has nothing left to say",
        );
    }
}
