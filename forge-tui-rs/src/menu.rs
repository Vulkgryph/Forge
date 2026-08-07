// SPDX-License-Identifier: Apache-2.0
//! The menu: model switching, permissions, tools, and session settings.
//!
//! A full-screen list rather than a modal, because it is a place you navigate
//! around in — several screens deep, with a stack — not a single question to
//! answer. Escape goes back one level and closes at the top, so there is always
//! a way out without needing to remember where you are.
//!
//! Two details that look small and are not:
//!
//!  * **Headers are not selectable.** A list with section headings has to skip
//!    them in both directions and when wrapping, or the cursor lands on a label
//!    and Enter does nothing — which reads as the menu being broken.
//!  * **Every toggle shows its current state.** A settings screen that does not
//!    say what is currently on is a screen you have to change something to read.
//!
//! Items are rebuilt from the [`Session`] on every draw rather than cached, so a
//! model switch or a tool toggle is reflected immediately without a separate
//! invalidation path to forget.

use forge_agent_proto::{ClientMessage, EndpointInfo};

use crate::app::Input;
use crate::screen::{Screen as Canvas, Style};
use crate::session::{Effect, PermissionMode, Session};
use crate::widgets::{self, Rect};

/// Which screen the menu is showing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Page {
    Root,
    Models,
    Settings,
    Permission,
    /// The handful of tools worth reaching for often.
    BasicTools,
    /// Every tool, the basic ones included.
    AdvancedTools,
    ContextStrategy,
    Offline,
    Subagents,
    SubagentModel,
    WebModel,
    Rewind,
    /// Saved sessions, read from disk.
    Sessions,
    /// Reasoning controls per endpoint.
    Thinking,
}

impl Page {
    fn title(&self) -> &'static str {
        match self {
            Page::Root => "Menu",
            Page::Models => "Main model",
            Page::Settings => "Settings",
            Page::Permission => "Permission mode",
            Page::BasicTools => "Basic tools",
            Page::AdvancedTools => "Advanced tools",
            Page::ContextStrategy => "Context strategy",
            Page::Offline => "Offline mode",
            Page::Subagents => "Agents",
            Page::SubagentModel => "Subagent default model",
            Page::WebModel => "Web tool model",
            Page::Rewind => "Rewind",
            Page::Sessions => "Saved sessions",
            Page::Thinking => "Reasoning",
        }
    }

    fn footer(&self) -> &'static str {
        match self {
            Page::Root => "Everything about this session.",
            Page::Models => "Switches the primary model. Takes effect immediately.",
            Page::Settings => "Permissions, tools, context and connectivity.",
            Page::Permission => "Also cycled with Shift+Tab, except for approve-everything.",
            Page::BasicTools => "Enter toggles. A disabled tool is not offered to the model.",
            Page::AdvancedTools => "Every tool, the basic ones included.",
            Page::ContextStrategy => "What happens as the context window fills.",
            Page::Offline => "Offline refuses network tools rather than failing on them.",
            Page::Subagents => "Definitions available for delegation.",
            Page::SubagentModel => "Which model subagents use unless told otherwise.",
            Page::WebModel => "Which model summarises fetched pages.",
            Page::Rewind => "Return the conversation to an earlier point.",
            Page::Sessions => "Resuming restarts the agent with that session loaded.",
            Page::Thinking => "Enter cycles a provider's thinking between default, on and off.",
        }
    }
}

/// The tools the basic list offers — the ones worth turning on and off often,
/// rather than the whole set. The TypeScript client's shortlist, unchanged.
const BASIC_TOOLS: &[&str] = &["web_search", "web_fetch", "shell_exec", "delegate_task"];

fn is_basic_tool(name: &str) -> bool {
    BASIC_TOOLS.contains(&name)
}

/// A readable name for a tool, falling back to the tool's own name for anything
/// not in the list — a tool added later shows up rather than disappearing.
fn tool_label(name: &str) -> String {
    let label = match name {
        "read_file" => "Read files",
        "list_directory" => "List directory",
        "search_code" => "Search code",
        "apply_patch" => "Apply patch",
        "write_file" => "Write files",
        "edit_file" => "Edit files",
        "glob_files" => "Glob files",
        "todo_write" => "Todo write",
        "web_search" => "Web search",
        "web_fetch" => "Web fetch",
        "shell_exec" => "Shell commands",
        "delegate_task" => "Subagents",
        other => other,
    };
    label.to_string()
}

/// "N disabled", or "All enabled" when none are.
fn disabled_summary(session: &Session, basic_only: bool) -> String {
    let disabled = session
        .available_tools
        .iter()
        .filter(|t| !t.enabled && (!basic_only || is_basic_tool(&t.name)))
        .count();
    if disabled == 0 {
        "All enabled".to_string()
    } else {
        format!("{disabled} disabled")
    }
}

/// What choosing an item does.
#[derive(Clone, Debug, PartialEq)]
enum Act {
    /// Descend into another page.
    Open(Page),
    /// Leave the menu.
    Close,
    /// Go back one level.
    Back,
    Send(ClientMessage),
    /// Restart the agent, optionally resuming a session.
    Restart { resume: Option<String> },
    SetPermission(PermissionMode),
    /// Nothing — an informational row.
    None,
}

/// One row.
struct Item {
    label:       String,
    description: String,
    /// Shown before the label: a state indicator, or a bullet.
    marker:      Option<&'static str>,
    act:         Act,
}

impl Item {
    fn header(label: impl Into<String>) -> Self {
        Self { label: label.into(), description: String::new(), marker: None, act: Act::None }
    }

    fn row(label: impl Into<String>, description: impl Into<String>, act: Act) -> Self {
        Self {
            label: label.into(),
            description: description.into(),
            marker: None,
            act,
        }
    }

    fn marked(mut self, on: bool) -> Self {
        self.marker = Some(if on { "●" } else { "○" });
        self
    }

    /// Headers are labels, not choices.
    fn is_header(&self) -> bool {
        matches!(self.act, Act::None) && self.description.is_empty()
    }
}

/// What the caller should do after a keypress.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Still open.
    Stay,
    /// Dismissed.
    Close,
    /// Dismissed, with messages to send.
    Act(Vec<Effect>),
    /// Set the permission mode. Its own outcome because the mode is the client's
    /// own gate rather than something the agent is told: the menu cannot reach
    /// into the session itself, and turning it into an outgoing message is what
    /// broke it before.
    SetPermission(PermissionMode),
}

pub struct Menu {
    /// Page stack; the last is showing. Never empty.
    stack:    Vec<Page>,
    selected: usize,
    /// First visible row, so a long list scrolls.
    scroll:   usize,
}

impl Default for Menu {
    fn default() -> Self {
        Self::new()
    }
}

impl Menu {
    pub fn new() -> Self {
        Self { stack: vec![Page::Root], selected: 0, scroll: 0 }
    }

    /// Open at a particular page, with Root beneath it so Escape still goes back
    /// rather than closing from a page the user did not navigate to.
    pub fn at(page: Page) -> Self {
        Self { stack: vec![Page::Root, page], selected: 0, scroll: 0 }
    }

    pub fn page(&self) -> &Page {
        self.stack.last().expect("the stack is never empty")
    }

    /// Rows for the current page, built fresh from the session.
    fn items(&self, session: &Session) -> Vec<Item> {
        match self.page() {
            Page::Root => vec![
                Item::row(
                    "Main model",
                    format!("{} ({})", session.model_name, session.model_id),
                    Act::Open(Page::Models),
                ),
                Item::row("Settings", "permissions, tools, context", Act::Open(Page::Settings)),
                Item::row(
                    "Agents",
                    format!("{} available", session.agent_defs.len()),
                    Act::Open(Page::Subagents),
                ),
                Item::row(
                    "Rewind",
                    format!("{} checkpoints", session.checkpoints.len()),
                    Act::Open(Page::Rewind),
                ),
                Item::row("Compact now", "summarise the conversation so far",
                          Act::Send(ClientMessage::Compact)),
                Item::row("Clear session", "start over, keeping the project",
                          Act::Send(ClientMessage::ClearSession)),
                Item::row(
                    "Saved sessions",
                    "resume an earlier conversation",
                    Act::Open(Page::Sessions),
                ),
                Item::row("Reasoning", "thinking controls", Act::Open(Page::Thinking)),
                Item::row("Close", "back to the conversation", Act::Close),
            ],

            Page::Models => session
                .endpoints
                .iter()
                .map(|ep| {
                    let current = ep.model_id == session.model_id;
                    Item::row(
                        ep.name.clone(),
                        format!("{} · {}k context", ep.endpoint_type,
                                ep.max_context_tokens / 1000),
                        Act::Send(ClientMessage::SwitchModel {
                            name: ep.name.clone(),
                            base_url: ep.base_url.clone(),
                            model_id: ep.model_id.clone(),
                            max_context_tokens: ep.max_context_tokens,
                            max_output_tokens: ep.max_output_tokens,
                            endpoint_type: ep.endpoint_type.clone(),
                            reasoning: ep.reasoning,
                        }),
                    )
                    .marked(current)
                })
                .collect(),

            Page::Settings => vec![
                Item::row(
                    "Permission mode",
                    match session.permission_mode {
                        PermissionMode::Ask => "ask before each tool",
                        PermissionMode::AutoAccept => "auto-accept edits",
                        PermissionMode::AllowAll => "approve everything",
                        PermissionMode::Plan => "planning, read-only",
                    },
                    Act::Open(Page::Permission),
                ),
                // Two lists rather than one, as the TypeScript client had them: a
                // short one for the tools people actually turn on and off, and the
                // full set behind "Advanced". Counted as "N disabled" rather than
                // "N of M enabled", since what a reader wants from this row is
                // whether anything is switched off.
                Item::row("Basic tools", disabled_summary(session, true), Act::Open(Page::BasicTools)),
                Item::row("Advanced tools", disabled_summary(session, false), Act::Open(Page::AdvancedTools)),
                Item::row("Context strategy", session.context_strategy.clone(),
                          Act::Open(Page::ContextStrategy)),
                Item::row(
                    "Offline mode",
                    if session.offline_mode { "on" } else { "off" },
                    Act::Open(Page::Offline),
                ),
                Item::row("Subagent model", "default for delegated work",
                          Act::Open(Page::SubagentModel)),
                Item::row("Web tool model", "summarises fetched pages",
                          Act::Open(Page::WebModel)),
            ],

            Page::Permission => vec![
                Item::row("Ask each time", "approve tools one at a time",
                          Act::SetPermission(PermissionMode::Ask))
                    .marked(session.permission_mode == PermissionMode::Ask),
                Item::row("Auto-accept edits", "edits go through; commands still ask",
                          Act::SetPermission(PermissionMode::AutoAccept))
                    .marked(session.permission_mode == PermissionMode::AutoAccept),
                Item::row("Plan mode", "read-only — the agent drafts a plan first",
                          Act::SetPermission(PermissionMode::Plan))
                    .marked(session.permission_mode == PermissionMode::Plan),
                Item::row("Approve everything", "no prompts — be sure",
                          Act::SetPermission(PermissionMode::AllowAll))
                    .marked(session.permission_mode == PermissionMode::AllowAll),
            ],

            Page::BasicTools | Page::AdvancedTools => session
                .available_tools
                .iter()
                .filter(|tool| *self.page() != Page::BasicTools || is_basic_tool(&tool.name))
                .map(|tool| {
                    Item::row(
                        // The tool's own name is the description, so the label can
                        // be readable without hiding what it actually is.
                        tool_label(&tool.name),
                        tool.name.clone(),
                        Act::Send(ClientMessage::UpdateToolConfig {
                            tool: tool.name.clone(),
                            // Enter flips it.
                            enabled: !tool.enabled,
                        }),
                    )
                    .marked(tool.enabled)
                })
                .collect(),

            Page::ContextStrategy => ["compact", "truncate", "none"]
                .iter()
                .map(|strategy| {
                    Item::row(
                        *strategy,
                        match *strategy {
                            "compact" => "summarise older turns",
                            "truncate" => "drop older turns",
                            _ => "let the window fill and fail",
                        },
                        Act::Send(ClientMessage::UpdateContextStrategy {
                            strategy: (*strategy).to_string(),
                        }),
                    )
                    .marked(session.context_strategy == *strategy)
                })
                .collect(),

            Page::Offline => vec![
                Item::row("Online", "network tools available",
                          Act::Send(ClientMessage::UpdateOfflineMode { enabled: false }))
                    .marked(!session.offline_mode),
                Item::row("Offline", "network tools refused",
                          Act::Send(ClientMessage::UpdateOfflineMode { enabled: true }))
                    .marked(session.offline_mode),
            ],

            Page::Subagents => {
                if session.agent_defs.is_empty() {
                    vec![Item::row("No agents defined", "nothing to delegate to", Act::Back)]
                } else {
                    session
                        .agent_defs
                        .iter()
                        .map(|def| {
                            Item::row(
                                def.name.clone(),
                                format!("{} · {}", def.model, def.source),
                                // Informational: definitions are files, not
                                // something to change from here.
                                Act::None,
                            )
                        })
                        .collect()
                }
            }

            Page::SubagentModel => {
                let mut items = vec![Item::row(
                    "Inherit",
                    "same model as the main agent",
                    Act::Send(ClientMessage::UpdateSubagentConfig {
                        enabled: None,
                        max_concurrent: None,
                        max_depth: None,
                        default_model: None,
                        clear_default_model: true,
                        max_delegate_secs: None,
                    }),
                )];
                items.extend(session.endpoints.iter().map(|ep| {
                    Item::row(
                        ep.name.clone(),
                        ep.endpoint_type.clone(),
                        Act::Send(ClientMessage::UpdateSubagentConfig {
                            enabled: None,
                            max_concurrent: None,
                            max_depth: None,
                            default_model: Some(ep.name.clone()),
                            clear_default_model: false,
                            max_delegate_secs: None,
                        }),
                    )
                }));
                items
            }

            Page::WebModel => {
                let mut items = vec![Item::row(
                    "Inherit",
                    "same model as the main agent",
                    Act::Send(ClientMessage::UpdateWebModel { model: String::new() }),
                )];
                items.extend(session.endpoints.iter().map(|ep| {
                    Item::row(
                        ep.name.clone(),
                        ep.endpoint_type.clone(),
                        Act::Send(ClientMessage::UpdateWebModel { model: ep.name.clone() }),
                    )
                }));
                items
            }

            Page::Sessions => {
                let sessions = crate::sessions::list(std::path::Path::new(
                    &session.project_root,
                ));
                if sessions.is_empty() {
                    vec![Item::row(
                        "No saved sessions",
                        "nothing to resume in this project",
                        Act::Back,
                    )]
                } else {
                    sessions
                        .iter()
                        .map(|meta| {
                            Item::row(
                                meta.label(),
                                meta.detail(),
                                Act::Restart { resume: Some(meta.id.clone()) },
                            )
                            .marked(session.session_id.as_deref() == Some(meta.id.as_str()))
                        })
                        .collect()
                }
            }

            Page::Thinking => session
                .endpoints
                .iter()
                .map(|ep| {
                    let current = describe_thinking(ep);
                    let mut next = ep.reasoning;
                    cycle_thinking(&mut next, &ep.endpoint_type);
                    Item::row(
                        ep.name.clone(),
                        format!("{} · {current}", ep.endpoint_type),
                        Act::Send(ClientMessage::UpdateEndpointReasoning {
                            endpoint_name: ep.name.clone(),
                            reasoning: next,
                        }),
                    )
                })
                .collect(),

            Page::Rewind => {
                if session.checkpoints.is_empty() {
                    vec![Item::row("No checkpoints yet", "nothing to rewind to", Act::Back)]
                } else {
                    let mut items = vec![Item::header("Newest first")];
                    items.extend(session.checkpoints.iter().rev().map(|cp| {
                        Item::row(
                            format!("#{} · {} messages", cp.display_index, cp.message_count),
                            cp.preview.clone(),
                            // Ask first: the preview says what would be lost.
                            Act::Send(ClientMessage::RewindPreview {
                                checkpoint_id: cp.id.clone(),
                            }),
                        )
                    }));
                    items
                }
            }
        }
    }

    /// Snap the cursor onto a selectable row.
    ///
    /// Called before both input and drawing, because several paths can leave it
    /// on a header: opening a page whose first row is a heading, or the list
    /// changing shape underneath it when the session updates.
    fn normalize(&mut self, items: &[Item]) {
        if items.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected =
            nearest_selectable(items, self.selected.min(items.len() - 1), Direction::Forward);
    }

    /// Handle a keypress.
    pub fn handle(&mut self, input: Input, session: &Session) -> Outcome {
        let items = self.items(session);
        self.normalize(&items);
        let rows = items.len();

        match input {
            // Escape goes back a level, and closes at the top. A menu you can
            // only leave from one particular page is a menu you get stuck in.
            Input::Escape => {
                if self.stack.len() > 1 {
                    self.pop(session);
                    Outcome::Stay
                } else {
                    Outcome::Close
                }
            }

            Input::Up => {
                self.selected = step(&items, self.selected, Direction::Back);
                Outcome::Stay
            }
            Input::Down => {
                self.selected = step(&items, self.selected, Direction::Forward);
                Outcome::Stay
            }
            Input::PageUp => {
                let target = self.selected.saturating_sub(10);
                self.selected = nearest_selectable(&items, target, Direction::Back);
                Outcome::Stay
            }
            Input::PageDown => {
                let target = (self.selected + 10).min(rows.saturating_sub(1));
                self.selected = nearest_selectable(&items, target, Direction::Forward);
                Outcome::Stay
            }
            Input::Home => {
                self.selected = nearest_selectable(&items, 0, Direction::Forward);
                Outcome::Stay
            }

            // Vim keys, since this is a list.
            Input::Char('k') => {
                self.selected = step(&items, self.selected, Direction::Back);
                Outcome::Stay
            }
            Input::Char('j') => {
                self.selected = step(&items, self.selected, Direction::Forward);
                Outcome::Stay
            }
            Input::Char('g') => {
                self.selected = nearest_selectable(&items, 0, Direction::Forward);
                Outcome::Stay
            }
            Input::Char('G') => {
                self.selected =
                    nearest_selectable(&items, rows.saturating_sub(1), Direction::Back);
                Outcome::Stay
            }
            // Digits jump, as in the dialogs.
            Input::Char(c) if c.is_ascii_digit() && c != '0' => {
                let idx = (c as u8 - b'1') as usize;
                if idx < rows && !items[idx].is_header() {
                    self.selected = idx;
                    return self.choose(&items, session);
                }
                Outcome::Stay
            }

            Input::Enter => self.choose(&items, session),

            // Quitting the program must work from inside the menu too.
            Input::Quit => Outcome::Close,

            _ => Outcome::Stay,
        }
    }

    fn choose(&mut self, items: &[Item], session: &Session) -> Outcome {
        let Some(item) = items.get(self.selected) else {
            return Outcome::Stay;
        };
        match &item.act {
            Act::Open(page) => {
                self.stack.push(page.clone());
                self.selected = 0;
                self.scroll = 0;
                // Land on a usable row straight away. Deferring this to the next
                // keypress leaves the cursor sitting on a heading in the
                // meantime, where Enter does nothing.
                let fresh = self.items(session);
                self.normalize(&fresh);
                Outcome::Stay
            }
            Act::Back => {
                self.pop(session);
                Outcome::Stay
            }
            Act::Close => Outcome::Close,
            Act::None => Outcome::Stay,
            Act::Send(msg) => Outcome::Act(vec![Effect::Send(msg.clone())]),
            Act::Restart { resume } => {
                Outcome::Act(vec![Effect::Restart { resume: resume.clone() }])
            }
            // Choosing a mode sets it, and nothing is sent: the permission mode is
            // the client's own approval gate, and the agent goes on asking exactly
            // as before — this side decides what to do with the question.
            //
            // This used to send `ToggleAutoMode` from both arms of an `if` whose
            // branches were identical, and never set the mode at all. So choosing
            // "Plan mode" did not enter plan mode; it toggled the agent's auto
            // mode, which is neither the mode chosen nor the one displayed.
            Act::SetPermission(mode) => Outcome::SetPermission(*mode),
        }
    }

    fn pop(&mut self, session: &Session) {
        if self.stack.len() > 1 {
            self.stack.pop();
        }
        self.selected = 0;
        self.scroll = 0;
        let fresh = self.items(session);
        self.normalize(&fresh);
    }

    /// Keep the selection inside the visible window.
    fn scroll_to_selection(&mut self, rows: usize, visible: usize) {
        if visible == 0 {
            return;
        }
        let max_scroll = rows.saturating_sub(visible);
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
        self.scroll = self.scroll.min(max_scroll);
    }

    pub fn draw(&mut self, canvas: &mut Canvas, area: Rect, session: &Session, accent: u8) {
        if area.is_empty() {
            return;
        }
        let items = self.items(session);

        // Title, then rows, then the footer and the key hint.
        let title_row = area.row;
        canvas.put(title_row, area.col, self.page().title(), Style::fg(accent).bold());

        // Breadcrumbs, so several levels deep is legible.
        if self.stack.len() > 1 {
            let trail: Vec<&str> = self.stack.iter().map(Page::title).collect();
            let text = widgets::clip(
                &trail.join(" › "),
                area.cols.saturating_sub(self.page().title().len() + 4),
            );
            canvas.put(
                title_row,
                area.col + self.page().title().len() as u16 as usize + 2,
                &text,
                Style::fg(245),
            );
        }

        // Budget: one row for the title, two at the bottom for the footer and
        // the key hint. When the list does not fit, two more go to the scroll
        // indicators — they need rows of their own, or they are drawn over
        // entries and both become unreadable.
        let list_area = area.rows.saturating_sub(3);
        let scrolling = items.len() > list_area;
        let visible = if scrolling { list_area.saturating_sub(2) } else { list_area };

        self.normalize(&items);
        self.scroll_to_selection(items.len(), visible);

        let first_list_row = area.row + 1 + usize::from(scrolling);
        let end = (self.scroll + visible).min(items.len());

        if scrolling && self.scroll > 0 {
            canvas.put(area.row + 1, area.col, &format!("  ↑ {} more", self.scroll),
                       Style::fg(245));
        }

        for (offset, item) in items[self.scroll..end].iter().enumerate() {
            let y = first_list_row + offset;
            if y >= area.row + area.rows.saturating_sub(2) {
                break;
            }
            let index = self.scroll + offset;
            self.draw_item(canvas, y, area, item, index == self.selected, accent);
        }

        let hidden_below = items.len().saturating_sub(end);
        if scrolling && hidden_below > 0 {
            let y = first_list_row + visible;
            if y < area.row + area.rows.saturating_sub(2) {
                canvas.put(y, area.col, &format!("  ↓ {hidden_below} more"),
                           Style::fg(245));
            }
        }

        // Footer and hint.
        let footer_row = area.row + area.rows.saturating_sub(2);
        canvas.put(footer_row, area.col, &widgets::clip(self.page().footer(), area.cols),
                   Style::fg(245));
        let hint = if self.stack.len() > 1 {
            "↑↓/jk move · Enter choose · Esc back"
        } else {
            "↑↓/jk move · Enter choose · Esc close"
        };
        canvas.put(
            area.row + area.rows.saturating_sub(1),
            area.col,
            &widgets::clip(hint, area.cols),
            Style::fg(238),
        );
    }

    fn draw_item(
        &self,
        canvas:   &mut Canvas,
        y:        usize,
        area:     Rect,
        item:     &Item,
        selected: bool,
        accent:   u8,
    ) {
        if item.is_header() {
            canvas.put(y, area.col + 2, &widgets::clip(&item.label, area.cols),
                       Style::fg(245));
            return;
        }

        let style = if selected { Style::fg(accent).bold() } else { Style::default() };
        let mut col = canvas.put(y, area.col + 2, if selected { "❯ " } else { "  " },
                                 Style::fg(accent));
        if let Some(marker) = item.marker {
            col = canvas.put(y, col, &format!("{marker} "), style);
        }
        col = canvas.put(y, col, &item.label, style);

        if !item.description.is_empty() {
            let used = col.saturating_sub(area.col);
            let room = area.cols.saturating_sub(used).saturating_sub(2);
            if room > 4 {
                canvas.put(y, col + 2, &widgets::clip(&item.description, room),
                           Style::fg(245));
            }
        }
    }
}

/// A one-word summary of a provider's thinking setting.
fn describe_thinking(ep: &EndpointInfo) -> &'static str {
    use forge_agent_proto::ProviderToggle::*;
    let toggle = match ep.endpoint_type.as_str() {
        "anthropic" => ep.reasoning.anthropic.thinking,
        _ => ep.reasoning.open_ai_compatible.thinking,
    };
    match toggle {
        ProviderDefault => "default",
        On => "on",
        Off => "off",
    }
}

/// Advance a provider's thinking toggle one step, so Enter cycles it.
///
/// Which field applies depends on the endpoint type — the same struct carries
/// settings for all three providers, and writing the wrong one would appear to do
/// nothing.
fn cycle_thinking(cfg: &mut forge_agent_proto::EndpointReasoningConfig, endpoint_type: &str) {
    use forge_agent_proto::ProviderToggle::*;
    let next = |t: forge_agent_proto::ProviderToggle| match t {
        ProviderDefault => On,
        On => Off,
        Off => ProviderDefault,
    };
    match endpoint_type {
        "anthropic" => cfg.anthropic.thinking = next(cfg.anthropic.thinking),
        _ => cfg.open_ai_compatible.thinking = next(cfg.open_ai_compatible.thinking),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Back,
}

/// Move one selectable row, wrapping, skipping headers.
///
/// Wrapping has to skip headers too — a list whose first row is a heading would
/// otherwise wrap onto it and strand the cursor somewhere Enter does nothing.
fn step(items: &[Item], from: usize, direction: Direction) -> usize {
    let n = items.len();
    if n == 0 {
        return 0;
    }
    for hop in 1..=n {
        let index = match direction {
            Direction::Forward => (from + hop) % n,
            Direction::Back => (from + n - (hop % n)) % n,
        };
        if !items[index].is_header() {
            return index;
        }
    }
    from // every row is a header
}

/// The nearest selectable row at or past `target`, searching `direction` first.
fn nearest_selectable(items: &[Item], target: usize, direction: Direction) -> usize {
    let n = items.len();
    if n == 0 {
        return 0;
    }
    let target = target.min(n - 1);
    if !items[target].is_header() {
        return target;
    }
    // Try the requested direction, then the other, so an edge still lands
    // somewhere usable.
    let forward = (target..n).find(|i| !items[*i].is_header());
    let backward = (0..=target).rev().find(|i| !items[*i].is_header());
    match direction {
        Direction::Forward => forward.or(backward),
        Direction::Back => backward.or(forward),
    }
    .unwrap_or(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agent_proto::{
        AgentDefInfo, AgentMessage, EndpointInfo, EndpointReasoningConfig, Init, ToolInfo,
    };

    fn endpoint(name: &str, model_id: &str) -> EndpointInfo {
        EndpointInfo {
            name: name.into(),
            base_url: "https://example.invalid".into(),
            model_id: model_id.into(),
            max_context_tokens: 200_000,
            max_output_tokens: 8_192,
            endpoint_type: "anthropic".into(),
            reasoning: EndpointReasoningConfig::default(),
            xai_priority_tier: false,
        }
    }

    fn session() -> Session {
        let mut s = Session::new();
        s.apply(AgentMessage::Init(Box::new(Init {
            model_name: "First".into(),
            model_id: "first-1".into(),
            max_context_tokens: 200_000,
            context_strategy: "compact".into(),
            endpoints: vec![endpoint("First", "first-1"), endpoint("Second", "second-1")],
            available_tools: vec![
                ToolInfo { name: "read_file".into(), enabled: true },
                ToolInfo { name: "shell_exec".into(), enabled: false },
            ],
            agent_definitions: vec![AgentDefInfo {
                name: "Explore".into(),
                description: "d".into(),
                model: "inherit".into(),
                max_turns: None,
                tools: vec![],
                source: "built-in".into(),
            }],
            ..Default::default()
        })));
        s
    }

    fn grid(canvas: &Canvas) -> String {
        (0..canvas.rows()).map(|r| canvas.row_text(r)).collect::<Vec<_>>().join("\n")
    }

    fn draw(menu: &mut Menu, session: &Session, cols: usize, rows: usize) -> String {
        let mut canvas = Canvas::new(cols, rows);
        canvas.begin_frame();
        menu.draw(&mut canvas, Rect::new(0, 0, rows, cols), session, 75);
        grid(&canvas)
    }

    /// Walk to a labelled row, so tests do not hard-code indices that shift when
    /// the menu gains an entry.
    fn select(menu: &mut Menu, session: &Session, label: &str) {
        for _ in 0..40 {
            let items = menu.items(session);
            if items.get(menu.selected).is_some_and(|i| i.label.starts_with(label)) {
                return;
            }
            menu.handle(Input::Down, session);
        }
        panic!("never landed on {label:?}");
    }

    // ── Navigation ────────────────────────────────────────────────────────

    #[test]
    fn opens_on_the_root_page() {
        let menu = Menu::new();
        assert_eq!(*menu.page(), Page::Root);
    }

    #[test]
    fn moving_down_and_up_returns_to_where_it_started() {
        let s = session();
        let mut menu = Menu::new();
        let start = menu.selected;
        menu.handle(Input::Down, &s);
        assert_ne!(menu.selected, start);
        menu.handle(Input::Up, &s);
        assert_eq!(menu.selected, start);
    }

    #[test]
    fn selection_wraps_at_both_ends() {
        let s = session();
        let mut menu = Menu::new();
        let count = menu.items(&s).len();

        for _ in 0..count {
            menu.handle(Input::Down, &s);
        }
        assert_eq!(menu.selected, 0, "wrapped to the top");

        menu.handle(Input::Up, &s);
        assert_eq!(menu.selected, count - 1, "and back to the bottom");
    }

    #[test]
    fn vim_keys_move_the_selection() {
        let s = session();
        let mut menu = Menu::new();
        menu.handle(Input::Char('j'), &s);
        assert_eq!(menu.selected, 1);
        menu.handle(Input::Char('k'), &s);
        assert_eq!(menu.selected, 0);
    }

    #[test]
    fn g_and_shift_g_jump_to_the_ends() {
        let s = session();
        let mut menu = Menu::new();
        menu.handle(Input::Char('G'), &s);
        assert_eq!(menu.selected, menu.items(&s).len() - 1);
        menu.handle(Input::Char('g'), &s);
        assert_eq!(menu.selected, 0);
    }

    /// A header must never be selectable, or Enter lands on a label and appears
    /// to do nothing.
    #[test]
    fn headers_are_skipped_in_both_directions() {
        let mut s = session();
        // The rewind page puts a header first.
        for i in 0..3 {
            s.apply(AgentMessage::RewindCheckpoint {
                id: format!("c{i}"),
                preview: "p".into(),
                message_count: 1,
                keep_on_restore: false,
            });
        }
        let mut menu = Menu::new();
        select(&mut menu, &s, "Rewind");
        menu.handle(Input::Enter, &s);
        assert_eq!(*menu.page(), Page::Rewind);

        let items = menu.items(&s);
        assert!(items[0].is_header(), "precondition: first row is a heading");

        // Walking the whole list must never rest on the header.
        for _ in 0..items.len() * 2 {
            assert!(
                !menu.items(&s)[menu.selected].is_header(),
                "selection landed on a header at {}", menu.selected,
            );
            menu.handle(Input::Down, &s);
        }
        for _ in 0..items.len() * 2 {
            assert!(!menu.items(&s)[menu.selected].is_header());
            menu.handle(Input::Up, &s);
        }
    }

    #[test]
    fn jumping_to_the_top_of_a_page_with_a_header_skips_it() {
        let mut s = session();
        s.apply(AgentMessage::RewindCheckpoint {
            id: "c1".into(), preview: "p".into(),
            message_count: 1, keep_on_restore: false,
        });
        let mut menu = Menu::new();
        select(&mut menu, &s, "Rewind");
        menu.handle(Input::Enter, &s);
        menu.handle(Input::Char('g'), &s);
        assert!(!menu.items(&s)[menu.selected].is_header());
    }

    // ── The stack ─────────────────────────────────────────────────────────

    #[test]
    fn entering_a_page_and_escaping_returns_to_the_parent() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        assert_eq!(*menu.page(), Page::Settings);

        assert_eq!(menu.handle(Input::Escape, &s), Outcome::Stay);
        assert_eq!(*menu.page(), Page::Root, "back at the top");
    }

    /// Escape at the top closes. A menu you can only leave from one page is one
    /// you get stuck in.
    #[test]
    fn escape_at_the_root_closes() {
        let s = session();
        let mut menu = Menu::new();
        assert_eq!(menu.handle(Input::Escape, &s), Outcome::Close);
    }

    #[test]
    fn nesting_several_levels_deep_unwinds_one_at_a_time() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Basic tools");
        menu.handle(Input::Enter, &s);
        assert_eq!(*menu.page(), Page::BasicTools);

        menu.handle(Input::Escape, &s);
        assert_eq!(*menu.page(), Page::Settings);
        menu.handle(Input::Escape, &s);
        assert_eq!(*menu.page(), Page::Root);
        assert_eq!(menu.handle(Input::Escape, &s), Outcome::Close);
    }

    /// Entering a page must not carry the parent's cursor position with it.
    #[test]
    fn opening_a_page_resets_the_selection() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        let parent_selection = menu.selected;
        assert!(parent_selection > 0);
        menu.handle(Input::Enter, &s);
        assert_eq!(menu.selected, 0, "starts at the top of the new page");
    }

    /// Quitting the program has to work from inside the menu.
    #[test]
    fn quit_closes_the_menu() {
        let s = session();
        let mut menu = Menu::new();
        assert_eq!(menu.handle(Input::Quit, &s), Outcome::Close);
    }

    // ── Actions ───────────────────────────────────────────────────────────

    /// The reason the menu exists: changing model.
    #[test]
    fn choosing_a_model_sends_that_endpoints_details() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Main model");
        menu.handle(Input::Enter, &s);
        assert_eq!(*menu.page(), Page::Models);

        select(&mut menu, &s, "Second");
        match menu.handle(Input::Enter, &s) {
            Outcome::Act(effects) => match &effects[0] {
                Effect::Send(ClientMessage::SwitchModel { name, model_id, base_url, .. }) => {
                    assert_eq!(name, "Second");
                    assert_eq!(model_id, "second-1");
                    assert!(!base_url.is_empty(), "the agent needs the URL too");
                }
                other => panic!("wrong message: {other:?}"),
            },
            other => panic!("expected an action, got {other:?}"),
        }
    }

    /// The current model must be marked, or you cannot tell what you are on.
    #[test]
    fn the_current_model_is_marked() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Main model");
        menu.handle(Input::Enter, &s);

        let items = menu.items(&s);
        let current = items.iter().find(|i| i.label == "First").expect("First is listed");
        assert_eq!(current.marker, Some("●"), "the active model is filled in");
        let other = items.iter().find(|i| i.label == "Second").unwrap();
        assert_eq!(other.marker, Some("○"));
    }

    /// Enter on a tool sends the *opposite* of its current state; sending the
    /// same value would look like the toggle did nothing.
    #[test]
    fn toggling_a_tool_sends_the_inverse_of_its_state() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        // read_file is not on the basic shortlist, so it lives under Advanced.
        select(&mut menu, &s, "Advanced tools");
        menu.handle(Input::Enter, &s);

        // read_file starts enabled, so choosing it must disable it.
        select(&mut menu, &s, "Read files");
        match menu.handle(Input::Enter, &s) {
            Outcome::Act(effects) => match &effects[0] {
                Effect::Send(ClientMessage::UpdateToolConfig { tool, enabled }) => {
                    assert_eq!(tool, "read_file");
                    assert!(!enabled, "an enabled tool toggles off");
                }
                other => panic!("wrong message: {other:?}"),
            },
            other => panic!("expected an action, got {other:?}"),
        }
    }

    #[test]
    fn a_disabled_tool_toggles_on() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Basic tools");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Shell commands");
        match menu.handle(Input::Enter, &s) {
            Outcome::Act(effects) => match &effects[0] {
                Effect::Send(ClientMessage::UpdateToolConfig { enabled, .. }) => {
                    assert!(enabled, "a disabled tool toggles on");
                }
                other => panic!("wrong message: {other:?}"),
            },
            other => panic!("expected an action, got {other:?}"),
        }
    }

    #[test]
    fn context_strategy_and_offline_mode_send_their_settings() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Context strategy");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "truncate");
        match menu.handle(Input::Enter, &s) {
            Outcome::Act(effects) => assert_eq!(
                effects,
                vec![Effect::Send(ClientMessage::UpdateContextStrategy {
                    strategy: "truncate".into(),
                })],
            ),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn offline_mode_can_be_turned_on() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Offline mode");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Offline");
        match menu.handle(Input::Enter, &s) {
            Outcome::Act(effects) => assert_eq!(
                effects,
                vec![Effect::Send(ClientMessage::UpdateOfflineMode { enabled: true })],
            ),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn compact_and_clear_are_reachable_from_the_root() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Compact now");
        assert_eq!(
            menu.handle(Input::Enter, &s),
            Outcome::Act(vec![Effect::Send(ClientMessage::Compact)]),
        );

        let mut menu = Menu::new();
        select(&mut menu, &s, "Clear session");
        assert_eq!(
            menu.handle(Input::Enter, &s),
            Outcome::Act(vec![Effect::Send(ClientMessage::ClearSession)]),
        );
    }

    #[test]
    fn the_close_entry_closes() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Close");
        assert_eq!(menu.handle(Input::Enter, &s), Outcome::Close);
    }

    /// A rewind asks for a preview first rather than acting: the preview says
    /// what would be lost, and losing turns silently would be unrecoverable.
    #[test]
    fn choosing_a_checkpoint_asks_for_a_preview_first() {
        let mut s = session();
        s.apply(AgentMessage::RewindCheckpoint {
            id: "cp-1".into(), preview: "earlier".into(),
            message_count: 4, keep_on_restore: false,
        });
        let mut menu = Menu::new();
        select(&mut menu, &s, "Rewind");
        menu.handle(Input::Enter, &s);
        menu.handle(Input::Char('g'), &s);

        match menu.handle(Input::Enter, &s) {
            Outcome::Act(effects) => assert_eq!(
                effects,
                vec![Effect::Send(ClientMessage::RewindPreview {
                    checkpoint_id: "cp-1".into(),
                })],
                "previews rather than rewinding outright",
            ),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn an_informational_row_does_nothing() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Agents");
        menu.handle(Input::Enter, &s);
        // Agent definitions are files, not settings.
        assert_eq!(menu.handle(Input::Enter, &s), Outcome::Stay);
    }

    #[test]
    fn subagent_and_web_model_pages_offer_inherit_first() {
        let s = session();
        for (entry, page) in [("Subagent model", Page::SubagentModel),
                              ("Web tool model", Page::WebModel)] {
            let mut menu = Menu::new();
            select(&mut menu, &s, "Settings");
            menu.handle(Input::Enter, &s);
            select(&mut menu, &s, entry);
            menu.handle(Input::Enter, &s);
            assert_eq!(*menu.page(), page);
            assert_eq!(menu.items(&s)[0].label, "Inherit");
        }
    }

    // ── Empty states ──────────────────────────────────────────────────────

    /// An empty page must say so and offer a way back, not be a dead end.
    #[test]
    fn an_empty_rewind_page_offers_a_way_back() {
        let s = session(); // no checkpoints
        let mut menu = Menu::new();
        select(&mut menu, &s, "Rewind");
        menu.handle(Input::Enter, &s);
        assert_eq!(*menu.page(), Page::Rewind);

        let items = menu.items(&s);
        assert_eq!(items.len(), 1);
        assert!(items[0].label.contains("No checkpoints"));

        menu.handle(Input::Enter, &s);
        assert_eq!(*menu.page(), Page::Root, "the row goes back");
    }

    #[test]
    fn a_session_with_no_endpoints_does_not_break_the_model_page() {
        let s = Session::new(); // never received init
        let mut menu = Menu::new();
        select(&mut menu, &s, "Main model");
        menu.handle(Input::Enter, &s);
        assert!(menu.items(&s).is_empty());
        // Navigation and Enter on an empty list must not panic.
        menu.handle(Input::Down, &s);
        menu.handle(Input::Up, &s);
        assert_eq!(menu.handle(Input::Enter, &s), Outcome::Stay);
    }

    // ── Rendering ─────────────────────────────────────────────────────────

    #[test]
    fn the_page_title_and_hint_are_drawn() {
        let s = session();
        let mut menu = Menu::new();
        let out = draw(&mut menu, &s, 70, 20);
        assert!(out.contains("Menu"), "title: {out:?}");
        assert!(out.contains("Enter choose"), "key hint");
        assert!(out.contains("Main model"), "entries");
    }

    #[test]
    fn the_selected_row_is_marked() {
        let s = session();
        let mut menu = Menu::new();
        assert!(draw(&mut menu, &s, 70, 20).contains("❯ Main model"));
    }

    #[test]
    fn a_nested_page_shows_a_trail_and_says_escape_goes_back() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        let out = draw(&mut menu, &s, 70, 20);
        assert!(out.contains("Esc back"), "hint changes when nested: {out:?}");
    }

    /// A list longer than the window must scroll and say how much is hidden.
    #[test]
    fn a_long_list_scrolls_and_reports_what_is_hidden() {
        let mut s = session();
        // More tools than fit in a short window.
        let tools: Vec<ToolInfo> = (0..30)
            .map(|i| ToolInfo { name: format!("tool_{i}"), enabled: i % 2 == 0 })
            .collect();
        s.apply(AgentMessage::Init(Box::new(Init {
            available_tools: tools,
            endpoints: vec![endpoint("First", "first-1")],
            ..Default::default()
        })));

        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Advanced tools");
        menu.handle(Input::Enter, &s);

        let out = draw(&mut menu, &s, 70, 12);
        assert!(out.contains("more"), "reports hidden rows: {out:?}");

        // Walking down must bring the selection into view rather than losing it.
        for _ in 0..20 {
            menu.handle(Input::Down, &s);
        }
        let out = draw(&mut menu, &s, 70, 12);
        assert!(out.contains('❯'), "the cursor stays visible: {out:?}");
    }

    #[test]
    fn drawing_survives_any_screen_size() {
        let s = session();
        for (cols, rows) in [(1usize, 1usize), (4, 2), (10, 3), (20, 5), (200, 60)] {
            let mut menu = Menu::new();
            let mut canvas = Canvas::new(cols, rows);
            canvas.begin_frame();
            menu.draw(&mut canvas, Rect::new(0, 0, rows, cols), &s, 75);
            let mut sink = Vec::new();
            canvas.flush(&mut sink).unwrap();
        }
    }

    #[test]
    fn a_frame_with_the_menu_never_erases_scrollback() {
        let s = session();
        let mut menu = Menu::new();
        let mut canvas = Canvas::new(70, 20);
        canvas.begin_frame();
        menu.draw(&mut canvas, Rect::new(0, 0, 20, 70), &s, 75);
        let mut sink = Vec::new();
        canvas.flush(&mut sink).unwrap();
        let text = String::from_utf8_lossy(&sink);
        assert!(!text.contains("\x1b[3J"));
    }
    /// The basic list is a shortlist, not everything — that is the whole point of
    /// there being two.
    #[test]
    fn the_basic_list_is_a_shortlist_and_advanced_is_everything() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Basic tools");
        menu.handle(Input::Enter, &s);
        let basic: Vec<String> = menu.items(&s).iter().map(|i| i.label.clone()).collect();
        assert!(basic.iter().any(|l| l == "Shell commands"), "basic has shell: {basic:?}");
        assert!(!basic.iter().any(|l| l == "Read files"), "basic excludes reads: {basic:?}");

        menu.handle(Input::Escape, &s);
        select(&mut menu, &s, "Advanced tools");
        menu.handle(Input::Enter, &s);
        let advanced: Vec<String> = menu.items(&s).iter().map(|i| i.label.clone()).collect();
        assert!(advanced.iter().any(|l| l == "Read files"), "advanced has reads: {advanced:?}");
        assert!(advanced.iter().any(|l| l == "Shell commands"),
                "and the basic ones too: {advanced:?}");
        assert!(advanced.len() > basic.len());
    }

    /// The row says whether anything is off, which is what a reader wants from it.
    /// `shell_exec` is disabled in the test session and is on the basic shortlist.
    #[test]
    fn the_settings_rows_count_what_is_disabled() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        let rows: Vec<(String, String)> = menu
            .items(&s)
            .iter()
            .map(|i| (i.label.clone(), i.description.clone()))
            .collect();
        let basic = rows.iter().find(|(l, _)| l == "Basic tools").expect("the basic row");
        assert_eq!(basic.1, "1 disabled", "got {rows:?}");
    }

    /// A tool the label list has never heard of still appears, under its own name.
    #[test]
    fn an_unknown_tool_keeps_its_name() {
        assert_eq!(tool_label("some_new_tool"), "some_new_tool");
        assert_eq!(tool_label("read_file"), "Read files");
    }

    /// Choosing a mode has to set that mode. This sent `ToggleAutoMode` from both
    /// arms of an identical `if` and never set anything, so picking "Plan mode"
    /// toggled the agent's auto mode instead of entering plan mode.
    #[test]
    fn choosing_a_mode_sets_that_mode() {
        for (label, want) in [
            ("Ask each time", PermissionMode::Ask),
            ("Auto-accept edits", PermissionMode::AutoAccept),
            ("Plan mode", PermissionMode::Plan),
            ("Approve everything", PermissionMode::AllowAll),
        ] {
            let s = session();
            let mut menu = Menu::new();
            select(&mut menu, &s, "Settings");
            menu.handle(Input::Enter, &s);
            select(&mut menu, &s, "Permission mode");
            menu.handle(Input::Enter, &s);
            select(&mut menu, &s, label);
            match menu.handle(Input::Enter, &s) {
                Outcome::SetPermission(mode) => assert_eq!(mode, want, "for {label}"),
                other => panic!("{label} gave {other:?}"),
            }
        }
    }

    /// And it sends nothing: the mode is this client's gate, not a setting the
    /// agent is told about.
    #[test]
    fn choosing_a_mode_sends_nothing_to_the_agent() {
        let s = session();
        let mut menu = Menu::new();
        select(&mut menu, &s, "Settings");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Permission mode");
        menu.handle(Input::Enter, &s);
        select(&mut menu, &s, "Plan mode");
        assert!(
            !matches!(menu.handle(Input::Enter, &s), Outcome::Act(_)),
            "no outgoing message for a local gate",
        );
    }

}
