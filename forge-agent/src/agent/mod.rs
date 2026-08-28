// SPDX-License-Identifier: Apache-2.0
/// Truncate to at most `max` **characters**, cutting on a character boundary.
///
/// `&s[..n]` slices by byte, and panics when byte `n` lands inside a multi-byte
/// character. That is not an edge case here: everything truncated is text
/// someone typed or a model wrote — an emoji, a CJK identifier, an accented
/// word, or the box-drawing characters models use for diagrams. One straddling
/// the cut takes the process down, which is what a `\u{258E}` in a first
/// message did to a live session.
pub fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((byte_index, _)) => &s[..byte_index],
        None => s,
    }
}

pub mod agent_def;
pub mod compaction;
pub mod conversation_log;
mod core;
pub mod log_types;
pub mod rewind;
pub mod subagent;

pub use core::{
    PersistEndpoint,
    Agent, AgentEvent, QuestionItem, QuestionOption, TokenUsageSnapshot, ToolKindEvent, UserAction,
};
