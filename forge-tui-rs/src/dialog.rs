// SPDX-License-Identifier: Apache-2.0
//! Modal prompts: approvals, questions, plans, process input, rewind.
//!
//! A [`Dialog`] is built from whatever the session is [`Pending`] on, owns only
//! its own cursor and typed text, and answers with a [`Decision`]. It never
//! touches the session — the caller applies the decision — so the key handling
//! and the option arithmetic are testable without a session or a terminal.
//!
//! Two behaviours are deliberate rather than inherited:
//!
//!  * **"Other" drops into free text.** A question with choices still lets the
//!    user write something the agent did not think of, matching the TypeScript
//!    dialog. Choosing it switches modes rather than answering.
//!  * **Escape is never silent.** On an approval it *denies*, because the agent
//!    is blocked waiting and dismissing the dialog with no reply would hang the
//!    turn with nothing on screen to explain it.

use forge_agent_proto::QuestionItem;

use crate::app::Input;
use crate::screen::{Screen, Style};
use crate::session::Pending;
use crate::widgets::{self, Rect, Row};

/// What the user decided. The caller turns this into session calls.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Approve { remember: bool },
    Deny,
    /// Free text, or a chosen option's label.
    Answer(String),
    ApprovePlan { clear_context: bool },
    /// Turn on the provider's priority tier for the endpoint that was refused.
    SwitchToPriorityTier,
    /// Keep the capacity rejection as an ordinary error and move on.
    DismissProviderBusy,
    /// Dismissed without answering. Only offered where the agent is not blocked.
    Cancel,
}

/// Which prompt this is, with the options it offers.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    Approval { tool_name: String },
    Plan,
    Question { multi_select: bool },
    /// Free text only — a process waiting on stdin has nothing to choose from.
    FreeText,
    Rewind,
    ProviderBusy,
}

pub struct Dialog {
    kind:     Kind,
    title:    String,
    body:     String,
    /// Option labels with their descriptions. Empty for a free-text prompt.
    options:  Vec<(String, String)>,
    selected: usize,
    /// Indices ticked in a multi-select question.
    checked:  Vec<usize>,
    /// `Some` while typing a custom answer.
    typing:   Option<String>,
}

/// The label that switches a choice list into free text.
const OTHER: &str = "Other";

impl Dialog {
    /// Build the dialog for what the agent is waiting on.
    pub fn for_pending(pending: &Pending) -> Self {
        match pending {
            Pending::Approval { tool_name, tool_args, kind, .. } => Self {
                kind: Kind::Approval { tool_name: tool_name.clone() },
                title: format!("Run {tool_name}?"),
                // Arguments are the substance of the decision — approving a
                // shell command without seeing it is not consent.
                body: format!("{kind} · {}", compact_args(tool_args)),
                options: vec![
                    ("Yes".into(), "run it once".into()),
                    ("Yes, always".into(), format!("don't ask again for {tool_name}")),
                    ("No".into(), "deny and tell the agent".into()),
                ],
                selected: 0,
                checked: Vec::new(),
                typing: None,
            },

            Pending::ProviderBusy { message, .. } => Self {
                kind:  Kind::ProviderBusy,
                title: message.clone(),
                // Whose charge this is, said plainly. The wording is the
                // TypeScript client's, which was careful about the same thing.
                body: "Priority requests higher scheduling priority from xAI during \
high demand, at double the standard per-token price. This is a charge from xAI, \
not Forge."
                    .into(),
                options: vec![
                    ("Switch to priority tier".into(), "2x cost, from xAI".into()),
                    ("Dismiss".into(), "leave the tier alone".into()),
                ],
                selected: 0,
                checked: Vec::new(),
                typing: None,
            },

            Pending::Plan { plan_path, content } => Self {
                kind: Kind::Plan,
                title: "Plan ready".into(),
                body: format!("{content}\n\n({plan_path})"),
                options: vec![
                    ("Approve".into(), "start work".into()),
                    ("Approve, clear context".into(), "start fresh".into()),
                    (OTHER.into(), "reject with feedback".into()),
                ],
                selected: 0,
                checked: Vec::new(),
                typing: None,
            },

            Pending::Question { question, items, .. } => {
                let item: Option<&QuestionItem> = items.first();
                let multi_select = item.map(|i| i.multi_select).unwrap_or(false);

                // The agent may already offer an "Other"; don't show it twice.
                let mut options: Vec<(String, String)> = item
                    .map(|i| {
                        i.options
                            .iter()
                            .filter(|o| !o.label.eq_ignore_ascii_case(OTHER))
                            .map(|o| (o.label.clone(), o.description.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                if !options.is_empty() {
                    options.push((OTHER.into(), "write your own answer".into()));
                }

                let prompt = item.map(|i| i.question.clone()).unwrap_or_else(|| question.clone());
                let header = item
                    .map(|i| i.header.clone())
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| "Question".into());

                Self {
                    kind: Kind::Question { multi_select },
                    title: header,
                    body: prompt,
                    // With no choices there is nothing to select, so start typing.
                    typing: options.is_empty().then(String::new),
                    options,
                    selected: 0,
                    checked: Vec::new(),
                }
            }

            Pending::ProcessInput { prompt } => Self {
                kind: Kind::FreeText,
                title: "Input needed".into(),
                body: prompt.clone(),
                options: Vec::new(),
                selected: 0,
                checked: Vec::new(),
                typing: Some(String::new()),
            },

            Pending::BackgroundInput { command, prompt, .. } => Self {
                kind: Kind::FreeText,
                title: format!("Input needed — {}", first_line(command)),
                body: prompt.clone(),
                options: Vec::new(),
                selected: 0,
                checked: Vec::new(),
                typing: Some(String::new()),
            },

            Pending::Rewind { preview, summary, .. } => Self {
                kind: Kind::Rewind,
                title: "Rewind?".into(),
                body: format!("{summary}\n\n{preview}"),
                options: vec![
                    ("Yes".into(), "rewind to here".into()),
                    ("No".into(), "stay where you are".into()),
                ],
                selected: 0,
                checked: Vec::new(),
                typing: None,
            },
        }
    }

    pub fn is_typing(&self) -> bool {
        self.typing.is_some()
    }

    pub fn typed(&self) -> &str {
        self.typing.as_deref().unwrap_or("")
    }

    /// Handle one keypress. `None` means the dialog stays open.
    pub fn handle(&mut self, input: Input) -> Option<Decision> {
        // Typing takes precedence: every printable key is text, so a custom
        // answer containing "y" or "n" is not intercepted as a shortcut.
        if self.typing.is_some() {
            return self.handle_typing(input);
        }

        match input {
            Input::Up => {
                self.selected = if self.selected == 0 {
                    self.options.len().saturating_sub(1)
                } else {
                    self.selected - 1
                };
                None
            }
            Input::Down => {
                self.selected = if self.options.is_empty() {
                    0
                } else {
                    (self.selected + 1) % self.options.len()
                };
                None
            }

            // Space ticks a box in a multi-select question.
            Input::Char(' ') if matches!(self.kind, Kind::Question { multi_select: true }) => {
                if let Some(pos) = self.checked.iter().position(|i| *i == self.selected) {
                    self.checked.remove(pos);
                } else {
                    self.checked.push(self.selected);
                }
                None
            }

            Input::Enter => self.confirm(),

            // Single-key shortcuts, only where they are unambiguous.
            Input::Char(c) => self.shortcut(c),

            // Escape must not leave the agent blocked with nothing on screen.
            Input::Escape => Some(self.escape()),

            _ => None,
        }
    }

    fn handle_typing(&mut self, input: Input) -> Option<Decision> {
        let buffer = self.typing.as_mut().expect("typing");
        match input {
            Input::Char(c) => {
                buffer.push(c);
                None
            }
            Input::Paste(text) => {
                buffer.push_str(&text.replace(['\n', '\r'], " "));
                None
            }
            Input::Backspace => {
                use unicode_segmentation::UnicodeSegmentation;
                if let Some(last) = buffer.graphemes(true).next_back() {
                    let keep = buffer.len() - last.len();
                    buffer.truncate(keep);
                } else if !self.options.is_empty() {
                    // Backspacing past the start returns to the choices, rather
                    // than trapping the user in a text field they opened.
                    self.typing = None;
                }
                None
            }
            Input::Enter => {
                let text = buffer.trim().to_string();
                if text.is_empty() {
                    return None; // an empty answer is not an answer
                }
                match self.kind {
                    Kind::Plan => Some(Decision::Answer(text)), // rejection feedback
                    _ => Some(Decision::Answer(text)),
                }
            }
            Input::Escape => {
                // Leave the text field, back to the options if there are any.
                if self.options.is_empty() {
                    Some(self.escape())
                } else {
                    self.typing = None;
                    None
                }
            }
            _ => None,
        }
    }

    /// Act on the highlighted option.
    fn confirm(&mut self) -> Option<Decision> {
        let label = self.options.get(self.selected).map(|(l, _)| l.as_str())?;

        if label == OTHER {
            self.typing = Some(String::new());
            return None;
        }

        match &self.kind {
            Kind::Approval { .. } => Some(match self.selected {
                0 => Decision::Approve { remember: false },
                1 => Decision::Approve { remember: true },
                _ => Decision::Deny,
            }),
            Kind::Plan => Some(Decision::ApprovePlan { clear_context: self.selected == 1 }),
            Kind::ProviderBusy => Some(if self.selected == 0 {
                Decision::SwitchToPriorityTier
            } else {
                Decision::DismissProviderBusy
            }),
            Kind::Rewind => Some(if self.selected == 0 {
                Decision::Approve { remember: false }
            } else {
                Decision::Deny
            }),
            Kind::Question { multi_select } => {
                if *multi_select {
                    // Nothing ticked means the highlighted row, which is what a
                    // user pressing Enter straight away intends.
                    let mut chosen: Vec<usize> =
                        if self.checked.is_empty() { vec![self.selected] } else { self.checked.clone() };
                    chosen.sort_unstable();
                    let joined = chosen
                        .iter()
                        .filter_map(|i| self.options.get(*i))
                        .map(|(l, _)| l.clone())
                        .collect::<Vec<_>>()
                        .join(", ");
                    Some(Decision::Answer(joined))
                } else {
                    Some(Decision::Answer(label.to_string()))
                }
            }
            Kind::FreeText => None,
        }
    }

    /// Single-key shortcuts.
    ///
    /// Deliberately **no** shortcut for "always allow". Granting a tool blanket
    /// permission for the session is the one choice here that is hard to undo,
    /// and a bare letter is reachable by accident — a user typing when the prompt
    /// appears would hand over standing approval without ever deciding to. It is
    /// available by selecting it, which takes an arrow key and Enter. Approving
    /// once and denying stay on shortcuts: one is the highlighted default anyway,
    /// and the other is refusal.
    fn shortcut(&mut self, c: char) -> Option<Decision> {
        match (&self.kind, c.to_ascii_lowercase()) {
            (Kind::Approval { .. }, 'y') => Some(Decision::Approve { remember: false }),
            (Kind::Approval { .. }, 'n') => Some(Decision::Deny),
            (Kind::Rewind, 'y') => Some(Decision::Approve { remember: false }),
            (Kind::Rewind, 'n') => Some(Decision::Deny),
            (Kind::Plan, 'y') => Some(Decision::ApprovePlan { clear_context: false }),
            (Kind::Plan, 'c') => Some(Decision::ApprovePlan { clear_context: true }),
            // The shortcuts the TypeScript client bound: `p` to switch, `d` to
            // dismiss. Both are easily undone, unlike standing approval.
            (Kind::ProviderBusy, 'p') => Some(Decision::SwitchToPriorityTier),
            (Kind::ProviderBusy, 'd') => Some(Decision::DismissProviderBusy),
            // A numbered pick, for a list of choices.
            (Kind::Question { multi_select: false }, d) if d.is_ascii_digit() => {
                let idx = (d as u8 - b'1') as usize;
                let label = self.options.get(idx).map(|(l, _)| l.clone())?;
                if label == OTHER {
                    self.typing = Some(String::new());
                    return None;
                }
                Some(Decision::Answer(label))
            }
            _ => None,
        }
    }

    /// What Escape means here.
    ///
    /// Never a silent dismissal where the agent is blocked: an approval becomes
    /// a denial so the turn can proceed, and a question the agent is waiting on
    /// cannot be walked away from.
    fn escape(&self) -> Decision {
        match self.kind {
            Kind::Approval { .. } | Kind::Rewind => Decision::Deny,
            // Escape here is "leave it as it was", which still records the error
            // rather than losing it — the agent is not waiting on this.
            Kind::ProviderBusy => Decision::DismissProviderBusy,
            Kind::Plan | Kind::Question { .. } | Kind::FreeText => Decision::Cancel,
        }
    }

    /// How many rows this dialog wants, given a width.
    pub fn height(&self, cols: usize) -> usize {
        let inner = cols.saturating_sub(4);
        let body = crate::width::wrap(&self.body, inner.max(1)).len().min(8);
        let options = if self.typing.is_some() { 1 } else { self.options.len() };
        // borders (2) + body + a blank + options + the key hint
        2 + body + 1 + options + 1
    }

    /// Draw into `area`, which should be [`Rect::bottom`] of the screen.
    pub fn draw(&self, screen: &mut Screen, area: Rect, accent: u8) {
        if area.rows < 3 || area.cols < 8 {
            // No room for a frame. Say what is happening rather than nothing at
            // all, so the user is not stuck at an invisible prompt.
            let text = widgets::clip(&self.title, area.cols);
            screen.put(area.row, area.col, &text, Style::fg(accent).bold());
            return;
        }

        let inner = widgets::frame(screen, area, Style::fg(accent));
        widgets::title(screen, area, &self.title, Style::fg(accent).bold());
        if inner.is_empty() {
            return;
        }

        // Reserve the last inner row for the key hint.
        let hint_row = inner.row + inner.rows - 1;
        let content = Rect::new(inner.row, inner.col, inner.rows.saturating_sub(1), inner.cols);

        let used = widgets::text_block(screen, content, &self.body, Style::default());
        let below = Rect::new(
            content.row + used + 1,
            content.col,
            content.rows.saturating_sub(used + 1),
            content.cols,
        );

        if let Some(buffer) = &self.typing {
            if !below.is_empty() {
                let col = screen.put(below.row, below.col, "› ", Style::fg(accent).bold());
                let shown = widgets::clip(buffer, below.cols.saturating_sub(2));
                let end = screen.put(below.row, col, &shown, Style::default());
                screen.set_cursor(below.row, end.min(area.cols.saturating_sub(1)));
            }
        } else if !below.is_empty() {
            let rows: Vec<Row> = self
                .options
                .iter()
                .enumerate()
                .map(|(i, (label, description))| Row {
                    label,
                    description,
                    selected: i == self.selected,
                    checked: matches!(self.kind, Kind::Question { multi_select: true })
                        .then(|| self.checked.contains(&i)),
                })
                .collect();
            widgets::list(screen, below, &rows, accent);
        }

        let hint = widgets::clip(self.hint(), inner.cols);
        screen.put(hint_row, inner.col, &hint, Style::fg(245));
    }

    fn hint(&self) -> &'static str {
        if self.typing.is_some() {
            return "Enter to send · Esc to go back";
        }
        match self.kind {
            Kind::Approval { .. } => "↑↓ move · Enter choose · y/n · Esc denies",
            Kind::Plan => "↑↓ move · Enter choose · y approve · c clear+approve",
            Kind::Question { multi_select: true } => "↑↓ move · Space tick · Enter send",
            Kind::Question { multi_select: false } => "↑↓ move · 1-9 pick · Enter choose",
            Kind::FreeText => "Enter to send",
            Kind::Rewind => "↑↓ move · Enter choose · y/n",
            Kind::ProviderBusy => "↑↓ move · Enter choose · p switch · d dismiss",
        }
    }
}

/// Tool arguments on one line, short enough to read.
fn compact_args(json: &str) -> String {
    use forge_agent_proto::json::{self, Json};

    let parsed = match json::parse(json) {
        Ok(v) => v,
        // Not JSON: show it as-is rather than hiding what is being approved.
        Err(_) => return first_line(json),
    };
    let Json::Obj(map) = &parsed else {
        return first_line(json);
    };
    map.iter()
        .map(|(k, v)| {
            let value = match v {
                Json::Str(s) => first_line(s),
                other => other.to_string(),
            };
            format!("{k}: {value}")
        })
        .collect::<Vec<_>>()
        .join("  ")
}

fn first_line(s: &str) -> String {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agent_proto::{QuestionItem, QuestionOption};

    fn approval() -> Pending {
        Pending::Approval {
            tool_name: "shell_exec".into(),
            tool_id: "t1".into(),
            tool_args: r#"{"command":"rm -rf build"}"#.into(),
            kind: "execute".into(),
        }
    }

    fn question(multi: bool, labels: &[&str]) -> Pending {
        Pending::Question {
            question: "outer".into(),
            tool_id: "q1".into(),
            items: vec![QuestionItem {
                question: "Which ones?".into(),
                header: "Pick".into(),
                multi_select: multi,
                options: labels
                    .iter()
                    .map(|l| QuestionOption {
                        label: (*l).into(),
                        description: "desc".into(),
                    })
                    .collect(),
            }],
        }
    }

    fn rendered(screen: &mut Screen) -> String {
        let mut sink = Vec::new();
        screen.flush(&mut sink).unwrap();
        String::from_utf8_lossy(&sink).into_owned()
    }

    /// What the user would see: the grid, joined by rows.
    ///
    /// Not the emitted bytes — the renderer skips unchanged cells, so a blank
    /// inside a run splits it into separate positioned writes and a substring
    /// search on the output would miss text that is plainly on screen.
    fn draw(dialog: &Dialog, cols: usize, rows: usize) -> String {
        let mut screen = Screen::new(cols, rows);
        screen.begin_frame();
        let area = Rect::bottom(&screen, dialog.height(cols).min(rows));
        dialog.draw(&mut screen, area, 75);
        (0..rows).map(|r| screen.row_text(r)).collect::<Vec<_>>().join("\n")
    }

    // ── Approval ──────────────────────────────────────────────────────────

    #[test]
    fn approval_shortcuts_decide_immediately() {
        for (key, expected) in [
            ('y', Decision::Approve { remember: false }),
            ('n', Decision::Deny),
        ] {
            let mut d = Dialog::for_pending(&approval());
            assert_eq!(d.handle(Input::Char(key)), Some(expected), "key {key}");
        }
    }

    /// "Always allow" must not be one stray keystroke away. Someone typing when
    /// the prompt appears would otherwise hand a tool standing permission for
    /// the whole session without ever choosing to.
    #[test]
    fn always_allow_has_no_bare_shortcut() {
        for key in ['a', 'A'] {
            let mut d = Dialog::for_pending(&approval());
            assert_eq!(
                d.handle(Input::Char(key)), None,
                "{key:?} must not grant blanket approval",
            );
        }
    }

    /// It is still reachable, deliberately.
    #[test]
    fn always_allow_is_reachable_by_selecting_it() {
        let mut d = Dialog::for_pending(&approval());
        d.handle(Input::Down);
        assert_eq!(
            d.handle(Input::Enter),
            Some(Decision::Approve { remember: true }),
        );
    }

    /// Typing a word at an approval prompt must not approve anything. "always"
    /// contains an 'a', "wait" contains one too.
    #[test]
    fn typing_a_word_at_an_approval_never_grants_permission() {
        let mut d = Dialog::for_pending(&approval());
        let mut granted = Vec::new();
        for c in "wait a moment".chars() {
            if let Some(decision) = d.handle(Input::Char(c)) {
                granted.push(decision);
            }
        }
        assert!(
            !granted.contains(&Decision::Approve { remember: true }),
            "blanket approval was granted by typing: {granted:?}",
        );
    }

    #[test]
    fn approval_shortcuts_are_case_insensitive() {
        let mut d = Dialog::for_pending(&approval());
        assert_eq!(
            d.handle(Input::Char('Y')),
            Some(Decision::Approve { remember: false }),
        );
        let mut d = Dialog::for_pending(&approval());
        assert_eq!(d.handle(Input::Char('N')), Some(Decision::Deny));
    }

    #[test]
    fn approval_options_can_be_chosen_with_the_arrows() {
        let mut d = Dialog::for_pending(&approval());
        assert_eq!(d.handle(Input::Enter), Some(Decision::Approve { remember: false }));

        let mut d = Dialog::for_pending(&approval());
        d.handle(Input::Down);
        assert_eq!(d.handle(Input::Enter), Some(Decision::Approve { remember: true }));

        let mut d = Dialog::for_pending(&approval());
        d.handle(Input::Down);
        d.handle(Input::Down);
        assert_eq!(d.handle(Input::Enter), Some(Decision::Deny));
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        let mut d = Dialog::for_pending(&approval());
        d.handle(Input::Up); // from the first option
        assert_eq!(d.handle(Input::Enter), Some(Decision::Deny), "wrapped to last");

        let mut d = Dialog::for_pending(&approval());
        for _ in 0..3 {
            d.handle(Input::Down);
        }
        assert_eq!(
            d.handle(Input::Enter),
            Some(Decision::Approve { remember: false }),
            "wrapped to first",
        );
    }

    /// Escaping an approval must deny, not dismiss: the agent is blocked, and a
    /// silent dismissal would hang the turn with nothing explaining it.
    #[test]
    fn escape_denies_an_approval_rather_than_dismissing_it() {
        let mut d = Dialog::for_pending(&approval());
        assert_eq!(d.handle(Input::Escape), Some(Decision::Deny));
    }

    /// Approving without seeing the arguments is not consent.
    #[test]
    fn an_approval_shows_what_would_run() {
        let d = Dialog::for_pending(&approval());
        let out = draw(&d, 60, 14);
        assert!(out.contains("rm -rf build"), "the command is visible: {out:?}");
        assert!(out.contains("shell_exec"), "and the tool");
    }

    #[test]
    fn unparseable_tool_arguments_are_still_shown() {
        let pending = Pending::Approval {
            tool_name: "mystery".into(),
            tool_id: "t".into(),
            tool_args: "not json at all".into(),
            kind: "execute".into(),
        };
        let d = Dialog::for_pending(&pending);
        assert!(draw(&d, 60, 14).contains("not json at all"));
    }

    // ── Questions ─────────────────────────────────────────────────────────

    #[test]
    fn a_single_select_question_answers_with_the_chosen_label() {
        let mut d = Dialog::for_pending(&question(false, &["Alpha", "Beta"]));
        d.handle(Input::Down);
        assert_eq!(d.handle(Input::Enter), Some(Decision::Answer("Beta".into())));
    }

    #[test]
    fn a_numbered_key_picks_an_option() {
        let mut d = Dialog::for_pending(&question(false, &["Alpha", "Beta", "Gamma"]));
        assert_eq!(d.handle(Input::Char('3')), Some(Decision::Answer("Gamma".into())));
    }

    #[test]
    fn a_numbered_key_past_the_end_does_nothing() {
        let mut d = Dialog::for_pending(&question(false, &["Alpha"]));
        assert_eq!(d.handle(Input::Char('9')), None, "no such option");
    }

    #[test]
    fn multi_select_ticks_and_joins_the_chosen_labels() {
        let mut d = Dialog::for_pending(&question(true, &["One", "Two", "Three"]));
        d.handle(Input::Char(' '));   // tick One
        d.handle(Input::Down);
        d.handle(Input::Down);
        d.handle(Input::Char(' '));   // tick Three
        assert_eq!(
            d.handle(Input::Enter),
            Some(Decision::Answer("One, Three".into())),
        );
    }

    #[test]
    fn ticking_twice_unticks() {
        let mut d = Dialog::for_pending(&question(true, &["One", "Two"]));
        d.handle(Input::Char(' '));
        d.handle(Input::Char(' '));
        d.handle(Input::Down);
        // Nothing ticked, so Enter takes the highlighted row.
        assert_eq!(d.handle(Input::Enter), Some(Decision::Answer("Two".into())));
    }

    /// Pressing Enter straight away in a multi-select should answer with the
    /// highlighted row, not an empty string.
    #[test]
    fn multi_select_with_nothing_ticked_uses_the_highlighted_row() {
        let mut d = Dialog::for_pending(&question(true, &["One", "Two"]));
        assert_eq!(d.handle(Input::Enter), Some(Decision::Answer("One".into())));
    }

    #[test]
    fn space_is_ordinary_text_in_a_single_select_question() {
        let mut d = Dialog::for_pending(&question(false, &["One", "Two"]));
        assert_eq!(d.handle(Input::Char(' ')), None, "no ticking here");
        // Still on the first option.
        assert_eq!(d.handle(Input::Enter), Some(Decision::Answer("One".into())));
    }

    /// "Other" must switch to free text rather than answering with the literal
    /// word "Other".
    #[test]
    fn choosing_other_switches_to_free_text() {
        let mut d = Dialog::for_pending(&question(false, &["Alpha"]));
        d.handle(Input::Down); // onto "Other"
        assert_eq!(d.handle(Input::Enter), None, "no answer yet");
        assert!(d.is_typing());

        for c in "my own answer".chars() {
            d.handle(Input::Char(c));
        }
        assert_eq!(
            d.handle(Input::Enter),
            Some(Decision::Answer("my own answer".into())),
        );
    }

    /// An agent that already offers "Other" must not produce two of them.
    #[test]
    fn an_agents_own_other_option_is_not_duplicated() {
        let d = Dialog::for_pending(&question(false, &["Alpha", "Other"]));
        assert_eq!(d.options.iter().filter(|(l, _)| l == OTHER).count(), 1);
    }

    /// A question with no options is a free-text prompt from the start.
    #[test]
    fn a_question_without_options_starts_in_free_text() {
        let pending = Pending::Question {
            question: "What now?".into(),
            tool_id: "q".into(),
            items: Vec::new(),
        };
        let mut d = Dialog::for_pending(&pending);
        assert!(d.is_typing());
        for c in "this".chars() {
            d.handle(Input::Char(c));
        }
        assert_eq!(d.handle(Input::Enter), Some(Decision::Answer("this".into())));
    }

    // ── Free text ─────────────────────────────────────────────────────────

    #[test]
    fn typed_text_is_not_intercepted_by_shortcuts() {
        // "yes, do it" contains y, n and a — none may act as a shortcut.
        let mut d = Dialog::for_pending(&Pending::ProcessInput { prompt: "?".into() });
        for c in "yes, and no".chars() {
            assert_eq!(d.handle(Input::Char(c)), None, "char {c:?} was intercepted");
        }
        assert_eq!(d.typed(), "yes, and no");
    }

    #[test]
    fn an_empty_answer_is_not_submitted() {
        let mut d = Dialog::for_pending(&Pending::ProcessInput { prompt: "?".into() });
        assert_eq!(d.handle(Input::Enter), None, "nothing to send");
        d.handle(Input::Char(' '));
        assert_eq!(d.handle(Input::Enter), None, "whitespace is not an answer");
    }

    #[test]
    fn backspace_removes_a_whole_grapheme() {
        let mut d = Dialog::for_pending(&Pending::ProcessInput { prompt: "?".into() });
        for c in "a\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}".chars() {
            d.handle(Input::Char(c));
        }
        d.handle(Input::Backspace);
        assert_eq!(d.typed(), "a", "the whole emoji went");
    }

    /// Opening a text field from a choice list must be escapable, or the user is
    /// trapped in it.
    #[test]
    fn backspacing_past_the_start_returns_to_the_options() {
        let mut d = Dialog::for_pending(&question(false, &["Alpha"]));
        d.handle(Input::Down);
        d.handle(Input::Enter); // into "Other"
        assert!(d.is_typing());
        d.handle(Input::Backspace);
        assert!(!d.is_typing(), "back to the choices");
    }

    #[test]
    fn escape_from_a_text_field_returns_to_the_options() {
        let mut d = Dialog::for_pending(&question(false, &["Alpha"]));
        d.handle(Input::Down);
        d.handle(Input::Enter);
        assert_eq!(d.handle(Input::Escape), None, "not a decision");
        assert!(!d.is_typing());
    }

    #[test]
    fn escape_from_a_text_only_prompt_cancels() {
        let mut d = Dialog::for_pending(&Pending::ProcessInput { prompt: "?".into() });
        assert_eq!(d.handle(Input::Escape), Some(Decision::Cancel));
    }

    #[test]
    fn a_pasted_newline_does_not_submit() {
        let mut d = Dialog::for_pending(&Pending::ProcessInput { prompt: "?".into() });
        assert_eq!(d.handle(Input::Paste("one\ntwo".into())), None);
        assert_eq!(d.typed(), "one two");
    }

    // ── Plan ──────────────────────────────────────────────────────────────

    #[test]
    fn plan_shortcuts_approve_with_and_without_clearing() {
        let plan = Pending::Plan { plan_path: "/p.md".into(), content: "steps".into() };
        let mut d = Dialog::for_pending(&plan);
        assert_eq!(
            d.handle(Input::Char('y')),
            Some(Decision::ApprovePlan { clear_context: false }),
        );
        let mut d = Dialog::for_pending(&plan);
        assert_eq!(
            d.handle(Input::Char('c')),
            Some(Decision::ApprovePlan { clear_context: true }),
        );
    }

    #[test]
    fn a_plan_can_be_rejected_with_feedback() {
        let plan = Pending::Plan { plan_path: "/p.md".into(), content: "steps".into() };
        let mut d = Dialog::for_pending(&plan);
        d.handle(Input::Down);
        d.handle(Input::Down);           // onto "Other"
        d.handle(Input::Enter);
        assert!(d.is_typing());
        for c in "needs detail".chars() {
            d.handle(Input::Char(c));
        }
        assert_eq!(
            d.handle(Input::Enter),
            Some(Decision::Answer("needs detail".into())),
        );
    }

    #[test]
    fn a_plan_shows_its_content() {
        let plan = Pending::Plan {
            plan_path: "/p.md".into(),
            content: "step one then step two".into(),
        };
        let d = Dialog::for_pending(&plan);
        assert!(draw(&d, 60, 16).contains("step one"));
    }

    // ── Rewind ────────────────────────────────────────────────────────────

    #[test]
    fn rewind_confirms_or_declines() {
        let rewind = Pending::Rewind {
            checkpoint_id: "c1".into(),
            preview: "p".into(),
            summary: "drops 3 messages".into(),
        };
        let mut d = Dialog::for_pending(&rewind);
        assert_eq!(
            d.handle(Input::Char('y')),
            Some(Decision::Approve { remember: false }),
        );
        let mut d = Dialog::for_pending(&rewind);
        assert_eq!(d.handle(Input::Char('n')), Some(Decision::Deny));
        // Escaping a rewind declines it rather than doing it.
        let mut d = Dialog::for_pending(&rewind);
        assert_eq!(d.handle(Input::Escape), Some(Decision::Deny));
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    #[test]
    fn a_dialog_states_its_keys() {
        let d = Dialog::for_pending(&approval());
        let out = draw(&d, 60, 14);
        assert!(out.contains("Enter"), "the hint is drawn: {out:?}");
    }

    #[test]
    fn a_dialog_marks_the_selected_option() {
        let d = Dialog::for_pending(&approval());
        assert!(draw(&d, 60, 14).contains("❯ Yes"));
    }

    #[test]
    fn a_multi_select_dialog_draws_checkboxes() {
        let mut d = Dialog::for_pending(&question(true, &["One", "Two"]));
        d.handle(Input::Char(' '));
        let out = draw(&d, 60, 14);
        assert!(out.contains("[x] One"), "got {out:?}");
        assert!(out.contains("[ ] Two"));
    }

    /// A terminal too small for the dialog must still say what is being asked,
    /// rather than leaving the user at an invisible prompt.
    #[test]
    fn a_dialog_degrades_instead_of_vanishing_on_a_tiny_screen() {
        let d = Dialog::for_pending(&approval());
        let out = draw(&d, 20, 2);
        assert!(out.contains("Run"), "the question survives: {out:?}");
    }

    /// Every degenerate geometry must draw without panicking.
    #[test]
    fn drawing_survives_any_screen_size() {
        let dialogs = [
            Dialog::for_pending(&approval()),
            Dialog::for_pending(&question(true, &["One", "Two"])),
            Dialog::for_pending(&Pending::ProcessInput { prompt: "?".into() }),
        ];
        for d in &dialogs {
            for (cols, rows) in [(1usize, 1usize), (2, 2), (4, 3), (8, 4), (20, 2), (80, 30)] {
                let mut screen = Screen::new(cols, rows);
                screen.begin_frame();
                let area = Rect::bottom(&screen, d.height(cols).min(rows));
                d.draw(&mut screen, area, 75);
                let mut sink = Vec::new();
                screen.flush(&mut sink).unwrap();
            }
        }
    }

    #[test]
    fn height_grows_with_the_options_and_body() {
        let small = Dialog::for_pending(&Pending::ProcessInput { prompt: "?".into() });
        let big = Dialog::for_pending(&question(false, &["a", "b", "c", "d"]));
        assert!(big.height(60) > small.height(60));
    }

    /// A frame is still one synchronized update with a dialog on top.
    #[test]
    fn a_dialog_frame_never_erases_scrollback() {
        let d = Dialog::for_pending(&approval());
        let mut screen = Screen::new(60, 14);
        screen.begin_frame();
        let area = Rect::bottom(&screen, 14);
        d.draw(&mut screen, area, 75);
        let out = rendered(&mut screen);
        assert!(!out.contains("\x1b[3J"));
        assert_eq!(out.matches(crate::screen::SYNC_BEGIN).count(), 1);
    }
}
