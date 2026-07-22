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

#[derive(PartialEq, Clone, Copy)]
enum PState { Ground, Esc, Csi, Osc, Charset }

pub struct Grid {
    /// Lines scrolled off the top of the viewport, oldest first. Never
    /// touched by cursor movement/erase — only by scrolling and resize.
    scrollback:   VecDeque<Vec<Cell>>,
    /// The current fixed-size screen; what cursor addressing/erase target.
    viewport:     Vec<Vec<Cell>>,
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
            viewport:   vec![vec![Cell::blank(); cols]; rows],
            rows, cols, cur_row: 0, cur_col: 0,
            cursor_visible: true,
            current_fg: default_fg(),
            current_bg: None,
            dim: false,
            parse_st: PState::Ground, parse_params: String::new(),
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
        GridSnapshot {
            rows: self.rows,
            cols: self.cols,
            cur_row: self.cur_row,
            cur_col: self.cur_col,
            cursor_visible: self.cursor_visible,
            cells: self.viewport.iter()
                .map(|row| row.iter().map(CellSnap::from).collect())
                .collect(),
            scrollback: self.scrollback.iter()
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
            for row in self.viewport.iter_mut().chain(self.scrollback.iter_mut()) {
                row.resize(new_cols, Cell::blank());
            }
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
                self.viewport.insert(0, row);
                pulled += 1;
            }
            self.cur_row += pulled;
            for _ in 0..(grow - pulled) {
                self.viewport.push(vec![Cell::blank(); self.cols]);
            }
        } else if new_rows < self.rows {
            let shrink = self.rows - new_rows;
            for _ in 0..shrink {
                if self.viewport.is_empty() { break; }
                let row = self.viewport.remove(0);
                self.scrollback.push_back(row);
            }
            self.cur_row = self.cur_row.saturating_sub(shrink);
        }
        self.rows = new_rows;
        self.cur_row = self.cur_row.min(self.rows - 1);
        self.trim_scrollback();
    }

    fn trim_scrollback(&mut self) {
        let before = self.scrollback.len();
        while self.scrollback.len() > MAX_SCROLLBACK {
            self.scrollback.pop_front();
        }
        if self.scrollback.len() != before { self.scrollback_version += 1; }
    }

    /// Shift the viewport up by one line, pushing the departing top line
    /// into scrollback. Does not move the cursor (matches real terminals'
    /// "scroll" primitive, used both by line-feed-at-bottom and ESC[S).
    fn scroll_up_one(&mut self) {
        let top = self.viewport.remove(0);
        self.scrollback.push_back(top);
        self.scrollback_version += 1;
        self.viewport.push(vec![Cell::blank(); self.cols]);
        self.trim_scrollback();
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
            self.cur_col = 0;
            self.line_feed();
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
                if mode == 3 { self.scrollback.clear(); }
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
            if rest.split(';').any(|p| p == "25") {
                match cmd {
                    'h' => self.cursor_visible = true,
                    'l' => self.cursor_visible = false,
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
        self.version += 1;
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
    }

    /// Cursor position within the *viewport* — the cursor is always in the
    /// viewport, never in scrollback history, so this indexes directly into
    /// `to_viewport_layout_job`'s own rows (no scrollback offset needed).
    pub fn cursor(&self) -> (usize, usize) { (self.cur_row, self.cur_col) }

    /// Full contents (scrollback + current viewport) as plain text, one
    /// trimmed line per row — used by the "Copy" context-menu action.
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

pub struct Terminal {
    // Deferred until first draw() when we know the real panel size.
    // `None` for a terminal constructed via `reattach` — already running.
    pending:   Option<std::path::PathBuf>,
    backend:   Option<PtyBackend>,
    /// This session's id in the pty-host daemon, if backed by one — `None`
    /// for the direct-PTY fallback, which has no cross-restart identity.
    pty_id:    Option<u32>,
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
            grid:          Arc::new(Mutex::new(Grid::new())),
            cwd:           cwd.to_path_buf(),
            focused:       false,
            last_rows:     0,
            last_cols:     0,
            pending_size:  None,
            pending_input: Vec::new(),
            last_input_at: None,
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
            grid,
            cwd:       cwd.to_path_buf(),
            focused:       false,
            last_rows: rows,
            last_cols: cols,
            pending_size:  None,
            pending_input: Vec::new(),
            last_input_at: None,
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
        let char_w  = ui.fonts(|f| {
            f.layout_no_wrap("a".repeat(40), font_id.clone(), egui::Color32::WHITE)
                .rect.width() / 40.0
        });

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
            .id_salt("term_scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
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
                                self.write_bytes(text.as_bytes());
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
                        let bytes = s.as_bytes().to_vec();
                        self.pending_input.push(bytes.clone());
                        self.write_bytes(&bytes);
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

pub fn key_to_pty(key: egui::Key, m: egui::Modifiers) -> Option<Vec<u8>> {
    // Alt/Option+Enter — the standard "insert a literal newline instead of
    // submitting" convention most readline-based line editors (including
    // ink, which Claude Code is built on) respond to. Previously
    // unhandled: Enter always sent plain `\r` regardless of modifiers, so
    // Option+Enter behaved identically to plain Enter.
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
    if m.ctrl {
        return match key {
            egui::Key::A => Some(b"\x01".to_vec()), egui::Key::B => Some(b"\x02".to_vec()),
            egui::Key::C => Some(b"\x03".to_vec()), egui::Key::D => Some(b"\x04".to_vec()),
            egui::Key::E => Some(b"\x05".to_vec()), egui::Key::K => Some(b"\x0b".to_vec()),
            egui::Key::L => Some(b"\x0c".to_vec()), egui::Key::N => Some(b"\x0e".to_vec()),
            egui::Key::P => Some(b"\x10".to_vec()), egui::Key::R => Some(b"\x12".to_vec()),
            egui::Key::U => Some(b"\x15".to_vec()), egui::Key::W => Some(b"\x17".to_vec()),
            egui::Key::Z => Some(b"\x1a".to_vec()),
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
