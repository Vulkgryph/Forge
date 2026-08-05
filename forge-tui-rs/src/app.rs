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
use crate::commands::{self, Command};
use crate::menu::{self, Menu, Page};
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
    /// Complete the slash command being typed.
    Complete,
    /// Insert a literal newline in the input.
    Newline,
    Quit,
    Resize(usize, usize),
}

/// Palette, matched to the TypeScript client's ink colours.
///
/// ink names map onto the terminal's first sixteen: blue is 12, magenta 13,
/// yellow 11, red 9, green 10, gray 8. Using the same indices means the TUI
/// picks up the user's terminal theme — which is the right default, since it is
/// the user's own choice of palette.
///
/// Three roles are exceptions, given explicit 256-colour values because the
/// theme's version of them was not readable. Index 12 is a dark indigo in many
/// dark themes, and it carried the prompt, the user's own messages, the menu
/// selection and the dialog accent — the most important chrome in the interface,
/// in the one colour hardest to see on black. Index 8 is often near #555, too
/// close to the background for text that still has to be read. These are picked
/// for luminance against a dark background instead.
mod palette {
    /// User messages, the prompt, the menu selection, dialog accents.
    ///
    /// A light sky blue rather than index 12's dark indigo: same family, but
    /// legible on black.
    pub const USER:     u8 = 117;
    /// Reasoning and the "Thinking…" label.
    ///
    /// Warm, so it reads as distinct from the blue chrome without competing with
    /// the assistant's own text, and bright enough to be read rather than merely
    /// noticed. Was magenta, which it shared with subagents and plan content.
    pub const THINKING: u8 = 179;
    /// Subagents and plan content: magenta.
    pub const MAGENTA:  u8 = 13;
    /// Plan status and the pause indicator: yellow.
    pub const YELLOW:   u8 = 11;
    pub const RED:      u8 = 9;
    pub const GREEN:    u8 = 10;
    /// Dim text throughout — hints, placeholders, the context bar.
    pub const GRAY:     u8 = 245;
    /// Diff line tints, as close as 256 colours get to ink's #002800/#280000.
    pub const DIFF_ADD_BG: u8 = 22;
    pub const DIFF_DEL_BG: u8 = 52;
    pub const PROMPT:   u8 = USER;
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
    /// Which suggestion is highlighted while a slash command is being typed.
    suggestion: usize,
    /// Frame of the activity spinner. Only advances while a turn is running.
    spinner: usize,
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
            suggestion: 0,
            spinner: 0,
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

            Input::Complete => {
                if let Some(entry) = self.selected_suggestion() {
                    self.input = entry.command.to_string();
                    self.suggestion = 0;
                }
            }

            Input::Newline => {
                self.input.push('\n');
                self.suggestion = 0;
            }

            Input::ToggleReasoning => {
                self.session.toggle_reasoning();
                self.cache = None;
            }

            Input::Char(c) => {
                self.input.push(c);
                self.suggestion = 0;
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
                self.suggestion = 0;
            }

            Input::Enter if self.input.ends_with('\\') => {
                // The placeholder advertises `\+Enter` for a newline, so a
                // trailing backslash means "continue" rather than "send". The
                // backslash itself is consumed; it was an instruction, not text.
                self.input.pop();
                self.input.push('\n');
            }

            Input::Enter => {
                // A highlighted suggestion is what Enter runs. Submitting the
                // half-typed text instead would report it as an unknown command,
                // which is what the prompt's "Enter select/run" hint rules out.
                if let Some(entry) = self.selected_suggestion() {
                    let command = entry.command;
                    self.input.clear();
                    self.suggestion = 0;
                    if let Some(parsed) = commands::parse(command) {
                        return self.run_command(parsed);
                    }
                }

                let text = self.input.trim().to_string();
                self.input.clear();
                self.suggestion = 0;
                if text.is_empty() {
                    return (Outcome::Continue, Vec::new());
                }
                // A slash command is for us, not the model. Sending it as a
                // message would have the agent answer questions about it.
                if let Some(command) = commands::parse(&text) {
                    return self.run_command(command);
                }

                self.session_mut().push_user(&text);
                self.follow_tail();
                return (
                    Outcome::Continue,
                    vec![Effect::Send(ClientMessage::SendMessage { content: text })],
                );
            }

            // With suggestions showing, the arrows move through them — that is
            // what the prompt's own hint promises.
            Input::Up if self.has_suggestions() => {
                let n = self.suggestions().len();
                self.suggestion = if self.suggestion == 0 { n - 1 } else { self.suggestion - 1 };
            }
            Input::Down if self.has_suggestions() => {
                let n = self.suggestions().len();
                self.suggestion = (self.suggestion + 1) % n;
            }

            // Otherwise scrolling belongs to the terminal: the transcript is
            // printed into its scrollback, so the wheel and the scrollbar work on
            // the real history rather than on a window we maintain.
            Input::Up | Input::Down | Input::PageUp | Input::PageDown
            | Input::Home | Input::End => {}
            Input::Resize(..) => {}
        }

        (Outcome::Continue, Vec::new())
    }

    /// Turn a dialog decision into session calls.
    /// Carry out a slash command.
    fn run_command(&mut self, command: Command) -> (Outcome, Vec<Effect>) {
        let send = |msg| (Outcome::Continue, vec![Effect::Send(msg)]);
        match command {
            Command::Quit => (Outcome::Quit, Vec::new()),
            Command::Restart => (Outcome::Continue, vec![Effect::Restart { resume: None }]),
            Command::Clear => send(ClientMessage::ClearSession),
            Command::Compact => send(ClientMessage::Compact),
            Command::Usage => send(ClientMessage::RequestUsage),
            Command::Plan => send(ClientMessage::EnterPlanMode),
            Command::Login => send(ClientMessage::LoginChatgpt),

            Command::Log => {
                let path = if self.session.log_path.is_empty() {
                    "no log path yet".to_string()
                } else {
                    self.session.log_path.clone()
                };
                self.session_mut().push_system(path);
                (Outcome::Continue, Vec::new())
            }

            Command::Help => {
                self.session_mut().push_system(commands::help_text());
                (Outcome::Continue, Vec::new())
            }

            Command::OpenMenu(page) => {
                self.menu = Some(Menu::at(match page {
                    commands::Page::Models => Page::Models,
                    commands::Page::Settings => Page::Settings,
                    commands::Page::ContextStrategy => Page::ContextStrategy,
                    commands::Page::Subagents => Page::Subagents,
                    commands::Page::Rewind => Page::Rewind,
                    commands::Page::Sessions => Page::Sessions,
                    commands::Page::Thinking => Page::Thinking,
                }));
                (Outcome::Continue, Vec::new())
            }

            Command::Unknown(name) => {
                // Say so rather than sending it to the model, which would answer
                // a question about a command that does not exist.
                self.session_mut()
                    .push_system(format!("{name} is not a command. /help lists them."));
                (Outcome::Continue, Vec::new())
            }
        }
    }

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
                            Span { text: "✻ ".into(), style: Style::fg(palette::THINKING) },
                            // Not dimmed: dimming a colour on a black background
                            // is what made this hard to read in the first place.
                            // The hint beside it stays dim, so the label still
                            // reads as the louder of the two.
                            Span { text: head, style: Style::fg(palette::THINKING) },
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
                        // The glyph occupies the first row; the text wraps under
                        // it rather than being cut off.
                        let mut rows = wrap_at(first, cols, 6);
                        let head = rows.remove(0);
                        out.push(Line {
                            spans: vec![
                                Span { text: "    ".into(), style: dim },
                                Span {
                                    text: if entry.success == Some(false) { "✗ " } else { "⎿ " }
                                        .into(),
                                    style: glyph_style,
                                },
                                Span { text: head, style: dim },
                            ],
                        });
                        for row in rows {
                            out.push(Line {
                                spans: vec![
                                    Span { text: " ".repeat(6), style: dim },
                                    Span { text: row, style: dim },
                                ],
                            });
                        }
                    }
                    for line in lines {
                        out.extend(diff_lines(line, cols, dim));
                    }
                }

                EntryKind::ToolOutput => {
                    for line in entry.content.lines() {
                        out.extend(diff_lines(line, cols, dim));
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

    /// Advance the spinner one frame.
    ///
    /// Driven by the event loop's timer rather than a clock read here, so the
    /// state machine stays free of time and the animation stops dead when there
    /// is nothing running.
    pub fn tick(&mut self) {
        if self.session.activity.is_busy() {
            self.spinner = self.spinner.wrapping_add(1);
        }
    }

    /// Whether anything on screen is animating, so the loop knows to wake.
    pub fn animating(&self) -> bool {
        self.session.activity.is_busy()
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

        // Commit only what has scrolled above the live window.
        //
        // Committing at the end of every turn — which this used to do — froze all
        // but the current turn, so resizing re-wrapped almost nothing. The
        // TypeScript client kept its most recent entries live and archived only
        // what fell above them, which is why resizing there appeared to reflow
        // the conversation: everything on screen was still being re-rendered.
        // Bounded by rows rather than a fixed entry count, so it adapts to the
        // window instead of guessing at it.
        let live_from = self.live_window_start(inline, cols);
        // Never commit the current turn: a discard can still take it back, and
        // printed output cannot be unprinted.
        let settled = live_from.min(
            self.session.turn_start().unwrap_or_else(|| self.session.entries().len()),
        );
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

    /// The first entry that should stay live, walking back from the newest until
    /// the live region is full.
    ///
    /// Anything before it has scrolled out of reach and is committed to the
    /// terminal's scrollback, where it keeps the width it was written at.
    /// Everything from here on is redrawn each frame, so it re-wraps when the
    /// window changes size.
    ///
    /// An entry taller than the whole live region is *not* kept live. This used to
    /// keep the newest entry whatever its size, on the reasoning that something
    /// should always be re-wrappable — but the live block is then trimmed to fit
    /// the window, and the rows trimmed off it were printed nowhere at all. A
    /// single long message therefore showed its tail under an "↑ N more lines"
    /// marker with no way to reach the rest: not on screen, not in the
    /// scrollback, and not expandable. Measured on a resumed session in a
    /// 20-row window: "↑ 360 more lines", and the first message of the
    /// conversation absent from the terminal's history entirely.
    ///
    /// Left to be committed instead, such an entry is printed whole and scrolls
    /// into the scrollback like any other terminal output. It stops re-wrapping
    /// on resize, which is the unavoidable cost of being in the scrollback, and
    /// is plainly better than being unreadable.
    fn live_window_start(&self, inline: &crate::inline::Inline, cols: usize) -> usize {
        let entries = self.session.entries();
        // Chrome takes rows too; leave room for it rather than letting the
        // transcript push the prompt off the bottom.
        let budget = inline.live_capacity().saturating_sub(3).max(1);

        let mut used = 0usize;
        let mut start = entries.len();
        for index in (self.committed..entries.len()).rev() {
            // One entry, plus the blank line between entries.
            let rows = self.build_range(index..index + 1, cols).len() + 1;
            if used + rows > budget {
                break;
            }
            used += rows;
            start = index;
            if used >= budget {
                break;
            }
        }
        start
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
        chrome.extend(self.suggestion_lines(cols));
        let prompt_offset = chrome.len();
        let prompt = self.prompt_lines(cols);
        // The caret belongs on the last row of the input, not the first.
        let caret_offset = prompt_offset + prompt.len() - 1;
        chrome.extend(prompt);
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
                let marker = format!(
                    "  ↑ {hidden} more line{}",
                    if hidden == 1 { "" } else { "s" },
                );
                tail[0] = Line {
                    spans: vec![Span {
                        text: widgets::clip(&marker, cols),
                        style: Style::fg(palette::GRAY).dim(),
                    }],
                };
            }
        }

        let prompt_row = Some(tail.len() + caret_offset);
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
        let last = self.input.rsplit('\n').next().unwrap_or("");
        // The caret follows the text onto the last row it wrapped onto, so its
        // column is measured against that row rather than the whole line.
        let last_row = wrap_at(last, cols, lead).pop().unwrap_or_default();
        let typed = str_width(&last_row);
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
        let heading = format!("Subagents · {} running", subs.len());
        let mut out = vec![Line {
            spans: vec![Span {
                text: widgets::clip(&heading, cols),
                style: Style::fg(palette::MAGENTA),
            }],
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

    /// Commands matching what has been typed.
    fn suggestions(&self) -> Vec<&'static commands::Entry> {
        if self.dialog.is_some() || self.menu.is_some() {
            return Vec::new();
        }
        commands::complete(&self.input)
    }

    fn has_suggestions(&self) -> bool {
        !self.suggestions().is_empty()
    }

    /// The highlighted suggestion, if the list is showing.
    ///
    /// `None` once the input exactly matches a command, so Enter runs what was
    /// typed rather than re-selecting it — and so a trailing argument, as
    /// `/login chatgpt` has, is not thrown away.
    fn selected_suggestion(&self) -> Option<&'static commands::Entry> {
        let typed = self.input.trim();
        if commands::TABLE.iter().any(|e| e.command == typed) {
            return None;
        }
        let matches = self.suggestions();
        matches.get(self.suggestion.min(matches.len().saturating_sub(1))).copied()
    }

    /// Matching commands, shown while a `/` is being typed.
    fn suggestion_lines(&self, cols: usize) -> Vec<Line> {
        if self.dialog.is_some() || !commands::is_command(&self.input) {
            return Vec::new();
        }
        let matches = commands::complete(&self.input);
        if matches.is_empty() {
            return Vec::new();
        }
        let dim = Style::fg(palette::GRAY).dim();
        // Enough to choose from without pushing the transcript off the screen.
        let shown = matches.len().min(6);
        // Keep the highlight in view when it is past the visible few.
        let first = self.suggestion.saturating_sub(shown.saturating_sub(1));
        let window = &matches[first..(first + shown).min(matches.len())];
        let mut out: Vec<Line> = window
            .iter()
            .enumerate()
            .map(|(i, entry)| Line {
                spans: vec![
                    Span {
                        text: widgets::clip(
                            &format!(
                                "{} {:<12}",
                                if first + i == self.suggestion { "❯" } else { " " },
                                entry.command,
                            ),
                            cols,
                        ),
                        style: if first + i == self.suggestion {
                            Style::fg(palette::PROMPT).bold()
                        } else {
                            Style::fg(palette::PROMPT)
                        },
                    },
                    Span {
                        text: widgets::clip(entry.description, cols.saturating_sub(16)),
                        style: dim,
                    },
                ],
            })
            .collect();
        let hidden = matches.len().saturating_sub(shown);
        if hidden > 0 {
            let hint = format!("  … {hidden} more · ↑↓ move · Tab complete · Enter run");
            out.push(Line {
                spans: vec![Span { text: widgets::clip(&hint, cols), style: dim }],
            });
        }
        out
    }

    /// The prompt, one row per line of input.
    ///
    /// A multi-line message needs a row each, and continuation rows carry a dim
    /// marker rather than the prompt glyph so it is clear where the message
    /// starts.
    fn prompt_lines(&self, cols: usize) -> Vec<Line> {
        if self.dialog.is_some() {
            return vec![Line {
                spans: vec![Span {
                    text: "  (answer above)".into(),
                    style: Style::fg(palette::GRAY).dim(),
                }],
            }];
        }
        let lead = str_width(PROMPT_GLYPH);
        let bold = Style::fg(palette::PROMPT).bold();
        let dim = Style::fg(palette::GRAY).dim();

        if self.input.is_empty() {
            return vec![Line {
                spans: vec![
                    Span { text: PROMPT_GLYPH.into(), style: bold },
                    Span {
                        text: widgets::clip(PLACEHOLDER, cols.saturating_sub(lead)),
                        style: dim,
                    },
                ],
            }];
        }

        // Wrapped, not truncated. This used to keep only each line's tail, which
        // hid the beginning of the user's own text the moment a line outgrew the
        // window. The TypeScript client rendered the line into an ink `<Text>`,
        // which wraps, so the whole of it stayed readable — and wrapping is also
        // the only version that survives a narrowing terminal intact.
        let mut out: Vec<Line> = Vec::new();
        for (i, text) in self.input.split('\n').enumerate() {
            // A marker introduces a *logical* line. Rows a long line wraps onto are
            // indented to the text instead, matching how ink laid the prefix and
            // the text out as a flex row.
            let (marker, marker_style) = if i == 0 {
                (PROMPT_GLYPH.to_string(), bold)
            } else {
                ("· ".to_string(), dim)
            };
            for (j, segment) in wrap_at(text, cols, lead).into_iter().enumerate() {
                out.push(Line {
                    spans: vec![
                        Span {
                            text:  if j == 0 { marker.clone() } else { " ".repeat(lead) },
                            style: marker_style,
                        },
                        Span { text: segment, style: Style::default() },
                    ],
                });
            }
        }
        out
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
        // The active endpoint's reasoning setting, as the original bar showed it.
        if let Some(ep) = crate::model_display::active_endpoint(
            &self.session.endpoints,
            &self.session.model_name,
            &self.session.model_id,
            self.session.max_context_tokens,
        ) {
            if let Some(label) = crate::model_display::thinking_intensity(ep) {
                parts.push(label);
            }
        }

        if let Some(label) = self.session.activity.label() {
            // A turning spinner beside it, so a long turn looks alive rather than
            // stuck. Only while busy — an idle session animates nothing, which is
            // what keeps it at no CPU.
            parts.push(format!("{} {label}", SPINNER[self.spinner % SPINNER.len()]));
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

}

/// Shown when nothing has been typed, as in the TypeScript client — including
/// the hint about newlines, which the original advertised and this must honour.
const PLACEHOLDER: &str = "Type a message...  (\\+Enter or Ctrl+N for newline)";
/// The prompt, as in the TypeScript client.
const PROMPT_GLYPH: &str = "❯ ";

/// Spinner frames. Braille dots turn smoothly and take one cell.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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

/// Wrap a preformatted line to the space left after `used` columns.
///
/// Wrapped, not truncated. The TypeScript client rendered tool output through
/// ink's `<Text>`, which wraps by default, so cutting long lines here lost
/// content the original showed — and silently, which is the worst way to lose it.
/// Always returns at least one row, so callers can take the first unconditionally.
fn wrap_at(line: &str, cols: usize, used: usize) -> Vec<String> {
    let budget = cols.saturating_sub(used).max(1);
    let mut rows = crate::width::wrap(line, budget);
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
}

/// One line of tool output as one or more rows, tinted when it is a diff.
///
/// Added and removed lines get a background rather than only coloured text, so a
/// diff reads as blocks at a glance — the same treatment the TypeScript client
/// gave them. Every row of a wrapped diff line carries the tint, or a long change
/// would appear to stop being a change halfway through.
fn diff_lines(line: &str, cols: usize, dim: Style) -> Vec<Line> {
    let trimmed = line.trim_start();
    let style = match trimmed.chars().next() {
        Some('+') if !trimmed.starts_with("+++") => {
            Style::fg(palette::GREEN).bg(palette::DIFF_ADD_BG)
        }
        Some('-') if !trimmed.starts_with("---") => {
            Style::fg(palette::RED).bg(palette::DIFF_DEL_BG)
        }
        _ => dim,
    };
    wrap_at(line, cols, 6)
        .into_iter()
        .map(|row| Line {
            spans: vec![
                Span { text: "      ".into(), style: dim },
                Span { text: row, style },
            ],
        })
        .collect()
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

    // ── Multi-line input ──────────────────────────────────────────────────

    /// The placeholder advertises Ctrl+N, so it has to insert a newline rather
    /// than submit.
    #[test]
    fn ctrl_n_inserts_a_newline_without_submitting() {
        let (mut app, _rows) = app_with(0);
        for c in "first".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Newline, ROWS);
        assert!(effects.is_empty(), "nothing was sent");
        for c in "second".chars() {
            app.update(Input::Char(c), ROWS);
        }
        assert_eq!(app.input(), "first\nsecond");
    }

    /// The other advertised route: a trailing backslash turns Enter into a
    /// newline, and the backslash itself is consumed as the instruction it was.
    #[test]
    fn a_trailing_backslash_makes_enter_a_newline() {
        let (mut app, _rows) = app_with(0);
        for c in "line\\".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert!(effects.is_empty(), "not submitted");
        assert_eq!(app.input(), "line\n", "backslash consumed, newline added");
    }

    #[test]
    fn a_multiline_message_submits_whole() {
        let (mut app, _rows) = app_with(0);
        for c in "one".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Newline, ROWS);
        for c in "two".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::SendMessage { content: "one\ntwo".into() })],
        );
    }

    /// Each line needs its own row, or a multi-line message is invisible.
    #[test]
    fn a_multiline_input_draws_a_row_per_line() {
        let (mut app, _rows) = app_with(0);
        for c in "alpha".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Newline, ROWS);
        for c in "beta".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let shown = live_text(&mut app);
        assert!(shown.contains("❯ alpha"), "first row prompted: {shown:?}");
        assert!(shown.contains("· beta"), "continuation row marked");
    }

    /// The caret must sit on the last line, not the first.
    #[test]
    fn the_caret_follows_the_last_line_of_input() {
        let mut app = App::new();
        for c in "aaaa".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Newline, ROWS);
        for c in "bb".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let inline = crate::inline::Inline::new(COLS, ROWS);
        let block = app.live_lines(&inline, COLS);
        let (row, col) = app.cursor_in(&block, COLS).expect("a caret");
        assert_eq!(col, str_width(PROMPT_GLYPH) + 2, "after the shorter last line");
        assert_eq!(block.prompt_row, Some(row));
    }

    #[test]
    fn the_placeholder_advertises_the_newline_keys() {
        let (mut app, _rows) = app_with(0);
        let shown = live_text(&mut app);
        assert!(shown.contains("Ctrl+N"), "the hint is shown: {shown:?}");
    }

    // ── Spinner and reasoning label ───────────────────────────────────────

    /// A long turn should look alive. The spinner must only turn while busy,
    /// because animating when idle is what would cost CPU.
    #[test]
    fn the_spinner_turns_only_while_busy() {
        let (mut app, _rows) = app_with(0);
        assert!(!app.animating(), "idle to begin with");

        let before = live_text(&mut app);
        app.tick();
        assert_eq!(live_text(&mut app), before, "idle ticks change nothing");

        app.session_mut().apply(AgentMessage::Thinking);
        assert!(app.animating(), "a running turn animates");
        let busy = live_text(&mut app);
        app.tick();
        assert_ne!(live_text(&mut app), busy, "the frame advanced");

        app.session_mut().apply(AgentMessage::Done);
        assert!(!app.animating(), "and stops when the turn ends");
    }

    #[test]
    fn the_context_bar_shows_the_reasoning_setting() {
        use forge_agent_proto::{EndpointInfo, EndpointReasoningConfig, ProviderToggle};
        let mut app = App::new();
        let mut ep = EndpointInfo {
            name: "Claude".into(),
            base_url: "https://api.example.invalid".into(),
            model_id: "claude-1".into(),
            max_context_tokens: 200_000,
            max_output_tokens: 65_536,
            endpoint_type: "anthropic".into(),
            reasoning: EndpointReasoningConfig::default(),
            xai_priority_tier: false,
        };
        ep.reasoning.anthropic.thinking = ProviderToggle::On;
        ep.reasoning.anthropic.budget_tokens = 8192;

        app.session_mut().apply(AgentMessage::Init(Box::new(Init {
            model_name: "Claude".into(),
            model_id: "claude-1".into(),
            max_context_tokens: 200_000,
            endpoints: vec![ep],
            ..Default::default()
        })));

        let shown = live_text(&mut app);
        assert!(shown.contains("thinking high"), "got {shown:?}");
    }

    // ── Slash commands ────────────────────────────────────────────────────

    /// The command the user actually needed: resuming a conversation.
    #[test]
    fn resume_opens_the_sessions_page() {
        let (mut app, _rows) = app_with(0);
        for c in "/resume".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (outcome, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(outcome, Outcome::Continue);
        assert!(effects.is_empty(), "opening a menu sends nothing");
        assert!(app.menu_open(), "the sessions page is open");
    }

    /// A command must not be sent to the model, which would answer questions
    /// about it instead of running it.
    #[test]
    fn a_command_is_not_sent_as_a_message() {
        let (mut app, _rows) = app_with(0);
        for c in "/compact".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(
            effects,
            vec![Effect::Send(ClientMessage::Compact)],
            "ran the command rather than sending the text",
        );
        assert!(
            !app.session().entries().iter().any(|e| e.content.contains("/compact")),
            "and it does not appear as a user message",
        );
    }

    #[test]
    fn commands_map_to_their_messages() {
        for (typed, expected) in [
            ("/clear", ClientMessage::ClearSession),
            ("/compact", ClientMessage::Compact),
            ("/usage", ClientMessage::RequestUsage),
            ("/plan", ClientMessage::EnterPlanMode),
            ("/login", ClientMessage::LoginChatgpt),
        ] {
            let (mut app, _rows) = app_with(0);
            for c in typed.chars() {
                app.update(Input::Char(c), ROWS);
            }
            let (_, effects) = app.update(Input::Enter, ROWS);
            assert_eq!(effects, vec![Effect::Send(expected)], "{typed}");
        }
    }

    #[test]
    fn quit_commands_quit() {
        for typed in ["/quit", "/exit"] {
            let (mut app, _rows) = app_with(0);
            for c in typed.chars() {
                app.update(Input::Char(c), ROWS);
            }
            assert_eq!(app.update(Input::Enter, ROWS).0, Outcome::Quit, "{typed}");
        }
    }

    #[test]
    fn restart_asks_for_a_restart_without_a_session() {
        let (mut app, _rows) = app_with(0);
        for c in "/restart".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(effects, vec![Effect::Restart { resume: None }]);
    }

    /// An unknown command must be reported, not handed to the model.
    #[test]
    fn an_unknown_command_is_reported_locally() {
        let (mut app, _rows) = app_with(0);
        for c in "/nonsense".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert!(effects.is_empty(), "nothing sent to the agent");
        let last = app.session().entries().last().unwrap();
        assert!(last.content.contains("not a command"), "got {:?}", last.content);
    }

    #[test]
    fn help_lists_the_commands_in_the_transcript() {
        let (mut app, _rows) = app_with(0);
        for c in "/help".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Enter, ROWS);
        let last = app.session().entries().last().unwrap();
        assert!(last.content.contains("/resume"), "got {:?}", last.content);
    }

    /// Suggestions have to appear, or the commands are invisible.
    #[test]
    fn typing_a_slash_shows_suggestions() {
        let (mut app, _rows) = app_with(0);
        app.update(Input::Char('/'), ROWS);
        let shown = live_text(&mut app);
        assert!(shown.contains("/quit"), "suggestions are drawn: {shown:?}");
        assert!(shown.contains("Exit Forge"), "with descriptions");
        // The list is capped so it cannot push the transcript off the screen, and
        // says how much it left out rather than silently truncating.
        assert!(shown.contains("more"), "the remainder is acknowledged: {shown:?}");
    }

    /// Narrowing has to reach the commands that are past the visible few.
    #[test]
    fn a_prefix_surfaces_a_command_that_was_below_the_cut() {
        let (mut app, _rows) = app_with(0);
        for c in "/res".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let shown = live_text(&mut app);
        assert!(shown.contains("/resume"), "got {shown:?}");
        assert!(shown.contains("Resume a saved session"));
    }

    #[test]
    fn suggestions_narrow_as_you_type_and_vanish_for_plain_text() {
        let (mut app, _rows) = app_with(0);
        for c in "/mod".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let shown = live_text(&mut app);
        assert!(shown.contains("/model"), "got {shown:?}");
        assert!(!shown.contains("/resume"), "unrelated commands are filtered out");

        let (mut app, _rows) = app_with(0);
        for c in "hello".chars() {
            app.update(Input::Char(c), ROWS);
        }
        assert!(!live_text(&mut app).contains("/model"), "no suggestions for prose");
    }

    #[test]
    fn tab_completes_a_unique_command() {
        let (mut app, _rows) = app_with(0);
        for c in "/comp".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Complete, ROWS);
        assert_eq!(app.input(), "/compact");
    }

    /// Tab must not guess when several commands match.
    /// Tab takes the highlighted suggestion, which is what the prompt's hint
    /// promises. Refusing to act when several match would make the hint a lie.
    #[test]
    fn tab_takes_the_highlighted_suggestion() {
        let (mut app, _rows) = app_with(0);
        for c in "/se".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Complete, ROWS);
        assert!(
            app.input() == "/settings" || app.input() == "/sessions",
            "completed to a real command, got {:?}", app.input(),
        );
    }

    /// The bug this replaced: Enter on a half-typed command reported it as
    /// unknown instead of running the highlighted one.
    #[test]
    fn enter_runs_the_highlighted_suggestion_not_the_partial_text() {
        let (mut app, _rows) = app_with(0);
        for c in "/resu".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (outcome, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(outcome, Outcome::Continue);
        assert!(effects.is_empty(), "opening a menu sends nothing");
        assert!(app.menu_open(), "/resume ran: {:?}", app.session().entries().last());
        assert!(
            !app.session().entries().iter().any(|e| e.content.contains("not a command")),
            "nothing was reported as unknown",
        );
    }

    /// `/res` is ambiguous — it matches `/restart` and `/resume`, and the first
    /// in the table is highlighted. Worth pinning, because the two do very
    /// different things and the order decides which Enter runs.
    #[test]
    fn an_ambiguous_prefix_highlights_the_first_match_and_the_rest_are_reachable() {
        let (mut app, _rows) = app_with(0);
        for c in "/res".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(
            effects,
            vec![Effect::Restart { resume: None }],
            "/restart is first in the table, so it is what Enter runs",
        );

        // One press down reaches /resume.
        let (mut app, _rows) = app_with(0);
        for c in "/res".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Down, ROWS);
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert!(effects.is_empty(), "a menu, not a restart");
        assert!(app.menu_open());
    }

    /// Arrows move through the suggestions, as the hint says.
    #[test]
    fn arrows_move_through_the_suggestions() {
        let (mut app, _rows) = app_with(0);
        for c in "/se".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Complete, ROWS);
        let first = app.input().to_string();

        let (mut app, _rows) = app_with(0);
        for c in "/se".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Down, ROWS);
        app.update(Input::Complete, ROWS);
        assert_ne!(app.input(), first, "moving changed the choice");
    }

    /// A fully typed command with an argument must survive Enter, not be
    /// replaced by a re-selected suggestion.
    #[test]
    fn a_command_with_an_argument_is_not_overwritten() {
        let (mut app, _rows) = app_with(0);
        for c in "/login chatgpt".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let (_, effects) = app.update(Input::Enter, ROWS);
        assert_eq!(effects, vec![Effect::Send(ClientMessage::LoginChatgpt)]);
    }

    /// Arrows still do nothing when no suggestions are open, so they cannot
    /// silently swallow input.
    #[test]
    fn arrows_are_inert_without_suggestions() {
        let (mut app, _rows) = app_with(0);
        for c in "hello".chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Up, ROWS);
        app.update(Input::Down, ROWS);
        assert_eq!(app.input(), "hello");
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

    /// Long tool output must wrap, not vanish. Truncating lost content the
    /// original showed, and lost it silently.
    #[test]
    fn long_tool_output_wraps_rather_than_being_truncated() {
        let mut app = App::new();
        let long = "the quick brown fox jumps over the lazy dog and keeps on going                     for quite a while longer than any terminal is wide";
        app.session_mut().apply(AgentMessage::ToolOutput {
            tool_name: "shell_exec".into(),
            content: long.into(),
        });

        let lines = app.build_lines(40);
        assert!(lines.len() > 1, "wrapped onto several rows");
        for line in &lines {
            assert!(line.width() <= 40, "{:?} overflows", line.plain());
        }
        // Nothing was dropped.
        let joined: String = lines.iter().map(|l| l.plain().trim().to_string())
            .collect::<Vec<_>>().join(" ");
        for word in ["quick", "lazy", "longer", "wide"] {
            assert!(joined.contains(word), "{word:?} was lost: {joined:?}");
        }
    }

    /// A long tool *result* wraps under its glyph rather than being cut.
    #[test]
    fn a_long_tool_result_wraps_under_its_glyph() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::ToolResult {
            tool_name: "shell_exec".into(),
            result: "a result line that is considerably longer than the terminal width".into(),
            success: true,
            subagent_id: None,
        });
        let lines = app.build_lines(30);
        assert!(lines.len() > 1, "wrapped");
        assert!(lines[0].plain().contains('⎿'), "the glyph leads the first row");
        assert!(lines[1].plain().starts_with("      "), "continuations indent to match");
        for line in &lines {
            assert!(line.width() <= 30);
        }
    }

    /// Every row of a wrapped diff line keeps the tint, or a long change appears
    /// to stop being a change halfway through.
    #[test]
    fn a_wrapped_diff_line_stays_tinted_throughout() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::ToolOutput {
            tool_name: "apply_patch".into(),
            content: format!("+{}", "added text ".repeat(12)),
        });
        let lines = app.build_lines(30);
        assert!(lines.len() > 1, "wrapped: {:?}", lines.len());
        for line in &lines {
            let tinted = line.spans.iter().any(|s| s.style.bg == Some(palette::DIFF_ADD_BG));
            assert!(tinted, "row not tinted: {:?}", line.plain());
        }
    }

    // ── Resizing ──────────────────────────────────────────────────────────

    /// Recent entries stay live so a resize re-wraps them. Committing at the end
    /// of every turn froze all but the current one, and resizing then re-wrapped
    /// almost nothing on screen.
    #[test]
    fn recent_entries_stay_live_after_a_turn_ends() {
        let mut app = App::new();
        app.session_mut().push_user("a question");
        app.session_mut().apply(AgentMessage::AssistantToken { content: "an answer".into() });
        app.session_mut().apply(AgentMessage::Done);

        let mut inline = crate::inline::Inline::new(COLS, ROWS);
        let mut out: Vec<u8> = Vec::new();
        app.render(&mut inline, &mut out).unwrap();

        assert_eq!(app.committed, 0, "a finished turn is still redrawable");
        let shown = live_text(&mut app);
        assert!(shown.contains("an answer"), "and still on screen: {shown:?}");
    }

    /// The reported bug: a message too tall for the window showed its tail under
    /// an "↑ N more lines" marker, and the rest was nowhere — not on screen, not in
    /// the scrollback, and not reachable. Measured on a resumed session in a
    /// 20-row window: "↑ 360 more lines", with the whole of that message absent
    /// from the terminal's history.
    ///
    /// Such an entry has to be committed instead, so it is printed whole and
    /// scrolls into the scrollback like any other output.
    #[test]
    fn a_message_taller_than_the_window_is_printed_whole() {
        let mut app = App::new();
        app.session_mut().push_user("a question");
        // Far more rows than the live region can hold, each line identifiable.
        let long: String =
            (0..200).map(|i| format!("line{i:03} of a very long reply\n")).collect();
        app.session_mut().apply(AgentMessage::AssistantMessage { content: long });
        app.session_mut().apply(AgentMessage::Done);

        let mut inline = crate::inline::Inline::new(COLS, ROWS);
        let mut out: Vec<u8> = Vec::new();
        app.render(&mut inline, &mut out).unwrap();

        let printed = visible(&String::from_utf8_lossy(&out));
        // Every row reached the terminal, top included.
        for i in [0usize, 1, 99, 150, 199] {
            assert!(
                printed.contains(&format!("line{i:03}")),
                "line{i:03} was never printed",
            );
        }
        // And nothing was hidden behind a marker.
        assert!(
            !printed.contains("more lines"),
            "content was hidden instead of printed: {:?}",
            printed.lines().filter(|l: &&str| l.contains("more lines")).collect::<Vec<_>>(),
        );
        assert!(app.committed > 0, "the oversized entry was committed");
    }

    /// Which means a resize re-wraps it, rather than leaving it at the old width.
    #[test]
    fn a_resize_rewraps_a_finished_turn() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::AssistantToken {
            content: "an answer long enough that its wrapping depends entirely on how                       wide the terminal happens to be at the time".into(),
        });
        app.session_mut().apply(AgentMessage::Done);

        let narrow = crate::inline::Inline::new(40, ROWS);
        let wide = crate::inline::Inline::new(100, ROWS);
        let narrow_rows = app.live_lines(&narrow, 40).lines.len();
        let wide_rows = app.live_lines(&wide, 100).lines.len();
        assert!(
            narrow_rows > wide_rows,
            "the finished turn re-wraps: {narrow_rows} rows at 40 vs {wide_rows} at 100",
        );
    }

    /// History still has to reach the scrollback, or the transcript would grow
    /// without bound in a region that is redrawn every frame.
    #[test]
    fn entries_that_scroll_out_of_reach_are_committed() {
        let mut app = App::new();
        for i in 0..60 {
            app.session_mut().push_system(format!("entry number {i}"));
        }
        let mut inline = crate::inline::Inline::new(COLS, ROWS);
        let mut out: Vec<u8> = Vec::new();
        app.render(&mut inline, &mut out).unwrap();

        assert!(app.committed > 0, "older entries were printed to scrollback");
        assert!(
            app.committed < 60,
            "but not all of them — the newest stay live, got {}", app.committed,
        );
        // Escapes stripped: each word is its own styled span, so the phrase is
        // not a contiguous substring of the raw output.
        let text = visible(&String::from_utf8_lossy(&out));
        assert!(text.contains("entry number 0"), "the oldest was committed");
        // The newest is in the live block, not the committed prefix — both reach
        // the same stream, so the distinction is which range produced them.
        let live = live_text(&mut app);
        assert!(live.contains("entry number 59"), "the newest is live: {live:?}");
        assert!(
            !live.contains("entry number 0"),
            "and the oldest is not, having scrolled out of reach",
        );
    }

    /// The current turn is never committed, because a discard can still take it
    /// back and printed output cannot be unprinted.
    #[test]
    fn a_turn_in_progress_is_never_committed() {
        let mut app = App::new();
        for i in 0..60 {
            app.session_mut().push_system(format!("filler {i}"));
        }
        let mut inline = crate::inline::Inline::new(COLS, ROWS);
        let mut out: Vec<u8> = Vec::new();
        app.render(&mut inline, &mut out).unwrap();
        let before = app.committed;

        // Open a turn, then produce a lot within it.
        app.session_mut().apply(AgentMessage::Thinking);
        for _ in 0..40 {
            app.session_mut().apply(AgentMessage::ToolOutput {
                tool_name: "t".into(),
                content: "a line of output".into(),
            });
        }
        app.render(&mut inline, &mut out).unwrap();

        let turn_start = app.session().turn_start().expect("a turn is open");
        assert!(
            app.committed <= turn_start,
            "committed {} must not pass the turn start {turn_start}", app.committed,
        );
        assert!(app.committed >= before, "and never goes backwards");
    }

    /// Narrowing the terminal must re-wrap, not clip.
    #[test]
    fn narrowing_rewraps_the_transcript() {
        let mut app = App::new();
        app.session_mut().apply(AgentMessage::AssistantToken {
            content: "a reply long enough that its wrapping depends entirely on the width                       of the terminal it is being shown in".into(),
        });
        let wide = app.build_lines(100);
        let narrow = app.build_lines(30);
        assert!(narrow.len() > wide.len(), "narrower means more rows");
        for line in &narrow {
            assert!(line.width() <= 30, "{:?} overflows", line.plain());
        }
    }

    /// Every width must produce rows that fit it — this is the invariant that,
    /// when broken, made messages overlap.
    #[test]
    fn every_width_produces_rows_that_fit() {
        let mut app = App::new();
        app.session_mut().push_user("日本語のテキスト with **bold** and `code`");
        app.session_mut().apply(AgentMessage::ToolOutput {
            tool_name: "t".into(),
            content: "+added 日本語 line\n-removed line\n plain".into(),
        });
        app.session_mut().apply(AgentMessage::AssistantToken {
            content: "- a bullet that runs on\n\n```rust\nlet x = 1;\n```".into(),
        });
        for cols in [8usize, 12, 20, 31, 40, 79, 80, 120] {
            for line in app.build_lines(cols) {
                assert!(
                    line.width() <= cols,
                    "at {cols} cols, {:?} is {} cells",
                    line.plain(), line.width(),
                );
            }
        }
    }

    /// A resize must not leave the live block wider than the new terminal.
    #[test]
    fn the_live_block_fits_after_a_resize() {
        let mut app = App::new();
        for i in 0..12 {
            app.session_mut().push_system(format!("entry {i} with some length to it"));
        }
        for c in "typed text".chars() {
            app.update(Input::Char(c), ROWS);
        }
        for (cols, rows) in [(100usize, 30usize), (24, 10), (40, 6), (12, 4)] {
            let inline = crate::inline::Inline::new(cols, rows);
            let block = app.live_lines(&inline, cols);
            assert!(
                block.lines.len() <= inline.live_capacity(),
                "block of {} rows exceeds capacity {} at {cols}x{rows}",
                block.lines.len(), inline.live_capacity(),
            );
            for line in &block.lines {
                assert!(line.width() <= cols, "{:?} overflows {cols}", line.plain());
            }
        }
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

    /// Paragraphs keep their shape at any width: the blank line between them
    /// survives, and each paragraph re-wraps on its own rather than running
    /// together.
    #[test]
    fn paragraphs_rewrap_and_keep_the_gap_between_them() {
        let (mut app, _rows) = app_with(0);
        let text = "Paragraph one runs on for a while so that it has to wrap \
several times over at any sensible terminal width.\n\nParagraph two is also \
long enough to wrap, and must stay separated from the first.";
        app.session_mut().apply(AgentMessage::AssistantMessage { content: text.into() });

        for cols in [100usize, 60, 30, 20] {
            let lines = app.lines_for(0..app.session().entries().len(), cols);
            let plain: Vec<String> = lines.iter().map(|l| l.plain()).collect();
            for line in &lines {
                assert!(line.width() <= cols, "at {cols}: {:?} overflows", line.plain());
            }
            // Every word survives the wrap, in order.
            let joined = plain.join(" ").split_whitespace().collect::<Vec<_>>().join(" ");
            for word in ["Paragraph", "one", "sensible", "two", "separated", "first."] {
                assert!(joined.contains(word), "at {cols}: lost {word:?} in {joined:?}");
            }
            // The paragraphs stay apart: a blank row between the last row of the
            // first and the first row of the second.
            let first_end = plain.iter().position(|p| p.contains("width.")).unwrap();
            let second_start = plain.iter().position(|p| p.contains("Paragraph two")
                || (p.contains("two") && !p.contains("one"))).unwrap();
            assert!(second_start > first_end, "at {cols}: paragraphs out of order");
            assert!(
                plain[first_end + 1..second_start].iter().any(|p| p.trim().is_empty()),
                "at {cols}: no blank row between paragraphs: {plain:?}",
            );
        }
    }

    /// Narrow far enough and one line has to become three or more. Every row still
    /// fits, and nothing is dropped on the way.
    #[test]
    fn one_line_becoming_several_keeps_all_of_it() {
        let (mut app, _rows) = app_with(0);
        let sentence: String =
            (0..14).map(|i| format!("word{i:02} ")).collect::<String>().trim_end().to_string();
        app.session_mut()
            .apply(AgentMessage::AssistantMessage { content: sentence.clone() });

        let wide = app.lines_for(0..app.session().entries().len(), 120);
        let narrow = app.lines_for(0..app.session().entries().len(), 24);
        assert!(
            narrow.len() >= wide.len() + 2,
            "24 columns should need several more rows than 120: {} vs {}",
            narrow.len(), wide.len(),
        );
        for line in &narrow {
            assert!(line.width() <= 24, "{:?} overflows 24", line.plain());
        }
        let joined = narrow.iter().map(|l| l.plain()).collect::<Vec<_>>().join(" ");
        for i in 0..14 {
            assert!(joined.contains(&format!("word{i:02}")), "lost word{i:02}: {joined:?}");
        }
    }

    /// A long line wraps, so all of what was typed stays readable. It used to keep
    /// only the tail, which hid the start of the user's own text — and the
    /// TypeScript client wrapped it (an ink `<Text>`, which has no other mode).
    #[test]
    fn long_input_wraps_rather_than_hiding_its_start() {
        let (mut app, _rows) = app_with(0);
        let typed: String = (0..8).map(|i| format!("segment{i} ")).collect();
        for c in typed.chars() {
            app.update(Input::Char(c), ROWS);
        }
        let lines = app.prompt_lines(40);
        assert!(lines.len() > 1, "a line longer than the window has to wrap: {lines:?}");
        for line in &lines {
            assert!(line.width() <= 40, "{:?} overflows", line.plain());
        }
        let joined: String = lines.iter().map(|l| l.plain()).collect::<Vec<_>>().join("");
        assert!(joined.contains("segment0"), "the beginning is still on screen");
        assert!(joined.contains("segment7"), "and so is the end");
    }

    /// Only a real newline earns a `·`. Rows that a single long line wrapped onto
    /// are indented instead, or a wrapped line would read as several lines.
    #[test]
    fn wrapped_rows_are_indented_and_new_lines_are_marked() {
        let (mut app, _rows) = app_with(0);
        for c in "x".repeat(90).chars() {
            app.update(Input::Char(c), ROWS);
        }
        app.update(Input::Newline, ROWS);
        for c in "short".chars() {
            app.update(Input::Char(c), ROWS);
        }
        let lines = app.prompt_lines(40);
        let plain: Vec<String> = lines.iter().map(|l| l.plain()).collect();
        assert!(plain[0].starts_with(PROMPT_GLYPH), "the first row is prompted: {plain:?}");
        // 90 x's at 38 usable columns is three rows; the two after the first carry
        // no marker.
        assert!(!plain[1].trim_start().starts_with('·'), "wrapped row marked: {plain:?}");
        assert!(!plain[2].trim_start().starts_with('·'), "wrapped row marked: {plain:?}");
        let second = plain.iter().find(|p| p.contains("short")).expect("the second line");
        assert!(second.starts_with('·'), "a real newline is marked: {second:?}");
        for line in &lines {
            assert!(line.width() <= 40, "{:?} overflows", line.plain());
        }
    }

    /// The caret follows the text onto the row it wrapped onto.
    #[test]
    fn the_caret_lands_on_the_last_wrapped_row() {
        let (mut app, rows) = app_with(0);
        for c in "y".repeat(50).chars() {
            app.update(Input::Char(c), ROWS);
        }
        let block = app.live_lines(&crate::inline::Inline::new(40, rows), 40);
        let (row, col) = app.cursor_in(&block, 40).expect("a caret");
        assert_eq!(block.lines[row].plain().trim_end().len() % 40, col % 40,
                   "the caret sits at the end of the row it is on");
        assert!(col < 40, "and inside the window");
    }
}
