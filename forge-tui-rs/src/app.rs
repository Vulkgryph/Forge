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
use crate::menu::{self, Menu};
use crate::screen::{Screen, Style};
use crate::session::{Effect, EntryKind, Pending, PermissionMode, Session};
use crate::widgets::{self, Rect};

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
    /// Open the menu.
    Menu,
    /// Show or hide the full text of reasoning blocks.
    ToggleReasoning,
    Quit,
    Resize(usize, usize),
}

/// Palette, matched to the TypeScript client's ink colours.
///
/// ink names map onto the terminal's first sixteen: blue is 12, magenta 13,
/// yellow 11, red 9, green 10, gray 8. Using the same indices means the TUI
/// picks up the user's terminal theme exactly as the old one did, rather than
/// imposing colours of its own.
mod palette {
    /// User messages: bold blue.
    pub const USER:     u8 = 12;
    /// Reasoning, subagents, plan content: magenta.
    pub const MAGENTA:  u8 = 13;
    /// Plan status and the pause indicator: yellow.
    pub const YELLOW:   u8 = 11;
    pub const RED:      u8 = 9;
    pub const GREEN:    u8 = 10;
    /// Dim text throughout.
    pub const GRAY:     u8 = 8;
    /// Diff line tints, as close as 256 colours get to ink's #002800/#280000.
    pub const DIFF_ADD_BG: u8 = 22;
    pub const DIFF_DEL_BG: u8 = 52;
    pub const PROMPT:   u8 = 12;
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
    /// The menu, when open. Takes the whole screen, so the transcript is hidden
    /// while it is up.
    menu:    Option<Menu>,
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
            menu: None,
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

    pub fn menu_open(&self) -> bool {
        self.menu.is_some()
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

        // The menu owns the screen while it is open.
        if self.menu.is_some() {
            let outcome = self
                .menu
                .as_mut()
                .map(|m| m.handle(input, &self.session))
                .unwrap_or(menu::Outcome::Stay);
            return match outcome {
                menu::Outcome::Stay => (Outcome::Continue, Vec::new()),
                menu::Outcome::Close => {
                    self.menu = None;
                    self.cache = None;
                    (Outcome::Continue, Vec::new())
                }
                menu::Outcome::Act(effects) => {
                    // Settings pages stay open so several can be changed in one
                    // visit; anything that ends the session's state closes.
                    self.cache = None;
                    (Outcome::Continue, effects)
                }
            };
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

            Input::Menu => {
                // Not while the agent is blocked on a prompt: the answer it is
                // waiting for has to come first.
                if self.dialog.is_none() {
                    self.menu = Some(Menu::new());
                }
            }

            Input::ToggleReasoning => {
                self.session.toggle_reasoning();
                self.cache = None;
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

    /// Render the transcript, following the TypeScript client's layout.
    ///
    /// The indentation is the point: tool calls sit four columns in and their
    /// output six, so a turn reads as a hierarchy rather than a wall of text.
    fn build_lines(&self, cols: usize) -> Vec<Line> {
        let dim = Style::fg(palette::GRAY).dim();
        let mut out = Vec::new();

        for (i, entry) in self.session.entries().iter().enumerate() {
            if i > 0 {
                out.push(Line::default());
            }
            match entry.kind {
                // Bold blue, no prefix glyph.
                EntryKind::User => {
                    let style = Style::fg(palette::USER).bold();
                    out.extend(restyle(markdown::render(&entry.content, cols), style));
                }

                EntryKind::Assistant | EntryKind::PlanContent => {
                    out.extend(markdown::render(&entry.content, cols));
                }

                // Collapsed to one line unless expanded: "✻ Thinking… (2.1s)".
                EntryKind::Thought | EntryKind::Reasoning => {
                    let stat = entry.duration.map(format_duration).unwrap_or_default();
                    let head = if stat.is_empty() {
                        "Thinking…".to_string()
                    } else {
                        format!("Thinking… ({stat})")
                    };
                    let hint = if self.session.reasoning_expanded {
                        "  (ctrl+t to collapse)"
                    } else {
                        "  (ctrl+t to expand)"
                    };
                    out.push(Line {
                        spans: vec![
                            Span { text: "✻ ".into(), style: Style::fg(palette::MAGENTA) },
                            Span { text: head, style: Style::fg(palette::MAGENTA).dim() },
                            Span { text: hint.into(), style: dim },
                        ],
                    });
                    if self.session.reasoning_expanded {
                        out.extend(indent(
                            restyle(markdown::render(&entry.content, cols.saturating_sub(2)), dim),
                            2,
                            dim,
                        ));
                    }
                }

                // Four columns in, with a status glyph.
                EntryKind::ToolCall => {
                    let mut spans = vec![
                        Span { text: "    ".into(), style: dim },
                        Span { text: "⏺ ".into(), style: Style::fg(palette::MAGENTA) },
                    ];
                    spans.push(Span { text: entry.content.clone(), style: dim });
                    out.push(Line { spans });
                }

                // Results and output are preformatted — running a diff or a
                // stack trace through the markdown wrapper would corrupt it.
                EntryKind::ToolResult => {
                    let glyph_style = match entry.success {
                        Some(false) => Style::fg(palette::RED),
                        _ => Style::fg(palette::GREEN),
                    };
                    let mut lines = entry.content.lines();
                    if let Some(first) = lines.next() {
                        out.push(Line {
                            spans: vec![
                                Span { text: "    ".into(), style: dim },
                                Span {
                                    text: if entry.success == Some(false) { "✗ " } else { "⎿ " }
                                        .into(),
                                    style: glyph_style,
                                },
                                Span { text: clip_line(first, cols, 6), style: dim },
                            ],
                        });
                    }
                    for line in lines {
                        out.push(diff_line(line, cols, dim));
                    }
                }

                EntryKind::ToolOutput => {
                    for line in entry.content.lines() {
                        out.push(diff_line(line, cols, dim));
                    }
                }

                EntryKind::System => {
                    out.extend(restyle(markdown::render(&entry.content, cols), dim));
                }

                EntryKind::Error => {
                    let red = Style::fg(palette::RED);
                    for (j, line) in entry.content.lines().enumerate() {
                        let style = if j == 0 { red.bold() } else { red };
                        out.push(Line {
                            spans: vec![Span { text: line.to_string(), style }],
                        });
                    }
                }

                EntryKind::PlanStatus => {
                    out.push(Line {
                        spans: vec![
                            Span { text: "◆ ".into(), style: Style::fg(palette::YELLOW) },
                            Span {
                                text: entry.content.clone(),
                                style: Style::fg(palette::YELLOW),
                            },
                        ],
                    });
                }

                EntryKind::SubagentHeader => {
                    out.push(Line {
                        spans: vec![
                            Span { text: "  ⎿ ".into(), style: dim },
                            Span {
                                text: entry.content.clone(),
                                style: Style::fg(palette::MAGENTA),
                            },
                        ],
                    });
                }
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

        // The menu replaces the transcript entirely — it is somewhere you are,
        // not an overlay on the conversation. Drawn before anything else so no
        // transcript rows are left showing underneath it.
        if self.menu.is_some() {
            let mut menu = self.menu.take().expect("checked above");
            menu.draw(screen, Rect::new(0, 0, rows, cols), &self.session, palette::PROMPT);
            self.menu = Some(menu);
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

        // Chrome, bottom-up: the context bar sits on the last row, the prompt
        // above it, and any running subagents above that. No separator rule and
        // no status line — the TypeScript client had neither, and the context
        // bar carries what they were showing.
        self.draw_context_bar(screen, rows - 1, cols);
        if rows >= 2 {
            self.draw_input(screen, rows - 2, cols);
        }
        if rows >= 4 {
            self.draw_subagents(screen, rows - 3, cols);
        }
    }

    /// Running subagents, listed above the prompt.
    ///
    /// Drawn upwards from `bottom` so the block grows into the transcript rather
    /// than pushing the prompt off the screen.
    fn draw_subagents(&self, screen: &mut Screen, bottom: usize, cols: usize) {
        let subagents = &self.session.subagents;
        if subagents.is_empty() {
            return;
        }
        let dim = Style::fg(palette::GRAY).dim();

        // One row per agent, plus the heading, as far as there is room.
        let room = bottom.min(subagents.len() + 1);
        let mut row = bottom + 1 - room;

        let col = screen.put(row, 0, "Subagents", Style::fg(palette::MAGENTA));
        screen.put(row, col, &format!(" · {} running", subagents.len()), dim);
        row += 1;

        for sub in subagents.iter().take(room.saturating_sub(1)) {
            let col = screen.put(row, 2, "◆ ", Style::fg(palette::MAGENTA));
            let col = screen.put(row, col, &sub.agent_type, Style::default());
            let detail = if sub.detail.is_empty() { "starting" } else { &sub.detail };
            let room = cols.saturating_sub(col + 3);
            screen.put(row, col, &format!(" · {}", widgets::clip(detail, room)), dim);
            row += 1;
        }
    }

    /// The prompt line: `❯ ` and what has been typed, or a placeholder.
    fn draw_input(&self, screen: &mut Screen, row: usize, cols: usize) {
        // With a prompt open the dialog owns the keyboard and the cursor;
        // showing a live input line as well would suggest typing goes there.
        if self.dialog.is_some() {
            screen.put(row, 0, "  (answer above)", Style::fg(palette::GRAY).dim());
            return;
        }

        let col = screen.put(row, 0, "❯ ", Style::fg(palette::PROMPT).bold());
        if self.input.is_empty() {
            screen.put(row, col, &widgets::clip(PLACEHOLDER, cols.saturating_sub(col)),
                       Style::fg(palette::GRAY).dim());
            screen.set_cursor(row, col);
            return;
        }
        let visible = self.visible_input(cols.saturating_sub(col));
        let end = screen.put(row, col, &visible, Style::default());
        screen.set_cursor(row, end.min(cols.saturating_sub(1)));
    }

    /// The context bar: model, context use and mode, dot-separated and dim.
    fn draw_context_bar(&self, screen: &mut Screen, row: usize, cols: usize) {
        let dim = Style::fg(palette::GRAY).dim();
        let mut parts: Vec<String> = Vec::new();

        if !self.session.model_name.is_empty() {
            parts.push(self.session.model_name.clone());
        }
        if let Some(usage) = self.session.usage {
            // Prompt tokens against the window, as the TypeScript bar computed
            // it — not prompt plus completion.
            let pct = if usage.max_context_tokens > 0 {
                (usage.last_prompt_tokens as f64 / usage.max_context_tokens as f64 * 100.0)
                    .round() as u32
            } else {
                0
            };
            parts.push(format!("{pct}% ctx"));
        }
        if self.session.plan_mode {
            parts.push("PLAN".into());
        }
        if let Some(label) = self.session.activity.label() {
            parts.push(label);
        }

        let col = screen.put(row, 0, &widgets::clip(&parts.join(" · "), cols), dim);

        // The mode indicator is coloured rather than dim, since it says
        // approvals are being skipped.
        let (label, colour) = match self.session.permission_mode {
            PermissionMode::AllowAll => (Some("⏵⏵ auto-accept edits"), palette::GREEN),
            PermissionMode::Plan => (Some("⏸ plan mode"), palette::YELLOW),
            PermissionMode::Ask => (None, palette::GRAY),
        };
        if let Some(label) = label {
            let room = cols.saturating_sub(col + 3);
            if room > 4 {
                let col = screen.put(row, col, " · ", dim);
                screen.put(row, col, &widgets::clip(label, room), Style::fg(colour));
            }
        }
    }

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

/// Shown when nothing has been typed, as in the TypeScript client.
const PLACEHOLDER: &str = "Type a message...";

/// Apply one style to every span of every line.
fn restyle(lines: Vec<Line>, style: Style) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| Line {
            spans: line
                .spans
                .into_iter()
                .map(|mut span| {
                    // Keep the markdown's own emphasis, take the colour.
                    span.style = Style { bold: span.style.bold, ..style };
                    span
                })
                .collect(),
        })
        .collect()
}

/// Shift lines right by `by` columns.
fn indent(lines: Vec<Line>, by: usize, style: Style) -> Vec<Line> {
    lines
        .into_iter()
        .map(|line| {
            let mut spans = vec![Span { text: " ".repeat(by), style }];
            spans.extend(line.spans);
            Line { spans }
        })
        .collect()
}

/// Truncate a preformatted line to the space left after `used` columns.
fn clip_line(line: &str, cols: usize, used: usize) -> String {
    crate::widgets::clip(line, cols.saturating_sub(used))
}

/// One line of tool output, tinted when it is a diff.
///
/// Added and removed lines get a background rather than only coloured text, so a
/// diff reads as blocks at a glance — the same treatment the TypeScript client
/// gave them. This is why [`Style`] needed a background at all.
fn diff_line(line: &str, cols: usize, dim: Style) -> Line {
    let style = match line.trim_start().chars().next() {
        Some('+') if !line.trim_start().starts_with("+++") => {
            Style::fg(palette::GREEN).bg(palette::DIFF_ADD_BG)
        }
        Some('-') if !line.trim_start().starts_with("---") => {
            Style::fg(palette::RED).bg(palette::DIFF_DEL_BG)
        }
        _ => dim,
    };
    Line {
        spans: vec![
            Span { text: "      ".into(), style: dim },
            Span { text: clip_line(line, cols, 6), style },
        ],
    }
}

/// A duration at a readable precision.
///
/// Truncating to whole seconds reported "thought for 0s" for anything under a
/// second, which is both wrong-looking and useless — most reasoning blocks are
/// fast, so that was the common case rather than an edge one.
fn format_duration(d: std::time::Duration) -> String {
    let ms = d.as_millis();
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let secs = d.as_secs();
    if secs < 60 {
        // One decimal below a minute: the difference between 2s and 2.7s is
        // worth seeing when you are waiting for it.
        return format!("{:.1}s", d.as_secs_f64());
    }
    let (minutes, rest) = (secs / 60, secs % 60);
    if rest == 0 {
        format!("{minutes}m")
    } else {
        format!("{minutes}m {rest}s")
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

    // ── Menu ──────────────────────────────────────────────────────────────

    #[test]
    fn the_menu_opens_and_closes() {
        let (mut app, screen) = app_with(0);
        assert!(!app.menu_open());
        app.update(Input::Menu, &screen);
        assert!(app.menu_open());
        // Escape at the top level closes it.
        app.update(Input::End, &screen);
        assert!(!app.menu_open());
    }

    /// Keystrokes must go to the menu, not the input line, or opening it would
    /// quietly type into the prompt behind it.
    #[test]
    fn the_menu_captures_typing() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Menu, &screen);
        for c in "hello".chars() {
            app.update(Input::Char(c), &screen);
        }
        assert_eq!(app.input(), "", "nothing reached the input line");
        assert!(app.menu_open(), "and it is still open");
    }

    /// The reason the menu exists: switching model has to reach the agent.
    #[test]
    fn switching_model_from_the_menu_sends_the_message() {
        use forge_agent_proto::{EndpointInfo, EndpointReasoningConfig};
        let mut app = App::new();
        let screen = Screen::new(80, 24);
        app.session_mut().apply(AgentMessage::Init(Box::new(Init {
            model_name: "First".into(),
            model_id: "first-1".into(),
            endpoints: vec![
                EndpointInfo {
                    name: "First".into(),
                    base_url: "https://example.invalid".into(),
                    model_id: "first-1".into(),
                    max_context_tokens: 1000,
                    max_output_tokens: 100,
                    endpoint_type: "anthropic".into(),
                    reasoning: EndpointReasoningConfig::default(),
                    xai_priority_tier: false,
                },
                EndpointInfo {
                    name: "Second".into(),
                    base_url: "https://example.invalid".into(),
                    model_id: "second-1".into(),
                    max_context_tokens: 2000,
                    max_output_tokens: 200,
                    endpoint_type: "open_ai".into(),
                    reasoning: EndpointReasoningConfig::default(),
                    xai_priority_tier: false,
                },
            ],
            ..Default::default()
        })));

        app.update(Input::Menu, &screen);
        // Root starts on "Main model".
        app.update(Input::Enter, &screen);
        app.update(Input::Down, &screen); // onto Second
        let (_, effects) = app.update(Input::Enter, &screen);

        match effects.first() {
            Some(Effect::Send(ClientMessage::SwitchModel { model_id, .. })) => {
                assert_eq!(model_id, "second-1");
            }
            other => panic!("expected a model switch, got {other:?}"),
        }
    }

    /// The menu must not be reachable while the agent is blocked on a prompt —
    /// the answer it is waiting for has to come first.
    #[test]
    fn the_menu_does_not_open_over_a_pending_prompt() {
        let (mut app, screen) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        app.update(Input::Menu, &screen);
        assert!(!app.menu_open(), "the approval still owns the screen");
        assert!(app.session().pending.is_some());
    }

    /// Quitting has to work from inside the menu too.
    #[test]
    fn quit_works_with_the_menu_open() {
        let (mut app, screen) = app_with(0);
        app.update(Input::Menu, &screen);
        assert_eq!(app.update(Input::Quit, &screen).0, Outcome::Quit);
    }

    #[test]
    fn the_menu_replaces_the_transcript_when_drawn() {
        let (mut app, mut screen) = app_with(10);
        app.update(Input::Menu, &screen);
        app.view(&mut screen);
        let grid: String = (0..screen.rows())
            .map(|r| screen.row_text(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(grid.contains("Main model"), "the menu is drawn: {grid:?}");
        assert!(!grid.contains("entry number"), "the transcript is hidden");
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
    /// compaction is coming. Text and dot-separated, as the TypeScript bar was —
    /// no gauge, which was my invention.
    #[test]
    fn the_context_bar_shows_context_use() {
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
        assert!(grid.contains("50% ctx"), "the figure is shown: {grid:?}");
        assert!(grid.contains("Model"), "alongside the model name");
        assert!(!grid.contains('█'), "no gauge — that was not in the original");
    }

    /// The prompt is `❯ ` with a placeholder, and no separator rule above it.
    #[test]
    fn the_prompt_matches_the_original() {
        let (mut app, mut screen) = app_with(0);
        app.view(&mut screen);
        let grid: String = (0..screen.rows())
            .map(|r| screen.row_text(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(grid.contains("❯"), "the prompt glyph: {grid:?}");
        assert!(grid.contains("Type a message"), "and the placeholder");
        assert!(!grid.contains("───"), "no rule — that was not in the original");
        assert!(!grid.contains("● ready"), "and no status line");
    }

    /// Auto-approve has to be visible, since it means prompts are being skipped.
    #[test]
    fn the_context_bar_flags_auto_approve() {
        let (mut app, mut screen) = app_with(0);
        app.session_mut().permission_mode = PermissionMode::AllowAll;
        app.view(&mut screen);
        let grid: String = (0..screen.rows())
            .map(|r| screen.row_text(r))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(grid.contains("auto-accept"), "got {grid:?}");
    }

    /// Tool calls indent four columns and their output six, so a turn reads as a
    /// hierarchy rather than a flat wall of text.
    #[test]
    fn tool_calls_and_output_are_indented() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::ToolRequest {
            tool_name: "read_file".into(),
            tool_args: r#"{"path":"src/main.rs"}"#.into(),
            tool_id: "t1".into(),
            kind: "read".into(),
            subagent_id: None,
            needs_approval: false,
        });
        app.session_mut().apply(AgentMessage::ToolOutput {
            tool_name: "read_file".into(),
            content: "the file contents".into(),
        });

        let lines: Vec<String> = app.build_lines(60).iter().map(|l| l.plain()).collect();
        let call = lines.iter().find(|l| l.contains("read_file(")).expect("the call");
        assert!(call.starts_with("    "), "call indented four: {call:?}");
        let output = lines.iter().find(|l| l.contains("file contents")).expect("the output");
        assert!(output.starts_with("      "), "output indented six: {output:?}");
    }

    /// Diff lines are tinted, not just coloured — the reason Style needed a
    /// background at all.
    #[test]
    fn diff_lines_in_tool_output_are_tinted() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::ToolOutput {
            tool_name: "apply_patch".into(),
            content: "--- a/x\n+++ b/x\n-removed line\n+added line\n unchanged".into(),
        });

        let lines = app.build_lines(60);
        let find = |needle: &str| {
            lines
                .iter()
                .find(|l| l.plain().contains(needle))
                .unwrap_or_else(|| panic!("no line with {needle:?}"))
                .spans
                .iter()
                .find(|s| s.text.contains(needle))
                .map(|s| s.style)
                .expect("a span")
        };

        assert_eq!(find("added line").bg, Some(palette::DIFF_ADD_BG), "additions tinted");
        assert_eq!(find("removed line").bg, Some(palette::DIFF_DEL_BG), "removals tinted");
        // The file headers are not changes and must not be tinted.
        assert_eq!(find("+++ b/x").bg, None, "the +++ header is not an addition");
        assert_eq!(find("--- a/x").bg, None, "nor is --- a removal");
        assert_eq!(find("unchanged").bg, None);
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

    /// Whole-second truncation showed "thought for 0s" for anything under a
    /// second, which is most reasoning blocks.
    #[test]
    fn short_durations_are_not_reported_as_zero() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(1)), "1ms");
        assert_eq!(format_duration(Duration::from_millis(420)), "420ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
        for d in [1u64, 250, 999] {
            let out = format_duration(Duration::from_millis(d));
            assert!(!out.starts_with('0'), "{d}ms rendered as {out:?}");
        }
    }

    #[test]
    fn longer_durations_read_naturally() {
        use std::time::Duration;
        assert_eq!(format_duration(Duration::from_millis(1_000)), "1.0s");
        assert_eq!(format_duration(Duration::from_millis(2_700)), "2.7s");
        assert_eq!(format_duration(Duration::from_secs(59)), "59.0s");
        assert_eq!(format_duration(Duration::from_secs(60)), "1m");
        assert_eq!(format_duration(Duration::from_secs(125)), "2m 5s");
    }

    /// Reasoning collapses to one line, as in the TypeScript client: a
    /// transcript that prints every thought in full is mostly grey text.
    #[test]
    fn reasoning_collapses_to_a_single_line() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::Reasoning);
        app.session_mut().apply(AgentMessage::ReasoningToken {
            content: "a long internal monologue".into(),
        });
        app.session_mut().apply(AgentMessage::Done);

        let text: Vec<String> = app.build_lines(60).iter().map(|l| l.plain()).collect();
        assert!(text.iter().any(|l| l.contains("✻")), "the glyph: {text:?}");
        assert!(text.iter().any(|l| l.contains("Thinking…")), "the label");
        assert!(text.iter().any(|l| l.contains("ctrl+t to expand")), "and how to open it");
        assert!(
            !text.iter().any(|l| l.contains("long internal monologue")),
            "the body stays hidden: {text:?}",
        );
    }

    /// Ctrl-T reveals it, and again hides it.
    #[test]
    fn ctrl_t_expands_and_collapses_reasoning() {
        let mut app = App::new();
        let screen = Screen::new(60, 20);
        app.session_mut().apply(AgentMessage::Reasoning);
        app.session_mut().apply(AgentMessage::ReasoningToken {
            content: "the hidden thought".into(),
        });
        app.session_mut().apply(AgentMessage::Done);

        let shown = |app: &App| {
            app.build_lines(60).iter().any(|l| l.plain().contains("hidden thought"))
        };
        assert!(!shown(&app), "collapsed to begin with");

        app.update(Input::ToggleReasoning, &screen);
        assert!(shown(&app), "expanded");
        assert!(
            app.build_lines(60).iter().any(|l| l.plain().contains("collapse")),
            "and the hint flips",
        );

        app.update(Input::ToggleReasoning, &screen);
        assert!(!shown(&app), "collapsed again");
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
