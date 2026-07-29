// SPDX-License-Identifier: Apache-2.0
//! Transcript, scrolling and input, as a state machine.
//!
//! Deliberately free of I/O: `update` takes an event and mutates state, `view`
//! draws into a [`Screen`]. Neither touches a terminal, so the scroll and layout
//! rules — the parts most likely to be subtly wrong — are testable directly.

use crate::markdown::{self, Line};
use crate::screen::{Screen, Style};

/// Who said it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Agent,
    System,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::User   => "› ",
            Role::Agent  => "  ",
            Role::System => "! ",
        }
    }
    fn style(self) -> Style {
        match self {
            Role::User   => Style::fg(75).bold(),
            Role::Agent  => Style::default(),
            Role::System => Style::fg(215).dim(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Message {
    pub role: Role,
    pub body: String,
}

/// What the event loop should do after an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    Quit,
}

/// Input, already decoded from terminal bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Char(char),
    Paste(String),
    Backspace,
    Enter,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Quit,
    Resize(usize, usize),
}

pub struct App {
    messages: Vec<Message>,
    input:    String,
    /// Lines scrolled up from the bottom. 0 means pinned to the newest output.
    scroll:   usize,
    /// Cached wrap of the transcript, and the width it was wrapped for.
    cache:    Option<(usize, Vec<Line>)>,
}

impl App {
    pub fn new() -> Self {
        Self {
            messages: Vec::new(),
            input:    String::new(),
            scroll:   0,
            cache:    None,
        }
    }

    pub fn push(&mut self, role: Role, body: impl Into<String>) {
        self.messages.push(Message { role, body: body.into() });
        self.cache = None;
        // New output pins the view to the bottom, which is what a reader
        // following along expects. Scrolling up is an explicit choice to stop
        // following, and is preserved by the branch in `scroll_by`.
        self.scroll = 0;
    }

    pub fn input(&self) -> &str { &self.input }
    pub fn scroll(&self) -> usize { self.scroll }
    pub fn messages(&self) -> &[Message] { &self.messages }

    pub fn update(&mut self, input: Input, screen: &Screen) -> Outcome {
        let page = self.viewport_rows(screen).max(1);
        match input {
            Input::Quit => return Outcome::Quit,
            Input::Char(c) => {
                self.input.push(c);
                self.scroll = 0;
            }
            Input::Paste(text) => {
                // Newlines in a paste would submit partway through, so they
                // become spaces; a multi-line composer is out of scope here.
                self.input.push_str(&text.replace(['\n', '\r'], " "));
                self.scroll = 0;
            }
            Input::Backspace => {
                // Pop a whole grapheme: backspacing an emoji should not leave
                // half of it behind.
                use unicode_segmentation::UnicodeSegmentation;
                if let Some(last) = self.input.graphemes(true).next_back() {
                    let keep = self.input.len() - last.len();
                    self.input.truncate(keep);
                }
            }
            Input::Enter => {
                let text = self.input.trim().to_string();
                self.input.clear();
                if !text.is_empty() {
                    self.push(Role::User, text);
                }
            }
            Input::Up       => self.scroll_by(1, screen),
            Input::Down     => self.scroll_by(-1, screen),
            Input::PageUp   => self.scroll_by(page as isize, screen),
            Input::PageDown => self.scroll_by(-(page as isize), screen),
            Input::Home     => self.scroll = self.max_scroll(screen),
            Input::End      => self.scroll = 0,
            Input::Resize(..) => self.cache = None,
        }
        Outcome::Continue
    }

    /// Rows available to the transcript: everything but the input line and the
    /// rule above it.
    fn viewport_rows(&self, screen: &Screen) -> usize {
        screen.rows().saturating_sub(2)
    }

    fn scroll_by(&mut self, delta: isize, screen: &Screen) {
        let max = self.max_scroll(screen);
        let next = self.scroll as isize + delta;
        self.scroll = next.clamp(0, max as isize) as usize;
    }

    fn max_scroll(&self, screen: &Screen) -> usize {
        let total = self.lines(screen.cols()).len();
        total.saturating_sub(self.viewport_rows(screen))
    }

    /// The whole transcript, wrapped. Cached because wrapping every message on
    /// every keystroke is wasted work; invalidated by new output or a resize.
    fn lines(&self, cols: usize) -> Vec<Line> {
        if let Some((cached_cols, lines)) = &self.cache {
            if *cached_cols == cols {
                return lines.clone();
            }
        }
        let mut out = Vec::new();
        for (i, msg) in self.messages.iter().enumerate() {
            if i > 0 {
                out.push(Line::default());
            }
            let label = msg.role.label();
            let body_cols = cols.saturating_sub(label.len());
            for (j, line) in markdown::render(&msg.body, body_cols).into_iter().enumerate() {
                let lead = if j == 0 { label.to_string() } else { " ".repeat(label.len()) };
                let mut spans = vec![crate::markdown::Span {
                    text:  lead,
                    style: msg.role.style(),
                }];
                spans.extend(line.spans);
                out.push(Line { spans });
            }
        }
        out
    }

    /// Wrap and cache. Separate from `lines` so `view` can populate the cache
    /// while the read-only helpers stay `&self`.
    fn lines_cached(&mut self, cols: usize) -> Vec<Line> {
        if self.cache.as_ref().is_none_or(|(c, _)| *c != cols) {
            let lines = self.lines(cols);
            self.cache = Some((cols, lines));
        }
        self.cache.as_ref().map(|(_, l)| l.clone()).unwrap_or_default()
    }

    pub fn view(&mut self, screen: &mut Screen) {
        screen.begin_frame();
        let cols = screen.cols();
        let rows = screen.rows();
        if rows == 0 || cols == 0 {
            return;
        }
        let viewport = self.viewport_rows(screen);
        let lines = self.lines_cached(cols);

        // Show the window ending `scroll` lines above the newest output. When
        // the transcript is shorter than the viewport it sits at the top.
        let end = lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(viewport);
        for (row, line) in lines[start..end].iter().enumerate() {
            let mut col = 0;
            for span in &line.spans {
                col = screen.put(row, col, &span.text, span.style);
            }
        }

        // A rule, with a hint when the view is detached from the bottom.
        if rows >= 2 {
            let rule_row = rows - 2;
            let hint = if self.scroll > 0 {
                format!(" {} lines below — End to follow ", self.scroll)
            } else {
                String::new()
            };
            let dim = Style::fg(238);
            let filled = screen.put(rule_row, 0, &"─".repeat(cols.saturating_sub(hint.len())), dim);
            screen.put(rule_row, filled, &hint, Style::fg(245).dim());
        }

        // Input line, with the cursor at the end of what has been typed.
        let prompt = "› ";
        let input_row = rows - 1;
        let col = screen.put(input_row, 0, prompt, Style::fg(75).bold());
        let visible = self.visible_input(cols.saturating_sub(col));
        let end_col = screen.put(input_row, col, &visible, Style::default());
        screen.set_cursor(input_row, end_col.min(cols.saturating_sub(1)));
    }

    /// The tail of the input that fits, so a long line scrolls horizontally
    /// rather than being cut off at the point the user is typing.
    fn visible_input(&self, budget: usize) -> String {
        use unicode_segmentation::UnicodeSegmentation;
        if crate::width::str_width(&self.input) <= budget {
            return self.input.clone();
        }
        let mut kept: Vec<&str> = Vec::new();
        let mut w = 0;
        for cluster in self.input.graphemes(true).rev() {
            let cw = crate::width::cluster_width(cluster);
            if w + cw > budget {
                break;
            }
            kept.push(cluster);
            w += cw;
        }
        kept.reverse();
        kept.concat()
    }
}

impl Default for App {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with(n: usize) -> (App, Screen) {
        let mut app = App::new();
        for i in 0..n {
            app.push(Role::Agent, format!("message number {i}"));
        }
        (app, Screen::new(40, 10))
    }

    #[test]
    fn typing_accumulates_and_backspace_removes() {
        let (mut app, screen) = app_with(0);
        for c in "hi".chars() {
            app.update(Input::Char(c), &screen);
        }
        assert_eq!(app.input(), "hi");
        app.update(Input::Backspace, &screen);
        assert_eq!(app.input(), "h");
    }

    /// Backspacing must remove a whole glyph. Truncating bytes would leave a
    /// broken sequence and render as a replacement character.
    #[test]
    fn backspace_removes_a_whole_grapheme() {
        let (mut app, screen) = app_with(0);
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        for c in family.chars() {
            app.update(Input::Char(c), &screen);
        }
        app.update(Input::Backspace, &screen);
        assert_eq!(app.input(), "", "the entire emoji is gone, not one codepoint");
    }

    #[test]
    fn backspace_on_empty_input_is_harmless() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Backspace, &screen);
        assert_eq!(app.input(), "");
    }

    #[test]
    fn enter_submits_and_clears() {
        let (mut app, screen) = app_with(0);
        for c in "hello".chars() {
            app.update(Input::Char(c), &screen);
        }
        app.update(Input::Enter, &screen);
        assert_eq!(app.input(), "");
        assert_eq!(app.messages().len(), 1);
        assert_eq!(app.messages()[0].role, Role::User);
    }

    #[test]
    fn enter_on_blank_input_submits_nothing() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Char(' '), &screen);
        app.update(Input::Enter, &screen);
        assert!(app.messages().is_empty(), "whitespace is not a message");
    }

    /// A pasted newline must not submit mid-paste.
    #[test]
    fn a_multiline_paste_does_not_submit() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Paste("one\ntwo".into()), &screen);
        assert!(app.messages().is_empty(), "paste never submits");
        assert_eq!(app.input(), "one two");
    }

    #[test]
    fn scrolling_is_clamped_at_both_ends() {
        let (mut app, screen) = app_with(30);
        for _ in 0..500 {
            app.update(Input::Up, &screen);
        }
        let max = app.scroll();
        assert!(max > 0, "there is history to scroll into");
        app.update(Input::Up, &screen);
        assert_eq!(app.scroll(), max, "cannot scroll past the oldest line");

        for _ in 0..500 {
            app.update(Input::Down, &screen);
        }
        assert_eq!(app.scroll(), 0, "cannot scroll past the newest line");
    }

    #[test]
    fn a_short_transcript_cannot_scroll() {
        let (mut app, screen) = app_with(1);
        app.update(Input::Up, &screen);
        assert_eq!(app.scroll(), 0, "nothing to scroll");
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let (mut app, screen) = app_with(30);
        app.update(Input::Home, &screen);
        assert!(app.scroll() > 0);
        app.update(Input::End, &screen);
        assert_eq!(app.scroll(), 0);
    }

    #[test]
    fn page_keys_move_about_a_screen() {
        let (mut app, screen) = app_with(40);
        app.update(Input::PageUp, &screen);
        let after = app.scroll();
        assert!(after >= screen.rows() - 2, "at least a viewport, got {after}");
    }

    /// New output should pull the view back to the bottom, so a reader following
    /// along is not left behind by an agent that keeps talking.
    #[test]
    fn new_output_returns_to_the_bottom() {
        let (mut app, screen) = app_with(30);
        app.update(Input::Home, &screen);
        assert!(app.scroll() > 0);
        app.push(Role::Agent, "something new");
        assert_eq!(app.scroll(), 0);
    }

    #[test]
    fn quit_is_reported() {
        let (mut app, screen) = app_with(0);
        assert_eq!(app.update(Input::Quit, &screen), Outcome::Quit);
    }

    /// The layout invariant: drawing a full transcript must fit the screen and
    /// produce exactly one frame.
    #[test]
    fn view_draws_one_frame_and_fits() {
        let (mut app, mut screen) = app_with(50);
        app.view(&mut screen);
        let mut sink = Vec::new();
        screen.flush(&mut sink).unwrap();
        let text = String::from_utf8_lossy(&sink);
        assert_eq!(text.matches(crate::screen::SYNC_BEGIN).count(), 1);
        assert!(!text.contains("\x1b[3J"), "never erases scrollback");
    }

    #[test]
    fn view_survives_a_degenerate_screen() {
        let mut app = App::new();
        app.push(Role::Agent, "text that will not fit anywhere");
        for (cols, rows) in [(0usize, 0usize), (1, 1), (2, 1), (1, 3), (3, 2)] {
            let mut screen = Screen::new(cols, rows);
            app.view(&mut screen); // must not panic
            let mut sink = Vec::new();
            screen.flush(&mut sink).unwrap();
        }
    }

    #[test]
    fn a_resize_rewraps_the_transcript() {
        let mut app = App::new();
        app.push(Role::Agent, "a message long enough that its wrapping depends on width");
        let narrow = Screen::new(20, 10);
        let wide = Screen::new(70, 10);
        let narrow_lines = app.lines_cached(narrow.cols()).len();
        let wide_lines = app.lines_cached(wide.cols()).len();
        assert!(narrow_lines > wide_lines, "narrower means more lines");
    }

    /// A long input line scrolls horizontally so the caret stays visible.
    #[test]
    fn long_input_shows_its_tail() {
        let (mut app, screen) = app_with(0);
        for c in "0123456789".repeat(10).chars() {
            app.update(Input::Char(c), &screen);
        }
        let visible = app.visible_input(10);
        assert_eq!(crate::width::str_width(&visible), 10);
        assert!(app.input().ends_with(&visible), "shows the end, not the start");
    }

    /// Everything drawn must respect the measured width, or the renderer and the
    /// layout have diverged — the original overlap bug.
    #[test]
    fn every_transcript_line_fits_the_width() {
        let mut app = App::new();
        app.push(Role::Agent, "日本語のテキスト with **bold** and `code` and \
                               \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} emoji");
        app.push(Role::User, "- a bullet that runs on long enough to wrap around");
        for cols in [10usize, 16, 24, 40, 80] {
            for line in app.lines_cached(cols) {
                assert!(
                    line.width() <= cols,
                    "line {:?} is {} cells, budget {cols}",
                    line.plain(), line.width(),
                );
            }
        }
    }
}
