// SPDX-License-Identifier: Apache-2.0
//! Describing the active model's reasoning settings.
//!
//! A port of `model-display.ts`. The labels feed the context bar, so what is said
//! there matches what the menu will do — a bar reading "thinking on, high" beside
//! a menu that disagrees is worse than no label.
//!
//! The Anthropic budget deserves a note: it is a token count, but presented as
//! low/medium/high/xhigh, and the levels are clamped to the endpoint's own output
//! limit. Two levels can therefore collapse onto the same number, in which case
//! the duplicate is dropped rather than offering the user two identical choices.

use forge_agent_proto::{ChatGptReasoningEffort, EndpointInfo, ProviderToggle};

fn toggle_label(toggle: ProviderToggle) -> &'static str {
    match toggle {
        ProviderToggle::ProviderDefault => "default",
        ProviderToggle::On => "on",
        ProviderToggle::Off => "off",
    }
}

fn effort_label(effort: ChatGptReasoningEffort) -> &'static str {
    match effort {
        ChatGptReasoningEffort::ProviderDefault => "default",
        ChatGptReasoningEffort::None => "none",
        ChatGptReasoningEffort::Minimal => "minimal",
        ChatGptReasoningEffort::Low => "low",
        ChatGptReasoningEffort::Medium => "medium",
        ChatGptReasoningEffort::High => "high",
        ChatGptReasoningEffort::Xhigh => "xhigh",
    }
}

/// The named budget levels for an endpoint, clamped to what it can output.
///
/// Duplicates are dropped: with a small `max_output_tokens` several levels clamp
/// to the same count, and offering the same number twice under different names
/// would be a menu that does nothing.
pub fn budget_levels(ep: &EndpointInfo) -> Vec<(&'static str, u32)> {
    let max_budget = ep.max_output_tokens.saturating_sub(1).max(1024);
    let mut levels: Vec<(&'static str, u32)> = [
        ("low", 1024u32),
        ("medium", 4096),
        ("high", 8192),
        ("xhigh", 32768),
    ]
    .into_iter()
    .map(|(label, tokens)| (label, tokens.min(max_budget)))
    .collect();
    levels.dedup_by(|a, b| a.1 == b.1);
    levels
}

/// The Anthropic budget as a name, or the raw count when it matches no level.
fn anthropic_budget_label(ep: &EndpointInfo) -> String {
    let current = ep.reasoning.anthropic.budget_tokens;
    match budget_levels(ep).into_iter().find(|(_, tokens)| *tokens == current) {
        Some((label, _)) => label.to_string(),
        // A hand-edited config can hold anything; show the number rather than
        // rounding it to a name it does not have.
        None => format!("{current} tokens"),
    }
}

/// The full reasoning description, for a settings screen.
pub fn reasoning_display(ep: &EndpointInfo) -> String {
    match ep.endpoint_type.as_str() {
        "anthropic" => format!(
            "thinking {}, {}",
            toggle_label(ep.reasoning.anthropic.thinking),
            anthropic_budget_label(ep),
        ),
        "chatgpt_codex" => {
            format!("reasoning {}", effort_label(ep.reasoning.chatgpt_codex.effort))
        }
        _ => format!(
            "thinking {}, preserve {}",
            toggle_label(ep.reasoning.open_ai_compatible.thinking),
            toggle_label(ep.reasoning.open_ai_compatible.preserve_thinking),
        ),
    }
}

/// The short label for the context bar, or `None` when thinking is not on.
///
/// Absent rather than "thinking off": the bar is a status line, and a row of
/// negatives tells the reader nothing they need.
pub fn thinking_intensity(ep: &EndpointInfo) -> Option<String> {
    match ep.endpoint_type.as_str() {
        "anthropic" => (ep.reasoning.anthropic.thinking == ProviderToggle::On)
            .then(|| format!("thinking {}", anthropic_budget_label(ep))),
        "chatgpt_codex" => {
            let effort = ep.reasoning.chatgpt_codex.effort;
            (effort != ChatGptReasoningEffort::ProviderDefault
                && effort != ChatGptReasoningEffort::None)
                .then(|| format!("thinking {}", effort_label(effort)))
        }
        _ => (ep.reasoning.open_ai_compatible.thinking == ProviderToggle::On)
            .then(|| "thinking on".to_string()),
    }
}

/// True for a genuine xAI endpoint — the only provider the priority-tier toggle
/// applies to.
pub fn is_xai(ep: &EndpointInfo) -> bool {
    ep.base_url.contains("x.ai")
}

/// The endpoint the session is currently using.
///
/// Matched by name first, then by model id with a matching context window, then
/// by model id alone. Several endpoints can share a model id — the same model
/// behind different gateways — so the name is the only reliable key, and the
/// fallbacks exist for a session whose name has drifted from the config.
pub fn active_endpoint<'a>(
    endpoints: &'a [EndpointInfo],
    model_name: &str,
    model_id: &str,
    max_context_tokens: usize,
) -> Option<&'a EndpointInfo> {
    endpoints
        .iter()
        .find(|ep| ep.name == model_name)
        .or_else(|| {
            endpoints.iter().find(|ep| {
                ep.model_id == model_id && ep.max_context_tokens == max_context_tokens
            })
        })
        .or_else(|| endpoints.iter().find(|ep| ep.model_id == model_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_agent_proto::{
        AnthropicReasoningConfig, ChatGptCodexReasoningConfig, EndpointReasoningConfig,
        OpenAiCompatibleReasoningConfig,
    };

    fn endpoint(name: &str, kind: &str) -> EndpointInfo {
        EndpointInfo {
            name: name.into(),
            base_url: "https://api.example.invalid".into(),
            model_id: format!("{name}-1"),
            max_context_tokens: 200_000,
            max_output_tokens: 65_536,
            endpoint_type: kind.into(),
            reasoning: EndpointReasoningConfig::default(),
            xai_priority_tier: false,
        }
    }

    #[test]
    fn anthropic_shows_thinking_and_a_named_budget() {
        let mut ep = endpoint("Claude", "anthropic");
        ep.reasoning.anthropic = AnthropicReasoningConfig {
            thinking: ProviderToggle::On,
            budget_tokens: 8192,
        };
        assert_eq!(reasoning_display(&ep), "thinking on, high");
        assert_eq!(thinking_intensity(&ep).unwrap(), "thinking high");
    }

    #[test]
    fn codex_shows_its_effort() {
        let mut ep = endpoint("Codex", "chatgpt_codex");
        ep.reasoning.chatgpt_codex = ChatGptCodexReasoningConfig {
            effort: ChatGptReasoningEffort::Xhigh,
        };
        assert_eq!(reasoning_display(&ep), "reasoning xhigh");
        assert_eq!(thinking_intensity(&ep).unwrap(), "thinking xhigh");
    }

    #[test]
    fn openai_compatible_shows_both_toggles() {
        let mut ep = endpoint("Local", "open_ai");
        ep.reasoning.open_ai_compatible = OpenAiCompatibleReasoningConfig {
            thinking: ProviderToggle::On,
            preserve_thinking: ProviderToggle::Off,
        };
        assert_eq!(reasoning_display(&ep), "thinking on, preserve off");
        assert_eq!(thinking_intensity(&ep).unwrap(), "thinking on");
    }

    /// The bar shows nothing rather than a negative — a status line full of
    /// "off" tells the reader nothing.
    #[test]
    fn the_bar_label_is_absent_when_thinking_is_not_on() {
        let ep = endpoint("Claude", "anthropic"); // defaults: thinking On
        assert!(thinking_intensity(&ep).is_some(), "the default is on");

        let mut off = endpoint("Claude", "anthropic");
        off.reasoning.anthropic.thinking = ProviderToggle::Off;
        assert!(thinking_intensity(&off).is_none());

        let mut dflt = endpoint("Codex", "chatgpt_codex");
        dflt.reasoning.chatgpt_codex.effort = ChatGptReasoningEffort::ProviderDefault;
        assert!(thinking_intensity(&dflt).is_none(), "provider default says nothing");

        dflt.reasoning.chatgpt_codex.effort = ChatGptReasoningEffort::None;
        assert!(thinking_intensity(&dflt).is_none());
    }

    // ── Budget levels ─────────────────────────────────────────────────────

    #[test]
    fn budget_levels_are_the_four_named_ones_when_there_is_room() {
        let ep = endpoint("Claude", "anthropic");
        let levels = budget_levels(&ep);
        assert_eq!(
            levels,
            vec![("low", 1024), ("medium", 4096), ("high", 8192), ("xhigh", 32768)],
        );
    }

    /// With a small output limit the levels clamp together, and identical ones
    /// must not be offered twice under different names.
    #[test]
    fn levels_clamped_to_the_same_count_are_deduplicated() {
        let mut ep = endpoint("Small", "anthropic");
        ep.max_output_tokens = 2048;
        let levels = budget_levels(&ep);
        let counts: Vec<u32> = levels.iter().map(|(_, n)| *n).collect();
        let mut unique = counts.clone();
        unique.dedup();
        assert_eq!(counts, unique, "no repeated counts: {levels:?}");
        assert!(!levels.is_empty(), "at least one level remains");
    }

    /// A floor of 1024 keeps the list usable even for a tiny endpoint.
    #[test]
    fn a_tiny_output_limit_still_yields_a_level() {
        let mut ep = endpoint("Tiny", "anthropic");
        ep.max_output_tokens = 16;
        let levels = budget_levels(&ep);
        assert!(!levels.is_empty());
        assert!(levels.iter().all(|(_, n)| *n >= 1024));
    }

    /// A hand-edited config can hold a count matching no level; show the number
    /// rather than rounding it to a name it does not have.
    #[test]
    fn an_unrecognised_budget_shows_its_number() {
        let mut ep = endpoint("Claude", "anthropic");
        ep.reasoning.anthropic = AnthropicReasoningConfig {
            thinking: ProviderToggle::On,
            budget_tokens: 3000,
        };
        assert_eq!(thinking_intensity(&ep).unwrap(), "thinking 3000 tokens");
    }

    // ── Endpoint matching ─────────────────────────────────────────────────

    #[test]
    fn the_active_endpoint_is_found_by_name_first() {
        let endpoints = vec![endpoint("First", "anthropic"), endpoint("Second", "open_ai")];
        let found = active_endpoint(&endpoints, "Second", "First-1", 200_000).unwrap();
        assert_eq!(found.name, "Second", "the name wins over a matching model id");
    }

    /// Several endpoints can share a model id — the same model behind different
    /// gateways — so the context window disambiguates before the bare id.
    #[test]
    fn a_shared_model_id_is_disambiguated_by_the_context_window() {
        let mut a = endpoint("A", "open_ai");
        let mut b = endpoint("B", "open_ai");
        a.model_id = "shared".into();
        b.model_id = "shared".into();
        a.max_context_tokens = 100_000;
        b.max_context_tokens = 200_000;
        let endpoints = vec![a, b];

        let found = active_endpoint(&endpoints, "gone", "shared", 200_000).unwrap();
        assert_eq!(found.name, "B");
    }

    #[test]
    fn a_bare_model_id_is_the_last_resort() {
        let endpoints = vec![endpoint("A", "open_ai")];
        let found = active_endpoint(&endpoints, "gone", "A-1", 999).unwrap();
        assert_eq!(found.name, "A");
    }

    #[test]
    fn no_match_is_none_rather_than_the_first_endpoint() {
        let endpoints = vec![endpoint("A", "open_ai")];
        assert!(active_endpoint(&endpoints, "gone", "missing", 1).is_none());
        assert!(active_endpoint(&[], "any", "any", 1).is_none());
    }

    #[test]
    fn xai_endpoints_are_recognised_by_their_host() {
        let mut ep = endpoint("Grok", "open_ai");
        ep.base_url = "https://api.x.ai/v1".into();
        assert!(is_xai(&ep));

        ep.base_url = "https://api.example.invalid/v1".into();
        assert!(!is_xai(&ep));
    }
}
