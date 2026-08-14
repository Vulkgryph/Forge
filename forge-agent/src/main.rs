// SPDX-License-Identifier: Apache-2.0
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::mpsc;

use crate::agent::agent_def;
use crate::agent::conversation_log::{self, ConversationLog};
use crate::agent::Agent;

mod agent;
mod api;
mod auth;
mod config;
mod headless;
mod tools;

/// Format an OAuth token-refresh failure into a clean multi-line message.
/// The underlying error has already been distilled to "<message> (code: X)" by
/// auth::parse_oauth_error_body — we just decide what next-step advice to give
/// based on common codes, and frame it nicely.
fn print_oauth_refresh_error(provider: &str, error: &str, relogin_command: &str) {
    eprintln!();
    eprintln!("forge: cannot use saved {} credentials.", provider);

    // Pull out a human-readable cause based on the parsed error code.
    let cause = if error.contains("rate-limited") || error.contains("HTTP 429") {
        "The auth server is rate-limiting refresh attempts. forge will not retry until the cooldown lifts; you can wait it out, or skip the wait by getting fresh tokens via a full re-login."
    } else if error.contains("refresh_token_reused") {
        "Your refresh token was already used elsewhere (another login replaced it)."
    } else if error.contains("invalid_grant") || error.contains("expired_token") {
        "Your saved session has expired or been revoked."
    } else if error.contains("invalid_client") || error.contains("unauthorized_client") {
        "The OAuth client is no longer authorized."
    } else if error.contains("HTTP 5") {
        "The auth server is temporarily unavailable. Try again in a moment."
    } else {
        // Fall back to whatever the server told us.
        error
    };

    eprintln!("        {}", cause);
    eprintln!("        Re-authenticate: {}", relogin_command);
    eprintln!();
}

#[derive(Parser)]
#[command(name = "forge-agent", version = env!("CARGO_PKG_VERSION"))]
struct Cli {
    /// Bypass all tool approval prompts (dangerous!)
    #[arg(long)]
    dangerously_allow_all: bool,

    /// Run in headless mode: JSON-newline protocol on stdin/stdout
    #[arg(long)]
    headless: bool,

    /// Resume a specific session by ID (used by headless/UI)
    #[arg(long)]
    resume_session: Option<String>,

    /// Log in to ChatGPT Codex via OAuth (for ChatGPT subscription Codex)
    #[arg(long)]
    login_chatgpt: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Handle --login-chatgpt before anything else.
    if cli.login_chatgpt {
        // Standalone forge-agent --login-chatgpt. We own stdin, so the paste
        // fallback is available if the localhost callback port is busy.
        //
        // wait_for_callback_on races the TCP listener against a tokio stdin
        // reader. When the listener wins, the stdin future is dropped but the
        // underlying blocking worker thread keeps holding stdin, preventing
        // tokio's runtime from shutting down cleanly. Explicit exit bypasses
        // the hang.
        auth::login_chatgpt(true).await?;
        std::process::exit(0);
    }

    // Load configuration
    let mut app_config = config::AppConfig::load()?;

    if !cli.headless {
        eprintln!(
            "forge-agent: only headless mode is supported. Use --headless or run via the UI."
        );
        eprintln!(
            "Usage: forge-agent --headless [--resume-session <id>] [--dangerously-allow-all]"
        );
        eprintln!("       forge-agent --login-chatgpt   (log in to ChatGPT Codex subscription)");
        std::process::exit(1);
    }

    // Owned rather than borrowed: the ChatGPT Codex discovery block below
    // needs `&mut app_config.models.endpoints` (to persist newly discovered
    // and pruned models), which can't coexist with a borrow of one of its
    // own elements held across that point.
    let endpoint = match app_config.default_endpoint() {
        Some(ep) => ep.clone(),
        None => {
            let err = serde_json::json!({
                "type": "error",
                "message": format!(
                    "No endpoint '{}' found in config. Edit ~/.config/forge/config.toml \
                     or re-run ./install.sh to reconfigure.",
                    app_config.models.default
                )
            });
            println!("{}", err);
            return Ok(());
        }
    };
    // Resolve "auto" model ID. The strategy differs by endpoint:
    //   - OpenAI-compatible / Anthropic: query /v1/models and pick the first.
    //   - ChatGPT Codex: there's no usable /v1/models endpoint, so query the
    //     Codex subscription model catalog (cached locally by the CLI) and
    //     pick the first.
    let model_id = if endpoint.model_id == "auto" {
        let resolved = match endpoint.endpoint_type {
            config::EndpointType::ChatGptCodex => {
                let models = auth::fetch_chatgpt_codex_models().await;
                models.first().map(|m| m.id.clone())
            }
            _ => {
                let probe_client = api::ApiClient::from_endpoint(&endpoint, None);
                probe_client.resolve_auto_model_id().await
            }
        };
        match resolved {
            Some(id) => {
                eprintln!(
                    "forge: auto-detected model '{}' from {}",
                    id, endpoint.base_url
                );
                id
            }
            None => {
                eprintln!("forge: warning — model_id = \"auto\" but no models could be discovered; using \"default\"");
                "default".to_string()
            }
        }
    } else {
        endpoint.model_id.clone()
    };
    let mut max_context_tokens = endpoint.max_context_tokens;

    // Anthropic and OpenAI-compatible endpoints authenticate with the endpoint's
    // API key (carried in config). Only the ChatGPT Codex subscription uses an
    // OAuth access token, loaded here.
    let oauth_token = match endpoint.endpoint_type {
        config::EndpointType::ChatGptCodex => {
            let http = reqwest::Client::new();
            match auth::get_valid_chatgpt_token(&http).await {
                Ok(t) => Some(t.access_token),
                Err(e) => {
                    print_oauth_refresh_error(
                        "ChatGPT Codex",
                        &e.to_string(),
                        "forge --login-chatgpt",
                    );
                    None
                }
            }
        }
        config::EndpointType::Anthropic | config::EndpointType::OpenAi => None,
    };

    let client = api::ApiClient::from_endpoint(&endpoint, oauth_token.clone());

    let mut all_endpoints = app_config.models.endpoints.clone();

    // In offline mode, this whole block only runs if the active endpoint is
    // itself ChatGPT Codex — otherwise it's pure incidental network traffic
    // for a catalog of models this session isn't even using.
    let chatgpt_logged_in = auth::load_chatgpt_tokens().is_some();
    if chatgpt_logged_in
        && (!app_config.agent.offline_mode
            || endpoint.endpoint_type == config::EndpointType::ChatGptCodex)
    {
        let http = reqwest::Client::new();
        let _ = auth::get_valid_chatgpt_token(&http).await;
        let (mut discovered, is_live) = auth::fetch_chatgpt_codex_models_with_provenance().await;
        // Only a genuine, non-empty live response is trustworthy enough to
        // prune by — an empty *live* result is more likely a transient
        // parsing/API hiccup than "you now have zero Codex models", and any
        // offline fallback (cache/CLI/hardcoded) is by definition not
        // current, so "missing from it" doesn't mean "no longer offered".
        let can_prune = is_live && !discovered.is_empty();
        let live_ids: std::collections::HashSet<String> =
            discovered.iter().map(|m| m.id.clone()).collect();
        if discovered.is_empty() {
            discovered.push(auth::ChatGptCodexModel {
                id: "gpt-5.4".to_string(),
                display_name: "gpt-5.4".to_string(),
                context_window: 258_400,
                max_output_tokens: 16_384,
            });
        }

        for model in discovered {
            if endpoint.endpoint_type == config::EndpointType::ChatGptCodex
                && endpoint.model_id == model.id
            {
                max_context_tokens = model.context_window;
            }

            if let Some(existing) = all_endpoints.iter_mut().find(|e| {
                e.endpoint_type == config::EndpointType::ChatGptCodex && e.model_id == model.id
            }) {
                existing.base_url = "https://chatgpt.com/backend-api/codex".to_string();
                existing.max_context_tokens = model.context_window;
                existing.max_output_tokens = model.max_output_tokens;
                existing.request_timeout_secs = config::default_request_timeout_secs();
                existing.endpoint_type = config::EndpointType::ChatGptCodex;
            } else {
                all_endpoints.push(config::ModelEndpoint {
                    name: model.display_name,
                    base_url: "https://chatgpt.com/backend-api/codex".to_string(),
                    model_id: model.id,
                    api_key: None,
                    max_context_tokens: model.context_window,
                    max_output_tokens: model.max_output_tokens,
                    request_timeout_secs: config::default_request_timeout_secs(),
                    endpoint_type: config::EndpointType::ChatGptCodex,
                    reasoning: config::EndpointReasoningConfig::default(),
                    xai_priority_tier: false,
                });
            }
        }

        // Remove ChatGPT Codex endpoints that are no longer offered — e.g. a
        // retired model like an old "gpt-5.4" entry once superseded models
        // take over. Scoped to ChatGPT Codex only: it's the only provider
        // here with a live catalog to check against; Anthropic, real
        // OpenAI, OpenRouter, and self-hosted/local endpoints have no
        // equivalent live discovery and are never touched by this.
        if can_prune {
            let before = all_endpoints.len();
            all_endpoints.retain(|e| {
                e.endpoint_type != config::EndpointType::ChatGptCodex
                    || live_ids.contains(&e.model_id)
            });
            if all_endpoints.len() != before {
                eprintln!(
                    "forge: removed {} ChatGPT Codex model(s) no longer offered",
                    before - all_endpoints.len()
                );
            }
        }
    }

    // ── xAI (Grok) model auto-discovery ──────────────────────────────────
    // Same idea as the Codex block above, but there's no login/OAuth concept
    // here — just whatever `api_key` a configured `api.x.ai` endpoint
    // already has. Grouped by (base_url, api_key) so multiple xAI accounts
    // would each get their own discovery pass, though in practice there's
    // usually just one. A failed/empty live query leaves that account's
    // entries untouched rather than deleting them.
    {
        let mut seen: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        let xai_accounts: Vec<(String, String)> = all_endpoints
            .iter()
            .filter(|e| {
                e.endpoint_type == config::EndpointType::OpenAi
                    && e.base_url.contains("api.x.ai")
                    && e.api_key.as_ref().is_some_and(|k| !k.is_empty())
            })
            .filter_map(|e| {
                let key = (e.base_url.clone(), e.api_key.clone().unwrap());
                seen.insert(key.clone()).then_some(key)
            })
            .collect();

        for (xai_base_url, xai_api_key) in xai_accounts {
            let Some(discovered) = auth::fetch_xai_models(&xai_base_url, &xai_api_key).await else {
                continue; // couldn't reach xAI right now — leave existing entries alone
            };
            if discovered.is_empty() {
                continue; // more likely a transient hiccup than "zero models available"
            }

            let live_ids: std::collections::HashSet<String> =
                discovered.iter().map(|m| m.id.clone()).collect();

            for model in &discovered {
                if endpoint.endpoint_type == config::EndpointType::OpenAi
                    && endpoint.base_url == xai_base_url
                    && endpoint.model_id == model.id
                {
                    if let Some(ctx) = model.context_length {
                        max_context_tokens = ctx;
                    }
                }

                if let Some(existing) = all_endpoints.iter_mut().find(|e| {
                    e.endpoint_type == config::EndpointType::OpenAi
                        && e.base_url == xai_base_url
                        && e.model_id == model.id
                }) {
                    if let Some(ctx) = model.context_length {
                        existing.max_context_tokens = ctx;
                    }
                    existing.api_key = Some(xai_api_key.clone());
                } else {
                    all_endpoints.push(config::ModelEndpoint {
                        name: auth::xai_display_name(&model.id),
                        base_url: xai_base_url.clone(),
                        model_id: model.id.clone(),
                        api_key: Some(xai_api_key.clone()),
                        max_context_tokens: model.context_length.unwrap_or(131_072),
                        max_output_tokens: 16_384,
                        request_timeout_secs: config::default_request_timeout_secs(),
                        endpoint_type: config::EndpointType::OpenAi,
                        reasoning: config::EndpointReasoningConfig::default(),
                        xai_priority_tier: false,
                    });
                }
            }

            let before = all_endpoints.len();
            all_endpoints.retain(|e| {
                !(e.endpoint_type == config::EndpointType::OpenAi && e.base_url == xai_base_url)
                    || live_ids.contains(&e.model_id)
            });
            if all_endpoints.len() != before {
                eprintln!(
                    "forge: removed {} xAI model(s) no longer offered",
                    before - all_endpoints.len()
                );
            }
        }
    }

    // Persist additions/updates/removals from both discovery passes above so
    // they don't have to be rediscovered (and re-pruned) fresh every launch.
    app_config.models.endpoints = all_endpoints.clone();
    let _ = app_config.save();

    // Channels
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let (action_tx, action_rx) = mpsc::unbounded_channel();

    let workspace_root = std::env::current_dir()?.to_string_lossy().to_string();
    let workspace_root_path = PathBuf::from(&workspace_root);
    let executor = tools::ToolExecutor::new(workspace_root.clone().into());
    let available_tools: Vec<headless::ToolInfo> = executor
        .toggleable_tool_names()
        .into_iter()
        .map(|name| headless::ToolInfo {
            enabled: !app_config.agent.disabled_tools.contains(&name),
            name,
        })
        .collect();

    // Context window detection happens in the background after the UI is up.
    // Doing it here would block the Init message and leave the UI blank.
    // The agent uses the configured value initially; if the server reports
    // a different value it will be used from the first model call onward.

    // Load agent definitions
    let agent_definitions = agent_def::load_agent_definitions(&workspace_root_path)?;
    let agent_defs_for_state = agent_definitions.clone();

    // Check if resuming a specific session
    let (hl_session_id, hl_log_path, hl_resume_meta) = if let Some(ref resume_id) =
        cli.resume_session
    {
        let sessions = conversation_log::scan_sessions(&workspace_root_path)?;
        let meta = sessions.into_iter().find(|m| m.id == *resume_id);
        match meta {
            Some(m) => {
                let lp = conversation_log::session_log_path(&workspace_root_path, &m.id);
                let id = m.id.clone();
                (id, lp, Some(m))
            }
            None => {
                // Send error as JSON so the UI can display it
                let err_msg = serde_json::json!({"type": "error", "message": format!("Session not found: {}. Searched in {}", resume_id, workspace_root_path.display())});
                println!("{}", err_msg);
                eprintln!(
                    "Session not found: {} (searched {}/.forge/sessions/)",
                    resume_id,
                    workspace_root_path.display()
                );
                return Ok(());
            }
        }
    } else {
        let session_id = conversation_log::generate_session_id();
        let log_path = conversation_log::session_log_path(&workspace_root_path, &session_id);
        (session_id, log_path, None)
    };

    // On resume, prefer the session's stored model over the global default.
    let (client, model_id, max_context_tokens) = if let Some(ref meta) = hl_resume_meta {
        if let Some(ep) = all_endpoints
            .iter()
            .find(|e| e.name == meta.model || e.model_id == meta.model)
        {
            let token = match ep.endpoint_type {
                config::EndpointType::ChatGptCodex => {
                    auth::load_chatgpt_tokens().map(|t| t.access_token)
                }
                config::EndpointType::Anthropic | config::EndpointType::OpenAi => None,
            };
            let c = api::ApiClient::from_endpoint(ep, token);
            (c, ep.model_id.clone(), ep.max_context_tokens)
        } else {
            (client, model_id, max_context_tokens)
        }
    } else {
        (client, model_id, max_context_tokens)
    };

    let log = ConversationLog::open(&hl_log_path)?;

    // Tag the client with the Forge session ID so Oxide can scope its KV
    // cache to this conversation and invalidate on session change.
    let mut client = client;
    client.forge_session_id = Some(hl_session_id.clone());

    let mut agent = if let Some(ref meta) = hl_resume_meta {
        Agent::resume(
            client,
            executor,
            app_config.agent.clone(),
            model_id.clone(),
            log,
            max_context_tokens,
            event_tx.clone(),
            action_tx.clone(),
            action_rx,
            agent_definitions,
            app_config.clone(),
            cli.dangerously_allow_all,
            hl_session_id.clone(),
            workspace_root_path.clone(),
            meta,
        )?
    } else {
        Agent::new(
            client,
            executor,
            app_config.agent.clone(),
            model_id.clone(),
            log,
            max_context_tokens,
            event_tx.clone(),
            action_tx.clone(),
            action_rx,
            agent_definitions,
            app_config.clone(),
            cli.dangerously_allow_all,
            hl_session_id.clone(),
            workspace_root_path.clone(),
        )
    };

    // Probe context window in background — updates the agent once resolved.
    // This must not block the main thread since Init hasn't been sent yet.
    {
        let probe_client = api::ApiClient::from_endpoint(&endpoint, oauth_token);
        let probe_model = model_id.clone();
        let probe_tx = action_tx.clone();
        let configured = max_context_tokens;
        tokio::spawn(async move {
            if let Some(detected) = probe_client.fetch_context_length(&probe_model).await {
                if detected != configured {
                    eprintln!(
                        "Context window: {} tokens (config: {})",
                        detected, configured
                    );
                    let _ = probe_tx.send(crate::agent::UserAction::UpdateContextWindow(detected));
                }
            }
        });
    }

    let agent_handle = tokio::spawn(async move { agent.run().await });

    // Build replay entries if resuming
    let replay_entries = if hl_resume_meta.is_some() {
        let replay_log = ConversationLog::open(&hl_log_path)?;
        match replay_log.replay_for_display() {
            Ok((entries, _, _)) => entries
                .iter()
                .map(|e| {
                    use conversation_log::DisplayEntry;
                    match e {
                        DisplayEntry::User(content) => headless::ReplayEntryJson {
                            kind: "user".into(),
                            content: content.clone(),
                            tool_name: None,
                            success: None,
                        },
                        DisplayEntry::Assistant(content) => headless::ReplayEntryJson {
                            kind: "assistant".into(),
                            content: content.clone(),
                            tool_name: None,
                            success: None,
                        },
                        DisplayEntry::ToolCall(name) => headless::ReplayEntryJson {
                            kind: "tool_call".into(),
                            content: name.clone(),
                            tool_name: Some(name.clone()),
                            success: None,
                        },
                        DisplayEntry::ToolResult {
                            tool_name,
                            output,
                            success,
                        } => headless::ReplayEntryJson {
                            kind: "tool_result".into(),
                            content: output.clone(),
                            tool_name: Some(tool_name.clone()),
                            success: Some(*success),
                        },
                    }
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };
    // Kept inside the frame cap — see `fit_replay_entries`. Without this a long
    // session's transcript was written uncapped and the client refused it, so
    // the resume came back empty.
    let replay_entries =
        headless::fit_replay_entries(replay_entries, headless::REPLAY_BUDGET_BYTES);

    let rewind_checkpoints = if hl_resume_meta.is_some() {
        let replay_log = ConversationLog::open(&hl_log_path)?;
        match replay_log.rewind_checkpoints_for_resume() {
            Ok(checkpoints) => checkpoints
                .into_iter()
                .map(|item| headless::RewindCheckpointJson {
                    id: item.checkpoint.id,
                    preview: item.checkpoint.preview,
                    message_count: item.checkpoint.message_count,
                    display_index: item.display_index,
                    keep_on_restore: item.checkpoint.keep_on_restore,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    let init = headless::HeadlessInit {
        project_root: workspace_root.clone(),
        model_name: endpoint.name.clone(),
        model_id: model_id.clone(),
        max_context_tokens,
        log_path: hl_log_path.to_string_lossy().to_string(),
        dangerously_allow_all: cli.dangerously_allow_all,
        agent_definitions: headless::HeadlessInit::make_agent_def_info(&agent_defs_for_state),
        endpoints: all_endpoints.iter().map(|ep| ep.into()).collect(),
        session_id: Some(hl_session_id),
        resume_meta: hl_resume_meta,
        replay_entries,
        rewind_checkpoints,
        available_tools,
        context_strategy: match app_config.agent.context_strategy {
            crate::config::ContextStrategy::RollingWindow => "rolling_window".to_string(),
            crate::config::ContextStrategy::Compaction => "compaction".to_string(),
        },
        chatgpt_logged_in,
        offline_mode: app_config.agent.offline_mode,
    };
    headless::run_headless(event_rx, action_tx, init, app_config.clone()).await?;
    let _ = agent_handle.await;
    Ok(())
}
