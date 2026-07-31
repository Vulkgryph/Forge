// SPDX-License-Identifier: Apache-2.0
//! The application: a [`Session`] plus a viewport onto it, and the input line.
//!
//! Still a pure state machine — [`App::update`] takes one decoded [`Input`] and
//! returns what to send to the agent, and [`App::view`] draws into a [`Screen`].
//! Neither touches a terminal or the agent, so scrolling, wrapping and the key
//! routing around a pending prompt are all testable directly.

use forge_agent_proto::ClientMessage;

use crate::markdown::{self, Line, Span};
use crate::screen::{Screen, Style};
use crate::session::{Effect, EntryKind, Pending, PermissionMode, Session};

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
    /// Ask the agent to stop the current turn.
    Interrupt,
    Quit,
    Resize(usize, usize),
}

/// Palette, in one place.
mod palette {
    pub const USER:     u8 = 75;
    pub const TOOL:     u8 = 108;
    pub const ERROR:    u8 = 174;
    pub const SYSTEM:   u8 = 245;
    pub const THOUGHT:  u8 = 240;
    pub const SUBAGENT: u8 = 141;
    pub const RULE:     u8 = 238;
    pub const PROMPT:   u8 = 75;
}

pub struct App {
    session: Session,
    input:   String,
    /// Lines scrolled up from the newest output. 0 means following along.
    scroll:  usize,
    /// Cached wrap of the transcript: the width it was built for, how many
    /// entries it covered, and the lines themselves.
    cache:   Option<(usize, usize, Vec<Line>)>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self { session: Session::new(), input: String::new(), scroll: 0, cache: None }
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        // Any change to the transcript invalidates the wrap.
        self.cache = None;
        &mut self.session
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    pub fn scroll(&self) -> usize {
        self.scroll
    }

    /// New agent output pulls the view back to the bottom, so a reader following
    /// along is not left behind. Scrolling up is an explicit choice to stop
    /// following and is preserved until they return.
    pub fn follow_tail(&mut self) {
        self.scroll = 0;
        self.cache = None;
    }

    pub fn update(&mut self, input: Input, screen: &Screen) -> (Outcome, Vec<Effect>) {
        let page = self.viewport_rows(screen).max(1);

        match input {
            Input::Quit => return (Outcome::Quit, Vec::new()),

            Input::Interrupt => {
                if self.session.activity.is_busy() {
                    return (
                        Outcome::Continue,
                        vec![Effect::Send(ClientMessage::CancelRun)],
                    );
                }
            }

            Input::Char(c) => {
                // While the agent is waiting on a yes/no decision, those keys
                // are the answer rather than text — otherwise the only way to
                // approve would be to type into a prompt that is not listening.
                if let Some(effects) = self.answer_with_key(c) {
                    return (Outcome::Continue, effects);
                }
                self.input.push(c);
                self.follow_tail();
            }

            Input::Paste(text) => {
                // Newlines would submit partway through a paste; a multi-line
                // composer is out of scope here.
                self.input.push_str(&text.replace(['\n', '\r'], " "));
                self.follow_tail();
            }

            Input::Backspace => {
                // Remove a whole grapheme: backspacing an emoji must not leave
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
                if text.is_empty() {
                    return (Outcome::Continue, Vec::new());
                }

                // A prompt taking free text consumes the line.
                if self.session.pending.is_some() {
                    // Cloned because a prompt that does *not* take text leaves
                    // the line to be sent as an ordinary message below.
                    let effects = self.session.reply(text.clone());
                    if !effects.is_empty() {
                        self.follow_tail();
                        return (Outcome::Continue, effects);
                    }
                    // Nothing consumed it (an approval, say) — fall through and
                    // treat the text as a message rather than swallowing it.
                }

                self.session_mut().push_user(&text);
                self.follow_tail();
                return (
                    Outcome::Continue,
                    vec![Effect::Send(ClientMessage::SendMessage { content: text })],
                );
            }

            Input::Up       => self.scroll_by(1, screen),
            Input::Down     => self.scroll_by(-1, screen),
            Input::PageUp   => self.scroll_by(page as isize, screen),
            Input::PageDown => self.scroll_by(-(page as isize), screen),
            Input::Home     => self.scroll = self.max_scroll(screen),
            Input::End      => self.scroll = 0,
            Input::Resize(..) => self.cache = None,
        }

        (Outcome::Continue, Vec::new())
    }

    /// Route a single keypress to a pending yes/no decision, if there is one.
    ///
    /// Only while the input line is empty. Otherwise the letters y, n and a
    /// could not be typed at all with a prompt open — "wait" would trigger
    /// "always approve" on its second keystroke. Starting to type is taken as
    /// choosing to write a reply instead of picking an option.
    ///
    /// Returns `None` when the key is just text.
    fn answer_with_key(&mut self, c: char) -> Option<Vec<Effect>> {
        if !self.input.is_empty() {
            return None;
        }
        match self.session.pending {
            Some(Pending::Approval { .. }) => match c.to_ascii_lowercase() {
                'y' => Some(self.session.approve(false)),
                'a' => Some(self.session.approve(true)),
                'n' => Some(self.session.deny("denied by the user")),
                _ => None,
            },
            Some(Pending::Plan { .. }) => match c.to_ascii_lowercase() {
                'y' => Some(self.session.approve_plan(false)),
                'c' => Some(self.session.approve_plan(true)),
                _ => None,
            },
            _ => None,
        }
    }

    /// Rows available to the transcript: everything but the status line, the
    /// rule, and the input line.
    fn viewport_rows(&self, screen: &Screen) -> usize {
        screen.rows().saturating_sub(3)
    }

    fn scroll_by(&mut self, delta: isize, screen: &Screen) {
        let max = self.max_scroll(screen) as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }

    fn max_scroll(&self, screen: &Screen) -> usize {
        self.wrap(screen.cols()).len().saturating_sub(self.viewport_rows(screen))
    }

    /// The transcript, wrapped to `cols`.
    ///
    /// Recomputed when the width changes or entries are added. The entry count
    /// is part of the key because streaming mutates the last entry in place, so
    /// a count alone would not notice — hence the deliberate exclusion of the
    /// tail entry from the cache, below.
    fn wrap(&self, cols: usize) -> Vec<Line> {
        if let Some((cached_cols, count, lines)) = &self.cache {
            if *cached_cols == cols && *count == self.session.entries().len() {
                return lines.clone();
            }
        }
        self.build_lines(cols)
    }

    fn build_lines(&self, cols: usize) -> Vec<Line> {
        let mut out = Vec::new();
        for (i, entry) in self.session.entries().iter().enumerate() {
            if i > 0 {
                out.push(Line::default());
            }
            let (prefix, style) = decorate(entry.kind, entry.success);
            let body_cols = cols.saturating_sub(prefix.chars().count());

            // Tool output and results are preformatted; running them through
            // the markdown parser would reflow a diff or a stack trace.
            let rendered = match entry.kind {
                EntryKind::ToolOutput | EntryKind::ToolResult => entry
                    .content
                    .lines()
                    .map(|l| Line { spans: vec![Span { text: l.to_string(), style }] })
                    .collect(),
                EntryKind::Thought => {
                    let secs = entry.duration.map(|d| d.as_secs()).unwrap_or(0);
                    let head = format!("thought for {secs}s");
                    let mut lines = vec![Line {
                        spans: vec![Span { text: head, style }],
                    }];
                    lines.extend(markdown::render(&entry.content, body_cols));
                    lines
                }
                _ => markdown::render(&entry.content, body_cols),
            };

            for (j, line) in rendered.into_iter().enumerate() {
                let lead = if j == 0 {
                    prefix.to_string()
                } else {
                    " ".repeat(prefix.chars().count())
                };
                let mut spans = Vec::new();
                if !lead.is_empty() {
                    spans.push(Span { text: lead, style });
                }
                spans.extend(line.spans);
                out.push(Line { spans });
            }
        }
        out
    }

    /// Wrap and cache.
    fn wrap_cached(&mut self, cols: usize) -> Vec<Line> {
        let count = self.session.entries().len();
        let fresh = match &self.cache {
            Some((c, n, _)) => *c != cols || *n != count,
            None => true,
        };
        if fresh {
            let lines = self.build_lines(cols);
            self.cache = Some((cols, count, lines));
        }
        self.cache.as_ref().map(|(_, _, l)| l.clone()).unwrap_or_default()
    }

    pub fn view(&mut self, screen: &mut Screen) {
        screen.begin_frame();
        let (cols, rows) = (screen.cols(), screen.rows());
        if cols == 0 || rows == 0 {
            return;
        }

        // Streaming rewrites the tail entry in place, so the cache cannot be
        // trusted while a turn is live. Rebuilding a wrap is cheap next to a
        // transcript that lags behind the output.
        let lines = if self.session.activity.is_busy() {
            self.cache = None;
            self.build_lines(cols)
        } else {
            self.wrap_cached(cols)
        };

        let viewport = self.viewport_rows(screen);
        let end = lines.len().saturating_sub(self.scroll);
        let start = end.saturating_sub(viewport);
        for (row, line) in lines[start..end].iter().enumerate() {
            let mut col = 0;
            for span in &line.spans {
                col = screen.put(row, col, &span.text, span.style);
            }
        }

        if rows >= 3 {
            self.draw_status(screen, rows - 3, cols);
        }
        if rows >= 2 {
            self.draw_rule(screen, rows - 2, cols);
        }
        self.draw_input(screen, rows - 1, cols);
    }

    fn draw_status(&self, screen: &mut Screen, row: usize, cols: usize) {
        let mut col = 0;

        if let Some(label) = self.session.activity.label() {
            col = screen.put(row, col, &format!("● {label}"), Style::fg(palette::USER));
        } else if self.session.connected {
            col = screen.put(row, col, "● ready", Style::fg(palette::SYSTEM).dim());
        } else {
            col = screen.put(row, col, "○ connecting", Style::fg(palette::SYSTEM).dim());
        }

        // Context use, when the agent has reported any.
        if self.session.usage.is_some() {
            let pct = (self.session.context_fraction() * 100.0).round() as u32;
            col = screen.put(row, col + 2, &format!("ctx {pct}%"),
                             Style::fg(palette::SYSTEM).dim());
        }

        if self.session.permission_mode == PermissionMode::AllowAll {
            col = screen.put(row, col + 2, "auto-approve", Style::fg(palette::ERROR));
        }
        if self.session.plan_mode {
            col = screen.put(row, col + 2, "plan", Style::fg(palette::SUBAGENT));
        }

        // Right-aligned model name, when there is room for it.
        let model = &self.session.model_name;
        if !model.is_empty() {
            let w = crate::width::str_width(model);
            if cols > col + w + 2 {
                screen.put(row, cols - w, model, Style::fg(palette::SYSTEM).dim());
            }
        }
    }

    fn draw_rule(&self, screen: &mut Screen, row: usize, cols: usize) {
        let hint = if self.scroll > 0 {
            format!(" {} lines below — End to follow ", self.scroll)
        } else {
            String::new()
        };
        let fill = cols.saturating_sub(hint.chars().count());
        let filled = screen.put(row, 0, &"─".repeat(fill), Style::fg(palette::RULE));
        screen.put(row, filled, &hint, Style::fg(palette::SYSTEM).dim());
    }

    fn draw_input(&self, screen: &mut Screen, row: usize, cols: usize) {
        // A pending decision replaces the prompt with what it is asking, so the
        // available keys are never a guess.
        let prompt = match &self.session.pending {
            Some(Pending::Approval { tool_name, .. }) => {
                format!("approve {tool_name}? [y]es [n]o [a]lways ")
            }
            Some(Pending::Plan { .. }) => "plan ready: [y]es [c]lear+yes, or type feedback ".into(),
            Some(Pending::Question { question, .. }) => format!("{question} "),
            Some(Pending::ProcessInput { prompt }) => format!("{prompt} "),
            Some(Pending::BackgroundInput { prompt, .. }) => format!("{prompt} "),
            Some(Pending::Rewind { summary, .. }) => format!("rewind: {summary} [y/n] "),
            None => "› ".to_string(),
        };
        let style = if self.session.pending.is_some() {
            Style::fg(palette::ERROR).bold()
        } else {
            Style::fg(palette::PROMPT).bold()
        };

        let col = screen.put(row, 0, &prompt, style);
        let visible = self.visible_input(cols.saturating_sub(col));
        let end = screen.put(row, col, &visible, Style::default());
        screen.set_cursor(row, end.min(cols.saturating_sub(1)));
    }

    /// The tail of the input that fits, so a long line scrolls horizontally
    /// rather than being cut off where the user is typing.
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

/// The prefix and style for an entry kind.
fn decorate(kind: EntryKind, success: Option<bool>) -> (&'static str, Style) {
    match kind {
        EntryKind::User      => ("› ", Style::fg(palette::USER).bold()),
        EntryKind::Assistant => ("  ", Style::default()),
        EntryKind::Reasoning => ("  ", Style::fg(palette::THOUGHT).dim()),
        EntryKind::Thought   => ("  ", Style::fg(palette::THOUGHT).dim()),
        EntryKind::ToolCall  => ("⏵ ", Style::fg(palette::TOOL)),
        EntryKind::ToolResult => match success {
            Some(false) => ("  ", Style::fg(palette::ERROR)),
            _ => ("  ", Style::fg(palette::SYSTEM).dim()),
        },
        EntryKind::ToolOutput => ("  ", Style::fg(palette::SYSTEM).dim()),
        EntryKind::System     => ("  ", Style::fg(palette::SYSTEM).dim()),
        EntryKind::Error      => ("✗ ", Style::fg(palette::ERROR)),
        EntryKind::PlanContent => ("  ", Style::default()),
        EntryKind::PlanStatus  => ("◆ ", Style::fg(palette::SUBAGENT)),
        EntryKind::SubagentHeader => ("  ", Style::fg(palette::SUBAGENT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agent_proto::{AgentMessage, Init};

    fn app_with(n: usize) -> (App, Screen) {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::Init(Box::new(Init {
            model_name: "Model".into(),
            model_id: "m-1".into(),
            max_context_tokens: 1000,
            ..Default::default()
        })));
        for i in 0..n {
            app.session_mut().push_system(format!("entry number {i}"));
        }
        (app, Screen::new(40, 12))
    }

    fn tool_request(name: &str, id: &str) -> AgentMessage {
        AgentMessage::ToolRequest {
            tool_name: name.into(),
            tool_args: "{}".into(),
            tool_id: id.into(),
            kind: "execute".into(),
            subagent_id: None,
            needs_approval: true,
        }
    }

    // ── Input ─────────────────────────────────────────────────────────────

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

    #[test]
    fn backspace_removes_a_whole_grapheme() {
        let (mut app, screen) = app_with(0);
        for c in "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".chars() {
            app.update(Input::Char(c), &screen);
        }
        app.update(Input::Backspace, &screen);
        assert_eq!(app.input(), "", "the whole emoji, not one codepoint");
    }

    #[test]
    fn enter_sends_the_message_and_records_it() {
        let (mut app, screen) = app_with(0);
        for c in "hello".chars() {
            app.update(Input::Char(c), &screen);
        }
        let (outcome, effects) = app.update(Input::Enter, &screen);
        assert_eq!(outcome, Outcome::Continue);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::SendMessage { content: "hello".into() })],
        );
        assert_eq!(app.input(), "");
        assert_eq!(
            app.session().entries().last().unwrap().kind,
            EntryKind::User,
            "the message appears before the reply",
        );
    }

    #[test]
    fn enter_on_blank_input_sends_nothing() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Char(' '), &screen);
        let (_, effects) = app.update(Input::Enter, &screen);
        assert!(effects.is_empty());
    }

    #[test]
    fn a_multiline_paste_does_not_submit() {
        let (mut app, screen) = app_with(0);
        let (_, effects) = app.update(Input::Paste("one\ntwo".into()), &screen);
        assert!(effects.is_empty(), "paste never submits");
        assert_eq!(app.input(), "one two");
    }

    // ── Interrupt ─────────────────────────────────────────────────────────

    #[test]
    fn interrupt_cancels_only_while_busy() {
        let (mut app, screen) = app_with(0);
        let (_, effects) = app.update(Input::Interrupt, &screen);
        assert!(effects.is_empty(), "nothing to cancel when idle");

        app.session_mut().apply(AgentMessage::Thinking);
        let (_, effects) = app.update(Input::Interrupt, &screen);
        assert_eq!(effects, vec![Effect::Send(ClientMessage::CancelRun)]);
    }

    // ── Approval key routing ──────────────────────────────────────────────

    /// With an approval pending, y/n/a are the answer. Otherwise there would be
    /// no way to approve — the prompt is not accepting text.
    #[test]
    fn approval_keys_answer_instead_of_typing() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));

        let (_, effects) = app.update(Input::Char('y'), &screen);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t1".into() })],
        );
        assert_eq!(app.input(), "", "the key was consumed, not typed");
    }

    #[test]
    fn a_denies_and_n_refuses_with_the_right_message() {
        let (mut app, screen) = app_with(0);

        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let (_, effects) = app.update(Input::Char('n'), &screen);
        assert!(matches!(
            effects.first(),
            Some(Effect::Send(ClientMessage::DenyAction { .. })),
        ));

        app.session_mut().apply(tool_request("shell_exec", "t2"));
        let (_, effects) = app.update(Input::Char('a'), &screen);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t2".into() })],
        );
        // "always" remembered it, so the next call goes straight through.
        let effects = app.session_mut().apply(tool_request("shell_exec", "t3"));
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t3".into() })],
        );
    }

    /// A key that is not one of the answers must still be typed, so a user can
    /// write feedback where feedback is accepted.
    #[test]
    fn other_keys_still_type_while_a_prompt_is_open() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        app.update(Input::Char('z'), &screen);
        assert_eq!(app.input(), "z");
    }

    /// Once the user has started typing, the answer keys are letters again —
    /// otherwise a word like "wait" would fire "always approve" on its second
    /// keystroke, silently granting a permission nobody chose.
    #[test]
    fn answer_keys_are_ordinary_letters_once_typing_has_started() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));

        let mut effects_seen = Vec::new();
        for c in "wait a moment".chars() {
            let (_, effects) = app.update(Input::Char(c), &screen);
            effects_seen.extend(effects);
        }
        assert!(
            effects_seen.is_empty(),
            "typing must not approve anything: {effects_seen:?}",
        );
        assert_eq!(app.input(), "wait a moment");
        assert!(app.session().pending.is_some(), "still waiting on the user");
    }

    /// The first keystroke on an empty line is still the shortcut.
    #[test]
    fn an_answer_key_on_an_empty_line_still_answers() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let (_, effects) = app.update(Input::Char('y'), &screen);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t1".into() })],
        );
    }

    #[test]
    fn approval_keys_do_nothing_when_no_approval_is_pending() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Char('y'), &screen);
        assert_eq!(app.input(), "y", "just a letter");
    }

    /// Typed text answers a question rather than being sent as a new message,
    /// which would leave the agent still waiting.
    #[test]
    fn typed_text_answers_a_pending_question() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(AgentMessage::QuestionRequest {
            question: "which one?".into(),
            tool_id: "q1".into(),
            items: Vec::new(),
        });
        for c in "the first".chars() {
            app.update(Input::Char(c), &screen);
        }
        let (_, effects) = app.update(Input::Enter, &screen);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::AnswerQuestion { answer: "the first".into() })],
        );
    }

    /// If the pending prompt does not take text, the typed line must not be
    /// silently swallowed.
    #[test]
    fn text_typed_during_an_approval_is_sent_as_a_message() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        for c in "wait".chars() {
            app.update(Input::Char(c), &screen);
        }
        let (_, effects) = app.update(Input::Enter, &screen);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::SendMessage { content: "wait".into() })],
            "the line was not lost",
        );
    }

    // ── Scrolling ─────────────────────────────────────────────────────────

    #[test]
    fn scrolling_is_clamped_at_both_ends() {
        let (mut app, screen) = app_with(40);
        for _ in 0..500 {
            app.update(Input::Up, &screen);
        }
        let max = app.scroll();
        assert!(max > 0, "there is history to scroll into");
        app.update(Input::Up, &screen);
        assert_eq!(app.scroll(), max, "cannot pass the oldest line");

        for _ in 0..500 {
            app.update(Input::Down, &screen);
        }
        assert_eq!(app.scroll(), 0, "cannot pass the newest line");
    }

    #[test]
    fn a_short_transcript_cannot_scroll() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Up, &screen);
        assert_eq!(app.scroll(), 0);
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let (mut app, screen) = app_with(40);
        app.update(Input::Home, &screen);
        assert!(app.scroll() > 0);
        app.update(Input::End, &screen);
        assert_eq!(app.scroll(), 0);
    }

    #[test]
    fn new_output_returns_to_the_bottom() {
        let (mut app, screen) = app_with(40);
        app.update(Input::Home, &screen);
        assert!(app.scroll() > 0);
        app.follow_tail();
        assert_eq!(app.scroll(), 0);
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    #[test]
    fn a_frame_is_one_synchronized_update_and_never_erases_scrollback() {
        let (mut app, mut screen) = app_with(30);
        app.view(&mut screen);
        let mut sink = Vec::new();
        screen.flush(&mut sink).unwrap();
        let text = String::from_utf8_lossy(&sink);
        assert_eq!(text.matches(crate::screen::SYNC_BEGIN).count(), 1);
        assert!(!text.contains("\x1b[3J"));
    }

    #[test]
    fn the_status_line_shows_what_the_agent_is_doing() {
        let (mut app, mut screen) = app_with(0);
        app.session_mut().apply(AgentMessage::ToolRequest {
            tool_name: "shell_exec".into(),
            tool_args: "{}".into(),
            tool_id: "t".into(),
            kind: "execute".into(),
            subagent_id: None,
            needs_approval: false,
        });
        app.view(&mut screen);
        let mut sink = Vec::new();
        screen.flush(&mut sink).unwrap();
        let text = String::from_utf8_lossy(&sink);
        assert!(text.contains("running shell_exec"), "status names the tool");
    }

    /// The prompt must state the available keys, or approving is guesswork.
    #[test]
    fn a_pending_approval_replaces_the_prompt_with_its_options() {
        let (mut app, mut screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        app.view(&mut screen);
        let mut sink = Vec::new();
        screen.flush(&mut sink).unwrap();
        let text = String::from_utf8_lossy(&sink);
        assert!(text.contains("approve shell_exec?"), "names the tool");
        assert!(text.contains("[y]es"), "and the keys");
    }

    /// Tool output is preformatted; reflowing a diff or a stack trace would
    /// corrupt it.
    #[test]
    fn tool_output_is_not_reflowed() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::ToolOutput {
            tool_name: "shell_exec".into(),
            content: "line one\n    indented\nline three".into(),
        });
        let lines = app.build_lines(80);
        let text: Vec<String> = lines.iter().map(|l| l.plain()).collect();
        assert!(
            text.iter().any(|l| l.contains("    indented")),
            "indentation preserved: {text:?}",
        );
    }

    #[test]
    fn a_thought_is_summarised_with_its_duration() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::Reasoning);
        app.session_mut().apply(AgentMessage::ReasoningToken { content: "hmm".into() });
        app.session_mut().apply(AgentMessage::Done);
        let text: Vec<String> = app.build_lines(60).iter().map(|l| l.plain()).collect();
        assert!(
            text.iter().any(|l| l.contains("thought for")),
            "got {text:?}",
        );
    }

    /// Every drawn line must respect the measured width, or layout and renderer
    /// have diverged — the original overlap bug.
    #[test]
    fn every_line_fits_the_width() {
        let mut app = App::new();
        app.session_mut().push_user("日本語のテキスト with **bold** and `code` and \
                                     \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}");
        app.session_mut().apply(AgentMessage::AssistantToken {
            content: "- a bullet that runs on long enough to need wrapping".into(),
        });
        for cols in [12usize, 20, 40, 80] {
            for line in app.build_lines(cols) {
                assert!(
                    line.width() <= cols,
                    "line {:?} is {} cells, budget {cols}",
                    line.plain(), line.width(),
                );
            }
        }
    }

    #[test]
    fn view_survives_a_degenerate_screen() {
        let mut app = App::new();
        app.session_mut().push_system("text that will not fit anywhere");
        for (cols, rows) in [(0usize, 0usize), (1, 1), (2, 1), (1, 3), (3, 2), (4, 4)] {
            let mut screen = Screen::new(cols, rows);
            app.view(&mut screen);
            let mut sink = Vec::new();
            screen.flush(&mut sink).unwrap();
        }
    }

    /// Streaming mutates the tail entry in place, so a cache keyed only on the
    /// entry count would show stale text.
    #[test]
    fn streaming_text_is_not_served_from_a_stale_cache() {
        let mut app = App::new();
        let mut screen = Screen::new(40, 10);

        app.session_mut().apply(AgentMessage::AssistantToken { content: "first".into() });
        app.view(&mut screen);

        app.session_mut().apply(AgentMessage::AssistantToken { content: " second".into() });
        app.view(&mut screen);

        let mut sink = Vec::new();
        screen.flush(&mut sink).unwrap();
        // The frame after the second token must contain the appended text.
        let text: Vec<String> = app.build_lines(40).iter().map(|l| l.plain()).collect();
        assert!(
            text.iter().any(|l| l.contains("first second")),
            "got {text:?}",
        );
    }

    #[test]
    fn resizing_rewraps_the_transcript() {
        let mut app = App::new();
        app.session_mut()
            .push_system("a message long enough that its wrapping depends on the width");
        let narrow = app.wrap_cached(20).len();
        let wide = app.wrap_cached(70).len();
        assert!(narrow > wide, "narrower means more lines");
    }

    #[test]
    fn quit_is_reported() {
        let (mut app, screen) = app_with(0);
        assert_eq!(app.update(Input::Quit, &screen).0, Outcome::Quit);
    }

    #[test]
    fn long_input_shows_its_tail() {
        let (mut app, screen) = app_with(0);
        for c in "0123456789".repeat(10).chars() {
            app.update(Input::Char(c), &screen);
        }
        let visible = app.visible_input(10);
        assert_eq!(crate::width::str_width(&visible), 10);
        assert!(app.input().ends_with(&visible), "the end, not the start");
    }
}
