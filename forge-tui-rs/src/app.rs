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
use crate::width::str_width;

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
    /// How many transcript entries have been printed permanently.
    ///
    /// Everything before the current turn is settled and gets committed to the
    /// terminal's scrollback, where it can be scrolled back to and copied. Only
    /// what follows is redrawn.
    committed: usize,
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
            committed: 0,
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

    /// Handle one input. `rows` is the window height, which the menu needs for
    /// paging.
    pub fn update(&mut self, input: Input, rows: usize) -> (Outcome, Vec<Effect>) {
        self.sync_dialog();
        let _ = rows;

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

            // Scrolling belongs to the terminal now: the transcript is printed
            // into its scrollback, so the mouse wheel and the scrollbar work on
            // the real history rather than on a window we maintain.
            Input::Up | Input::Down | Input::PageUp | Input::PageDown
            | Input::Home | Input::End => {}
            Input::Resize(..) => {}
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

    /// Render the transcript, following the TypeScript client's layout.
    ///
    /// The indentation is the point: tool calls sit four columns in and their
    /// output six, so a turn reads as a hierarchy rather than a wall of text.
    fn build_range(&self, range: std::ops::Range<usize>, cols: usize) -> Vec<Line> {
        let dim = Style::fg(palette::GRAY).dim();
        let mut out = Vec::new();

        for (i, entry) in self.session.entries()[range.clone()].iter().enumerate() {
            if i > 0 || range.start > 0 {
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

    /// The whole transcript as lines. Used by tests; rendering goes through
    /// [`App::lines_for`] so it can commit and redraw separate ranges.
    #[cfg(test)]
    fn build_lines(&self, cols: usize) -> Vec<Line> {
        self.build_range(0..self.session.entries().len(), cols)
    }

    /// Print settled output, then redraw what is still changing.
    ///
    /// This is the whole point of rendering inline: the conversation goes into
    /// the terminal's own scrollback rather than into a window we page through.
    pub fn render(
        &mut self,
        inline: &mut crate::inline::Inline,
        out:    &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        // Agent events arrive between keypresses, so the dialog has to be brought
        // into step here as well as in `update` — otherwise a prompt the agent is
        // blocked on would not appear until the user pressed something.
        self.sync_dialog();
        let cols = inline.cols().max(1);

        // A cleared session removes entries that were already printed. They
        // cannot be unprinted, so start counting again rather than indexing past
        // the end of a shorter transcript.
        if self.session.entries().len() < self.committed {
            self.committed = 0;
        }

        // Everything before the current turn is settled. Mid-turn, nothing new
        // is committed, because a discard can still take it back.
        let settled = self.session.turn_start().unwrap_or_else(|| self.session.entries().len());
        if settled > self.committed {
            let lines = self.lines_for(self.committed..settled, cols);
            inline.commit(out, &lines)?;
            self.committed = settled;
        }

        let live = self.live_lines(inline, cols);
        let cursor = self.cursor_in(&live, cols);
        inline.draw_live(out, &live.lines, cursor)
    }

    /// Print everything still live, so a finished conversation is entirely in
    /// the scrollback rather than partly erased when the program exits.
    pub fn commit_all(
        &mut self,
        inline: &mut crate::inline::Inline,
        out:    &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        let cols = inline.cols().max(1);
        let total = self.session.entries().len();
        if total > self.committed {
            let lines = self.lines_for(self.committed..total, cols);
            inline.commit(out, &lines)?;
            self.committed = total;
        }
        Ok(())
    }

    /// The block redrawn at the bottom: whatever the current turn has produced
    /// so far, plus the chrome.
    fn live_lines(&mut self, inline: &crate::inline::Inline, cols: usize) -> LiveBlock {
        let capacity = inline.live_capacity();

        // The menu replaces everything while it is open.
        if self.menu.is_some() {
            let mut canvas = Screen::new(cols, capacity);
            canvas.begin_frame();
            let mut menu = self.menu.take().expect("checked above");
            menu.draw(&mut canvas, Rect::new(0, 0, capacity, cols), &self.session,
                      palette::PROMPT);
            self.menu = Some(menu);
            return LiveBlock { lines: canvas.to_lines(), prompt_row: None };
        }

        let mut chrome = Vec::new();

        // Subagents, then the prompt, then the context bar.
        chrome.extend(self.subagent_lines(cols));
        let dialog_lines = self.dialog_lines(cols, capacity);
        chrome.extend(dialog_lines);
        let prompt_offset = chrome.len();
        chrome.push(self.prompt_line(cols));
        chrome.push(self.context_bar_line(cols));

        // The tail of the current turn, as much as fits above the chrome.
        let room = capacity.saturating_sub(chrome.len());
        let mut tail = self.lines_for(self.committed..self.session.entries().len(), cols);
        let hidden = tail.len().saturating_sub(room);
        if hidden > 0 {
            // Keep the newest, and say what is above — the same thing the
            // TypeScript client did while streaming a long reply.
            tail = tail.split_off(hidden);
            if !tail.is_empty() {
                tail[0] = Line {
                    spans: vec![Span {
                        text: format!("  ↑ {hidden} more line{}", if hidden == 1 { "" } else { "s" }),
                        style: Style::fg(palette::GRAY).dim(),
                    }],
                };
            }
        }

        let prompt_row = Some(tail.len() + prompt_offset);
        let mut lines = tail;
        lines.extend(chrome);
        LiveBlock { lines, prompt_row }
    }

    /// Where the caret goes within the live block.
    fn cursor_in(&self, block: &LiveBlock, cols: usize) -> Option<(usize, usize)> {
        let row = block.prompt_row?;
        if self.dialog.is_some() {
            return None; // the dialog owns the caret
        }
        let lead = str_width(PROMPT_GLYPH);
        let typed = str_width(&self.visible_input(cols.saturating_sub(lead)));
        Some((row, (lead + typed).min(cols.saturating_sub(1))))
    }

    /// Render a range of transcript entries.
    fn lines_for(&self, range: std::ops::Range<usize>, cols: usize) -> Vec<Line> {
        let entries = self.session.entries();
        let range = range.start.min(entries.len())..range.end.min(entries.len());
        self.build_range(range, cols)
    }

    fn subagent_lines(&self, cols: usize) -> Vec<Line> {
        let dim = Style::fg(palette::GRAY).dim();
        let subs = &self.session.subagents;
        if subs.is_empty() {
            return Vec::new();
        }
        let mut out = vec![Line {
            spans: vec![
                Span { text: "Subagents".into(), style: Style::fg(palette::MAGENTA) },
                Span { text: format!(" · {} running", subs.len()), style: dim },
            ],
        }];
        for sub in subs {
            let detail = if sub.detail.is_empty() { "starting" } else { &sub.detail };
            out.push(Line {
                spans: vec![
                    Span { text: "  ◆ ".into(), style: Style::fg(palette::MAGENTA) },
                    Span { text: sub.agent_type.clone(), style: Style::default() },
                    Span {
                        text: format!(" · {}", widgets::clip(detail, cols.saturating_sub(20))),
                        style: dim,
                    },
                ],
            });
        }
        out
    }

    fn dialog_lines(&self, cols: usize, capacity: usize) -> Vec<Line> {
        let Some(dialog) = &self.dialog else { return Vec::new() };
        let height = dialog.height(cols).min(capacity.saturating_sub(2)).max(1);
        let mut canvas = Screen::new(cols, height);
        canvas.begin_frame();
        dialog.draw(&mut canvas, Rect::new(0, 0, height, cols), palette::PROMPT);
        canvas.to_lines()
    }

    fn prompt_line(&self, cols: usize) -> Line {
        if self.dialog.is_some() {
            return Line {
                spans: vec![Span {
                    text: "  (answer above)".into(),
                    style: Style::fg(palette::GRAY).dim(),
                }],
            };
        }
        let lead = str_width(PROMPT_GLYPH);
        let mut spans = vec![Span {
            text: PROMPT_GLYPH.into(),
            style: Style::fg(palette::PROMPT).bold(),
        }];
        if self.input.is_empty() {
            spans.push(Span {
                text: widgets::clip(PLACEHOLDER, cols.saturating_sub(lead)),
                style: Style::fg(palette::GRAY).dim(),
            });
        } else {
            spans.push(Span {
                text: self.visible_input(cols.saturating_sub(lead)),
                style: Style::default(),
            });
        }
        Line { spans }
    }

    fn context_bar_line(&self, cols: usize) -> Line {
        let dim = Style::fg(palette::GRAY).dim();
        let mut parts: Vec<String> = Vec::new();
        if !self.session.model_name.is_empty() {
            parts.push(self.session.model_name.clone());
        }
        if let Some(usage) = self.session.usage {
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

        let mut spans = vec![Span {
            text: widgets::clip(&parts.join(" · "), cols),
            style: dim,
        }];
        let (label, colour) = match self.session.permission_mode {
            PermissionMode::AllowAll => (Some("⏵⏵ auto-accept edits"), palette::GREEN),
            PermissionMode::Plan => (Some("⏸ plan mode"), palette::YELLOW),
            PermissionMode::Ask => (None, palette::GRAY),
        };
        if let Some(label) = label {
            spans.push(Span { text: " · ".into(), style: dim });
            spans.push(Span { text: label.into(), style: Style::fg(colour) });
        }
        Line { spans }
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
/// The prompt, as in the TypeScript client.
const PROMPT_GLYPH: &str = "❯ ";

/// The redrawn block, and where the prompt sits inside it.
struct LiveBlock {
    lines:      Vec<Line>,
    /// Row of the prompt within the block, for placing the caret.
    prompt_row: Option<usize>,
}

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

    /// Window height used throughout these tests.
    const ROWS: usize = 24;
    const COLS: usize = 80;

    /// Render through the real inline path and return what was written.
    fn rendered(app: &mut App) -> String {
        let mut inline = crate::inline::Inline::new(COLS, ROWS);
        let mut out: Vec<u8> = Vec::new();
        app.render(&mut inline, &mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    /// What the live block contains, as plain text lines.
    ///
    /// Syncs the dialog first, as rendering does — a prompt the agent is blocked
    /// on has to be reflected before the block is built.
    fn live_text(app: &mut App) -> String {
        app.sync_dialog();
        let inline = crate::inline::Inline::new(COLS, ROWS);
        let block = app.live_lines(&inline, COLS);
        block.lines.iter().map(|l| l.plain()).collect::<Vec<_>>().join("\n")
    }

    /// Emitted output with the escape sequences removed.
    ///
    /// Each span carries its own colour and reset, so text spanning two spans is
    /// not a contiguous substring of the raw output.
    fn visible(out: &str) -> String {
        let mut plain = String::new();
        let mut chars = out.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\u{1b}' {
                plain.push(c);
                continue;
            }
            // Skip a CSI sequence up to its final byte.
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
        }
        plain
    }

    fn app_with(n: usize) -> (App, usize) {
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
        (app, ROWS)
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
        let (mut app, _rows) = app_with(0);
        for c in "hi".chars() {
            app.update(Input::Char(c), ROWS);
        }
        assert_eq!(app.input(), "hi");
        app.update(Input::Backspace, ROWS);
        assert_eq!(app.input(), "h");
    }

    #[test]
    fn backspace_removes_a_whole_grapheme() {
        let (mut app, _rows) = app_with(0);
        for c in "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Backspace, ROWS);
        assert_eq!(app.input(), "", "the whole emoji, not one codepoint");
    }

    #[test]
    fn enter_sends_the_message_and_records_it() {
        let (mut app, _rows) = app_with(0);
        for c in "hello".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (outcome, effects) = app.update(Input::Enter, ROWS);
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
        let (mut app, _rows) = app_with(0);
        app.update(Input::Char(' '), ROWS);
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert!(effects.is_empty());
    }

    #[test]
    fn a_multiline_paste_does_not_submit() {
        let (mut app, _rows) = app_with(0);
        let (_, effects) = app.update(Input::Paste("one\ntwo".into()), ROWS);
        assert!(effects.is_empty(), "paste never submits");
        assert_eq!(app.input(), "one two");
    }

    // ── Interrupt ─────────────────────────────────────────────────────────

    #[test]
    fn interrupt_cancels_only_while_busy() {
        let (mut app, _rows) = app_with(0);
        let (_, effects) = app.update(Input::Interrupt, ROWS);
        assert!(effects.is_empty(), "nothing to cancel when idle");

        app.session_mut().apply(AgentMessage::Thinking);
        let (_, effects) = app.update(Input::Interrupt, ROWS);
        assert_eq!(effects, vec![Effect::Send(ClientMessage::CancelRun)]);
    }

    // ── Approval key routing ──────────────────────────────────────────────

    /// With an approval pending, y/n/a are the answer. Otherwise there would be
    /// no way to approve — the prompt is not accepting text.
    #[test]
    fn approval_keys_answer_instead_of_typing() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));

        let (_, effects) = app.update(Input::Char('y'), ROWS);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::ApproveAction { tool_id: "t1".into() })],
        );
        assert_eq!(app.input(), "", "the key was consumed, not typed");
    }

    #[test]
    fn n_refuses_with_a_reason() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let (_, effects) = app.update(Input::Char('n'), ROWS);
        assert!(matches!(
            effects.first(),
            Some(Effect::Send(ClientMessage::DenyAction { .. })),
        ));
    }

    /// Selecting "Yes, always" grants the tool standing approval, so later calls
    /// of it stop asking. Reachable only by selection, never a bare keystroke.
    #[test]
    fn selecting_always_allow_remembers_the_tool() {
        let (mut app, _rows) = app_with(0);

        app.session_mut().apply(tool_request("shell_exec", "t2"));
        app.update(Input::Down, ROWS);              // onto "Yes, always"
        let (_, effects) = app.update(Input::Enter, ROWS);
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
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let (_, effects) = app.update(Input::Char('y'), ROWS);
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
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));

        let mut effects_seen = Vec::new();
        for c in "wait".chars() {
            let (_, effects) = app.update(Input::Char(c), ROWS);
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
        let (mut app, _rows) = app_with(0);
        assert!(!app.menu_open());
        app.update(Input::Menu, ROWS);
        assert!(app.menu_open());
        // Escape at the top level closes it.
        app.update(Input::End, ROWS);
        assert!(!app.menu_open());
    }

    /// Keystrokes must go to the menu, not the input line, or opening it would
    /// quietly type into the prompt behind it.
    #[test]
    fn the_menu_captures_typing() {
        let (mut app, _rows) = app_with(0);
        app.update(Input::Menu, ROWS);
        for c in "hello".chars() {
            app.update(Input::Char(c), ROWS);
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

        app.update(Input::Menu, ROWS);
        // Root starts on "Main model".
        app.update(Input::Enter, ROWS);
        app.update(Input::Down, ROWS); // onto Second
        let (_, effects) = app.update(Input::Enter, ROWS);

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
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        app.update(Input::Menu, ROWS);
        assert!(!app.menu_open(), "the approval still owns the screen");
        assert!(app.session().pending.is_some());
    }

    /// Quitting has to work from inside the menu too.
    #[test]
    fn quit_works_with_the_menu_open() {
        let (mut app, _rows) = app_with(0);
        app.update(Input::Menu, ROWS);
        assert_eq!(app.update(Input::Quit, ROWS).0, Outcome::Quit);
    }

    #[test]
    fn the_menu_replaces_the_transcript_when_drawn() {
        let (mut app, _rows) = app_with(10);
        app.update(Input::Menu, ROWS);
        let grid = live_text(&mut app);
        assert!(grid.contains("Main model"), "the menu is drawn: {grid:?}");
        assert!(!grid.contains("entry number"), "the transcript is hidden");
    }

    /// A modal must not be a trap: quitting has to work regardless.


    #[test]
    fn quit_works_with_a_prompt_open() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        assert_eq!(app.update(Input::Quit, ROWS).0, Outcome::Quit);
    }

    /// Interrupting abandons the turn, so the prompt it belongs to is denied
    /// rather than left on screen waiting for an answer that will not come.
    #[test]
    fn interrupt_denies_an_open_prompt() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let (_, effects) = app.update(Input::Interrupt, ROWS);
        assert!(matches!(
            effects.first(),
            Some(Effect::Send(ClientMessage::DenyAction { .. })),
        ), "got {effects:?}");
        assert!(app.dialog().is_none(), "and the prompt closes");
    }

    /// The dialog must actually appear, with the options and the keys.
    #[test]
    fn a_pending_approval_draws_a_dialog() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(tool_request("shell_exec", "t1"));
        let grid = live_text(&mut app);
        assert!(grid.contains("shell_exec"), "names the tool: {grid:?}");
        assert!(grid.contains("Yes"), "and offers the options");
        assert!(grid.contains("answer above"), "input line points at the prompt");
    }

    /// An open prompt takes rows from the live block, so less of the in-progress
    /// turn is shown — but the prompt itself is never pushed off.
    #[test]
    fn an_open_prompt_takes_rows_from_the_live_block() {
        let mut app = App::new();
        for i in 0..40 {
            app.session_mut().push_system(format!("line {i}"));
        }
        let inline = crate::inline::Inline::new(COLS, ROWS);
        let before = app.live_lines(&inline, COLS).lines.len();

        app.session_mut().apply(tool_request("shell_exec", "t1"));
        app.sync_dialog();
        let block = app.live_lines(&inline, COLS);
        assert!(block.lines.len() <= inline.live_capacity(), "still fits the window");
        assert!(before <= inline.live_capacity());
        // The prompt row is still inside the block.
        assert!(block.prompt_row.is_some_and(|r| r < block.lines.len()));
    }

    #[test]
    fn approval_keys_do_nothing_when_no_approval_is_pending() {
        let (mut app, _rows) = app_with(0);
        app.update(Input::Char('y'), ROWS);
        assert_eq!(app.input(), "y", "just a letter");
    }

    /// Typed text answers a question rather than being sent as a new message,
    /// which would leave the agent still waiting.
    #[test]
    fn typed_text_answers_a_pending_question() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(AgentMessage::QuestionRequest {
            question: "which one?".into(),
            tool_id: "q1".into(),
            items: Vec::new(),
        });
        for c in "the first".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::AnswerQuestion { answer: "the first".into() })],
        );
    }


    // ── Scrolling ─────────────────────────────────────────────────────────

    /// Scrolling is the terminal's job now: the transcript is printed into its
    /// scrollback, so the wheel and scrollbar act on the real history. The keys
    /// that used to move an internal window must do nothing rather than
    /// silently maintain a viewport nobody sees.
    #[test]
    fn scroll_keys_no_longer_move_an_internal_window() {
        let (mut app, _rows) = app_with(40);
        for key in [Input::Up, Input::Down, Input::PageUp, Input::PageDown,
                    Input::Home, Input::End] {
            let (outcome, effects) = app.update(key, ROWS);
            assert_eq!(outcome, Outcome::Continue);
            assert!(effects.is_empty(), "no message to the agent");
        }
        assert_eq!(app.input(), "", "and nothing typed");
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    #[test]
    fn a_frame_is_one_synchronized_update_and_never_erases_scrollback() {
        let (mut app, _rows) = app_with(30);
        let text = rendered(&mut app);
        assert_eq!(text.matches(crate::screen::SYNC_BEGIN).count(), 1);
        assert!(!text.contains("\x1b[3J"));
    }

    /// The context bar has to reflect real usage, since it is what tells you a
    /// compaction is coming. Text and dot-separated, as the TypeScript bar was —
    /// no gauge, which was my invention.
    #[test]
    fn the_context_bar_shows_context_use() {
        use forge_agent_proto::UsageSnapshot;
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(AgentMessage::UsageUpdate {
            snapshot: UsageSnapshot {
                last_prompt_tokens: 500,
                last_completion_tokens: 0,
                max_context_tokens: 1000,
                ..Default::default()
            },
        });
        let grid = live_text(&mut app);
        assert!(grid.contains("50% ctx"), "the figure is shown: {grid:?}");
        assert!(grid.contains("Model"), "alongside the model name");
        assert!(!grid.contains('█'), "no gauge — that was not in the original");
    }

    /// The prompt is `❯ ` with a placeholder, and no separator rule above it.
    #[test]
    fn the_prompt_matches_the_original() {
        let (mut app, _rows) = app_with(0);
        let grid = live_text(&mut app);
        assert!(grid.contains("❯"), "the prompt glyph: {grid:?}");
        assert!(grid.contains("Type a message"), "and the placeholder");
        assert!(!grid.contains("───"), "no rule — that was not in the original");
        assert!(!grid.contains("● ready"), "and no status line");
    }

    /// Auto-approve has to be visible, since it means prompts are being skipped.
    #[test]
    fn the_context_bar_flags_auto_approve() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().permission_mode = PermissionMode::AllowAll;
        let grid = live_text(&mut app);
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
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(AgentMessage::SubagentStarted {
            id: "s1".into(),
            agent_type: "Explore".into(),
            prompt: "look".into(),
            parent_id: None,
        });
        let grid = live_text(&mut app);
        assert!(grid.contains("Explore"), "got {grid:?}");
    }

    #[test]
    fn the_status_line_shows_what_the_agent_is_doing() {
        let (mut app, _rows) = app_with(0);
        app.session_mut().apply(AgentMessage::ToolRequest {
            tool_name: "shell_exec".into(),
            tool_args: "{}".into(),
            tool_id: "t".into(),
            kind: "execute".into(),
            subagent_id: None,
            needs_approval: false,
        });
        let text = rendered(&mut app);
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

        app.update(Input::ToggleReasoning, ROWS);
        assert!(shown(&app), "expanded");
        assert!(
            app.build_lines(60).iter().any(|l| l.plain().contains("collapse")),
            "and the hint flips",
        );

        app.update(Input::ToggleReasoning, ROWS);
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
    fn rendering_survives_a_degenerate_window() {
        let mut app = App::new();
        app.session_mut().push_system("text that will not fit anywhere");
        for (cols, rows) in [(1usize, 1usize), (2, 1), (4, 2), (10, 3), (20, 5)] {
            let mut inline = crate::inline::Inline::new(cols, rows);
            let mut out: Vec<u8> = Vec::new();
            app.render(&mut inline, &mut out).expect("must not fail");
        }
    }

    /// Streaming appends to the tail entry, so successive renders must show the
    /// growing text rather than a stale copy of it.
    #[test]
    fn streaming_text_is_redrawn_as_it_grows() {
        let mut app = App::new();
        let mut inline = crate::inline::Inline::new(COLS, ROWS);
        let mut out: Vec<u8> = Vec::new();

        app.session_mut().apply(AgentMessage::AssistantToken { content: "first".into() });
        app.render(&mut inline, &mut out).unwrap();

        out.clear();
        app.session_mut().apply(AgentMessage::AssistantToken { content: " second".into() });
        app.render(&mut inline, &mut out).unwrap();

        let text = visible(&String::from_utf8_lossy(&out));
        assert!(text.contains("first second"), "the appended text is drawn: {text:?}");
    }

    #[test]
    fn narrower_windows_wrap_into_more_lines() {
        let mut app = App::new();
        app.session_mut()
            .push_system("a message long enough that its wrapping depends on the width");
        assert!(
            app.build_lines(20).len() > app.build_lines(70).len(),
            "narrower means more lines",
        );
    }

    #[test]
    fn quit_is_reported() {
        let (mut app, _rows) = app_with(0);
        assert_eq!(app.update(Input::Quit, ROWS).0, Outcome::Quit);
    }

    #[test]
    fn long_input_shows_its_tail() {
        let (mut app, _rows) = app_with(0);
        for c in "0123456789".repeat(10).chars() {
            app.update(Input::Char(c), ROWS);
        }
        let visible = app.visible_input(10);
        assert_eq!(crate::width::str_width(&visible), 10);
        assert!(app.input().ends_with(&visible), "the end, not the start");
    }
}
