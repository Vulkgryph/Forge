// SPDX-License-Identifier: Apache-2.0
//! The application: a [`Session`] plus a viewport onto it, and the input line.
//!
//! Still a pure state machine — [`App::update`] takes one decoded [`Input`] and
//! returns what to send to the agent, and [`App::view`] draws into a [`Screen`].
//! Neither touches a terminal or the agent, so scrolling, wrapping and the key
//! routing around a pending prompt are all testable directly.

use forge_agent_proto::ClientMessage;

use crate::dialog::{Decision, Dialog};
use crate::markdown::{self, Line, Span};
use crate::screen::{Screen, Style};
use crate::session::{Effect, EntryKind, Pending, PermissionMode, Session};
use crate::widgets::Rect;

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
    /// The modal prompt, mirroring `session.pending`.
    dialog:  Option<Dialog>,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            session: Session::new(),
            input: String::new(),
            scroll: 0,
            cache: None,
            dialog: None,
        }
    }

    /// Keep the dialog in step with what the agent is waiting on.
    ///
    /// Rebuilding only on a transition preserves the user's cursor and any text
    /// they have typed; rebuilding every frame would reset both under them.
    fn sync_dialog(&mut self) {
        match (self.session.pending.is_some(), self.dialog.is_some()) {
            (true, false) => {
                self.dialog = self.session.pending.as_ref().map(Dialog::for_pending);
            }
            (false, true) => self.dialog = None,
            _ => {}
        }
    }

    /// The open modal prompt, if any.
    pub fn dialog(&self) -> Option<&Dialog> {
        self.dialog.as_ref()
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
        self.sync_dialog();
        let page = self.viewport_rows(screen).max(1);

        // Quitting and interrupting are never captured by a prompt: a modal that
        // could not be escaped would be a trap.
        match input {
            Input::Quit => return (Outcome::Quit, Vec::new()),
            Input::Interrupt if self.dialog.is_some() => {
                // Interrupting while blocked denies the prompt as a side effect,
                // since the turn it belongs to is being abandoned.
                let effects = self.decide(Decision::Deny);
                return (Outcome::Continue, effects);
            }
            Input::Resize(..) => {
                self.cache = None;
                return (Outcome::Continue, Vec::new());
            }
            _ => {}
        }

        // A prompt is modal: everything else goes to it.
        if self.dialog.is_some() {
            let decision = self.dialog.as_mut().and_then(|d| d.handle(input));
            return match decision {
                Some(decision) => (Outcome::Continue, self.decide(decision)),
                None => (Outcome::Continue, Vec::new()),
            };
        }

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

    /// Turn a dialog decision into session calls.
    fn decide(&mut self, decision: Decision) -> Vec<Effect> {
        let is_rewind = matches!(self.session.pending, Some(Pending::Rewind { .. }));
        let effects = match decision {
            // A rewind answers with a checkpoint id, not a tool id.
            Decision::Approve { .. } if is_rewind => self.session.confirm_rewind(),
            Decision::Approve { remember } => self.session.approve(remember),
            Decision::Deny if is_rewind => {
                self.session.cancel_pending();
                Vec::new()
            }
            Decision::Deny => self.session.deny("denied by the user"),
            Decision::Answer(text) => self.session.reply(text),
            Decision::ApprovePlan { clear_context } => self.session.approve_plan(clear_context),
            Decision::Cancel => {
                self.session.cancel_pending();
                Vec::new()
            }
        };
        self.dialog = None;
        self.cache = None;
        self.follow_tail();
        effects
    }

    /// Rows available to the transcript: everything but the status line, the
    /// rule, the input line, and whatever the open prompt occupies.
    fn viewport_rows(&self, screen: &Screen) -> usize {
        let chrome = 3 + self.dialog_rows(screen);
        screen.rows().saturating_sub(chrome)
    }

    /// How many rows the open prompt wants, bounded so the transcript never
    /// disappears entirely behind it.
    fn dialog_rows(&self, screen: &Screen) -> usize {
        match &self.dialog {
            None => 0,
            Some(d) => {
                let wanted = d.height(screen.cols());
                let ceiling = screen.rows().saturating_sub(4).max(1);
                wanted.min(ceiling)
            }
        }
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
        self.sync_dialog();
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

        // The prompt sits above the chrome, over the transcript's lowest rows.
        let dialog_rows = self.dialog_rows(screen);
        if dialog_rows > 0 {
            let area = Rect::new(
                rows.saturating_sub(3 + dialog_rows),
                0,
                dialog_rows,
                cols,
            );
            if let Some(dialog) = &self.dialog {
                dialog.draw(screen, area, palette::PROMPT);
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

        // Context use as a bar plus a figure — the bar is readable at a glance,
        // the number is what you quote when it matters.
        if self.session.usage.is_some() {
            let fraction = self.session.context_fraction();
            let pct = (fraction * 100.0).round() as u32;
            // Amber past three quarters: the point where compaction is near.
            let style = if fraction > 0.75 {
                Style::fg(palette::ERROR)
            } else {
                Style::fg(palette::TOOL)
            };
            crate::widgets::gauge(screen, row, col + 2, 8, fraction, style);
            col = screen.put(row, col + 11, &format!("{pct}%"),
                             Style::fg(palette::SYSTEM).dim());
        }

        // Running subagents, named so a long delegation is not a mystery.
        if !self.session.subagents.is_empty() {
            let text = match self.session.subagents.len() {
                1 => {
                    let sub = &self.session.subagents[0];
                    if sub.detail.is_empty() {
                        format!("▸ {}", sub.agent_type)
                    } else {
                        format!("▸ {}: {}", sub.agent_type, sub.detail)
                    }
                }
                n => format!("▸ {n} subagents"),
            };
            let room = cols.saturating_sub(col + 4);
            col = screen.put(row, col + 2, &crate::widgets::clip(&text, room),
                             Style::fg(palette::SUBAGENT));
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
        // With a prompt open, the dialog owns both the keyboard and the cursor;
        // showing a live input line as well would suggest typing goes there.
        if self.dialog.is_some() {
            screen.put(row, 0, "  (answer above)", Style::fg(palette::SYSTEM).dim());
            return;
        }

        let col = screen.put(row, 0, "› ", Style::fg(palette::PROMPT).bold());
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
    fn n_refuses_with_a_reason() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let (_, effects) = app.update(Input::Char('n'), &screen);
        assert!(matches!(
            effects.first(),
            Some(Effect::Send(ClientMessage::DenyAction { .. })),
        ));
    }

    /// Selecting "Yes, always" grants the tool standing approval, so later calls
    /// of it stop asking. Reachable only by selection, never a bare keystroke.
    #[test]
    fn selecting_always_allow_remembers_the_tool() {
        let (mut app, screen) = app_with(0);

        app.session_mut().apply(tool_request("shell_exec", "t2"));
        app.update(Input::Down, &screen);              // onto "Yes, always"
        let (_, effects) = app.update(Input::Enter, &screen);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t2".into() })],
        );

        // The next call of that tool goes straight through.
        let effects = app.session_mut().apply(tool_request("shell_exec", "t3"));
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t3".into() })],
        );

        // A different tool still asks.
        app.session_mut().apply(tool_request("write_file", "t4"));
        assert!(app.session().pending.is_some());
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

    /// A prompt is modal: keystrokes belong to it, not the input line. This is
    /// what removes the earlier hazard where typing "wait" fired "always
    /// approve" — there is no longer any ambiguity to resolve.
    #[test]
    fn a_prompt_captures_typing_instead_of_the_input_line() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));

        let mut effects_seen = Vec::new();
        for c in "wait".chars() {
            let (_, effects) = app.update(Input::Char(c), &screen);
            effects_seen.extend(effects);
        }
        assert!(
            effects_seen.is_empty(),
            "typing approved nothing: {effects_seen:?}",
        );
        assert_eq!(app.input(), "", "the input line was not touched");
        assert!(app.session().pending.is_some(), "still waiting");
    }

    /// A modal must not be a trap: quitting has to work regardless.
    #[test]
    fn quit_works_with_a_prompt_open() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        assert_eq!(app.update(Input::Quit, &screen).0, Outcome::Quit);
    }

    /// Interrupting abandons the turn, so the prompt it belongs to is denied
    /// rather than left on screen waiting for an answer that will not come.
    #[test]
    fn interrupt_denies_an_open_prompt() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let (_, effects) = app.update(Input::Interrupt, &screen);
        assert!(matches!(
            effects.first(),
            Some(Effect::Send(ClientMessage::DenyAction { .. })),
        ), "got {effects:?}");
        assert!(app.dialog().is_none(), "and the prompt closes");
    }

    /// The dialog must actually appear, with the options and the keys.
    #[test]
    fn a_pending_approval_draws_a_dialog() {
        let (mut app, mut screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        app.view(&mut screen);
        let grid: String = (0..screen.rows())
            .map(|r| screen.row_text(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(grid.contains("shell_exec"), "names the tool: {grid:?}");
        assert!(grid.contains("Yes"), "and offers the options");
        assert!(grid.contains("answer above"), "input line points at the prompt");
    }

    /// The transcript must not be drawn underneath the prompt.
    #[test]
    fn an_open_prompt_shrinks_the_transcript_viewport() {
        let (mut app, screen) = app_with(30);
        let before = app.viewport_rows(&screen);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        app.sync_dialog();
        let after = app.viewport_rows(&screen);
        assert!(after < before, "{after} should be fewer rows than {before}");
        assert!(after >= 1, "the transcript never vanishes entirely");
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

    /// The context bar has to reflect real usage, since it is what tells you a
    /// compaction is coming.
    #[test]
    fn the_status_line_shows_context_use() {
        use forge_agent_proto::UsageSnapshot;
        let (mut app, mut screen) = app_with(0);
        app.session_mut().apply(AgentMessage::UsageUpdate {
            snapshot: UsageSnapshot {
                last_prompt_tokens: 500,
                last_completion_tokens: 0,
                max_context_tokens: 1000,
                ..Default::default()
            },
        });
        app.view(&mut screen);
        let grid: String = (0..screen.rows())
            .map(|r| screen.row_text(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(grid.contains("50%"), "the figure is shown: {grid:?}");
        assert!(grid.contains('█'), "and the bar");
    }

    #[test]
    fn the_status_line_names_a_running_subagent() {
        let (mut app, mut screen) = app_with(0);
        app.session_mut().apply(AgentMessage::SubagentStarted {
            id: "s1".into(),
            agent_type: "Explore".into(),
            prompt: "look".into(),
            parent_id: None,
        });
        app.view(&mut screen);
        let grid: String = (0..screen.rows())
            .map(|r| screen.row_text(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(grid.contains("Explore"), "got {grid:?}");
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
