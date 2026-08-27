// SPDX-License-Identifier: Apache-2.0
//! Slash commands.
//!
//! The table is the TypeScript client's, verbatim — same commands, same
//! descriptions, same aliases — so anything typed out of habit still works.
//!
//! Completion is offered as soon as the input starts with `/`, matching on
//! prefix. Aliases are kept out of the suggestion list but still accepted when
//! typed in full: showing `/ctx`, `/tokens` and `/usage` as three separate
//! choices is noise, but somebody who knows `/ctx` should not be told it does
//! not exist.

/// One entry in the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub command:     &'static str,
    pub description: &'static str,
    /// Aliases are accepted but not suggested.
    pub alias:       bool,
}

/// What a command asks for.
///
/// Deliberately does not carry the effects themselves: the app decides how to
/// act, so this stays a pure parse of what the user typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Quit,
    Restart,
    Clear,
    Compact,
    /// Put the agent's last message on the system clipboard.
    Copy,
    Usage,
    Plan,
    Login,
    /// Show the log file's path.
    Log,
    /// Report which build this is, and which agent it is talking to.
    Version,
    /// List the commands.
    Help,
    /// Open the menu at a particular page.
    OpenMenu(Page),
    /// Typed a `/` that matches nothing.
    Unknown(String),
}

/// Menu pages a command can jump to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Models,
    Settings,
    ContextStrategy,
    Subagents,
    Rewind,
    Sessions,
    Thinking,
}

/// Every command, in the order the TypeScript client listed them.
pub const TABLE: &[Entry] = &[
    Entry { command: "/quit",      description: "Exit Forge", alias: false },
    Entry { command: "/restart",   description: "Restart the agent process", alias: false },
    Entry { command: "/clear",     description: "Start a fresh session", alias: false },
    Entry { command: "/compact",   description: "Compact conversation context", alias: false },
    Entry { command: "/copy",      description: "Copy the agent's last message to the clipboard", alias: false },
    Entry { command: "/revert",    description: "Restore a previous user turn and code snapshot", alias: false },
    Entry { command: "/usage",     description: "Show token and context usage", alias: false },
    Entry { command: "/model",     description: "Open model selector", alias: false },
    Entry { command: "/thinking",  description: "Open reasoning and thinking controls", alias: false },
    Entry { command: "/context",   description: "Open context strategy settings", alias: false },
    Entry { command: "/settings",  description: "Open settings", alias: false },
    Entry { command: "/subagent",  description: "Configure subagents", alias: false },
    Entry { command: "/plan",      description: "Enter plan mode", alias: false },
    Entry { command: "/resume",    description: "Resume a saved session", alias: false },
    Entry { command: "/sessions",  description: "Open saved sessions", alias: false },
    Entry { command: "/log",       description: "Show current session log path", alias: false },
    Entry { command: "/version",   description: "Show the running build and the agent it is talking to", alias: false },
    Entry { command: "/login",     description: "Log in to Claude or ChatGPT", alias: false },
    Entry { command: "/help",      description: "Show command help", alias: false },
    Entry { command: "/exit",      description: "Exit Forge", alias: false },
    // Aliases: accepted, not suggested.
    Entry { command: "/tokens",    description: "Alias for /usage", alias: true },
    Entry { command: "/ctx",       description: "Alias for /usage", alias: true },
    Entry { command: "/agents",    description: "Alias for /subagent", alias: true },
    Entry { command: "/think",     description: "Alias for /thinking", alias: true },
    Entry { command: "/reasoning", description: "Alias for /thinking", alias: true },
    Entry { command: "/reconnect", description: "Alias for /restart", alias: true },
    Entry { command: "/ver",       description: "Alias for /version", alias: true },
];

/// True when the input should offer completions.
pub fn is_command(input: &str) -> bool {
    input.starts_with('/')
}

/// Commands whose name starts with `input`, aliases excluded.
///
/// An exact match returns just itself, so a fully typed command does not sit
/// under a list of longer ones it happens to prefix — `/log` should not be
/// buried beneath `/login`.
pub fn complete(input: &str) -> Vec<&'static Entry> {
    if !is_command(input) {
        return Vec::new();
    }
    let typed = input.trim();
    if let Some(exact) = TABLE.iter().find(|e| e.command == typed) {
        return vec![exact];
    }
    TABLE
        .iter()
        .filter(|e| !e.alias && e.command.starts_with(typed))
        .collect()
}

/// Parse a submitted line.
///
/// Returns `None` when it is not a command at all, so the caller sends it to the
/// agent as an ordinary message.
pub fn parse(input: &str) -> Option<Command> {
    let trimmed = input.trim();
    if !is_command(trimmed) {
        return None;
    }
    // Only the first word is the command; the rest is an argument, which today
    // only /login takes.
    let name = trimmed.split_whitespace().next().unwrap_or(trimmed);

    Some(match name {
        "/quit" | "/exit" => Command::Quit,
        "/restart" | "/reconnect" => Command::Restart,
        "/clear" => Command::Clear,
        "/compact" => Command::Compact,
        "/copy" => Command::Copy,
        "/usage" | "/tokens" | "/ctx" => Command::Usage,
        "/plan" => Command::Plan,
        "/login" => Command::Login,
        "/log" => Command::Log,
        "/version" | "/ver" => Command::Version,
        "/help" => Command::Help,
        "/model" => Command::OpenMenu(Page::Models),
        "/settings" => Command::OpenMenu(Page::Settings),
        "/context" => Command::OpenMenu(Page::ContextStrategy),
        "/subagent" | "/agents" => Command::OpenMenu(Page::Subagents),
        "/revert" => Command::OpenMenu(Page::Rewind),
        "/resume" | "/sessions" => Command::OpenMenu(Page::Sessions),
        "/thinking" | "/think" | "/reasoning" => Command::OpenMenu(Page::Thinking),
        other => Command::Unknown(other.to_string()),
    })
}

/// The command list, for `/help`.
pub fn help_text() -> String {
    let mut out = String::from("Commands:\n");
    for entry in TABLE.iter().filter(|e| !e.alias) {
        out.push_str(&format!("  {:<12} {}\n", entry.command, entry.description));
    }
    out.push_str("\nAliases: ");
    let aliases: Vec<&str> = TABLE.iter().filter(|e| e.alias).map(|e| e.command).collect();
    out.push_str(&aliases.join(", "));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_leading_slash_starts_a_command() {
        assert!(is_command("/model"));
        assert!(!is_command("model"));
        assert!(!is_command("what about /model"));
        assert!(!is_command(""));
    }

    #[test]
    fn ordinary_text_is_not_a_command() {
        assert_eq!(parse("hello there"), None);
        assert_eq!(parse(""), None);
        assert_eq!(parse("  "), None);
    }

    /// The one the user actually needed.
    #[test]
    fn resume_and_sessions_both_open_the_sessions_page() {
        assert_eq!(parse("/resume"), Some(Command::OpenMenu(Page::Sessions)));
        assert_eq!(parse("/sessions"), Some(Command::OpenMenu(Page::Sessions)));
    }

    #[test]
    fn every_command_in_the_table_parses_to_something_known() {
        for entry in TABLE {
            let parsed = parse(entry.command).expect("a command");
            assert!(
                !matches!(parsed, Command::Unknown(_)),
                "{} parsed as unknown", entry.command,
            );
        }
    }

    /// Aliases have to behave identically to what they alias, or they are a
    /// promise the help text does not keep.
    #[test]
    fn aliases_resolve_to_the_same_command() {
        assert_eq!(parse("/tokens"), parse("/usage"));
        assert_eq!(parse("/ctx"), parse("/usage"));
        assert_eq!(parse("/agents"), parse("/subagent"));
        assert_eq!(parse("/think"), parse("/thinking"));
        assert_eq!(parse("/reasoning"), parse("/thinking"));
        assert_eq!(parse("/reconnect"), parse("/restart"));
        assert_eq!(parse("/copy"), Some(Command::Copy));
        assert_eq!(parse("/exit"), parse("/quit"));
    }

    /// Every alias's description must name a command that exists.
    #[test]
    fn alias_descriptions_point_at_real_commands() {
        for entry in TABLE.iter().filter(|e| e.alias) {
            let target = entry
                .description
                .strip_prefix("Alias for ")
                .unwrap_or_else(|| panic!("{} has no target", entry.command));
            assert!(
                TABLE.iter().any(|e| e.command == target),
                "{} aliases {target}, which is not in the table", entry.command,
            );
        }
    }

    #[test]
    fn an_unknown_command_is_reported_rather_than_sent_to_the_agent() {
        match parse("/nonsense") {
            Some(Command::Unknown(name)) => assert_eq!(name, "/nonsense"),
            other => panic!("expected Unknown, got {other:?}"),
        }
    }

    /// An argument must not stop the command being recognised — /login takes one.
    #[test]
    fn arguments_do_not_prevent_recognition() {
        assert_eq!(parse("/login chatgpt"), Some(Command::Login));
        assert_eq!(parse("/model   "), Some(Command::OpenMenu(Page::Models)));
    }

    // ── Completion ────────────────────────────────────────────────────────

    #[test]
    fn a_bare_slash_suggests_everything_except_aliases() {
        let all = complete("/");
        assert!(all.len() > 10, "plenty offered");
        assert!(all.iter().all(|e| !e.alias), "no aliases in the list");
    }

    #[test]
    fn a_prefix_narrows_the_suggestions() {
        let matches = complete("/se");
        let names: Vec<&str> = matches.iter().map(|e| e.command).collect();
        assert!(names.contains(&"/settings"), "got {names:?}");
        assert!(names.contains(&"/sessions"));
        assert!(!names.contains(&"/model"));
    }

    /// A fully typed command must not be buried under longer ones it prefixes.
    #[test]
    fn an_exact_match_stands_alone() {
        let matches = complete("/log");
        assert_eq!(matches.len(), 1, "just /log, not /login too");
        assert_eq!(matches[0].command, "/log");
    }

    #[test]
    fn a_prefix_matching_nothing_suggests_nothing() {
        assert!(complete("/zzz").is_empty());
    }

    #[test]
    fn plain_text_offers_no_completions() {
        assert!(complete("hello").is_empty());
    }

    // ── Help ──────────────────────────────────────────────────────────────

    #[test]
    fn the_help_text_lists_the_commands_and_aliases() {
        let help = help_text();
        assert!(help.contains("/resume"), "the commands: {help}");
        assert!(help.contains("Resume a saved session"), "with descriptions");
        assert!(help.contains("/ctx"), "and the aliases");
        // Aliases are listed once, at the bottom, not as their own entries.
        assert_eq!(help.matches("Alias for").count(), 0, "no alias descriptions inline");
    }

    /// The table must not contain duplicates: two entries for one name means one
    /// of them is unreachable.
    #[test]
    fn no_command_appears_twice() {
        let mut seen: Vec<&str> = TABLE.iter().map(|e| e.command).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before, "a command is listed twice");
    }

    #[test]
    fn every_command_starts_with_a_slash() {
        for entry in TABLE {
            assert!(entry.command.starts_with('/'), "{} is malformed", entry.command);
        }
    }
}
