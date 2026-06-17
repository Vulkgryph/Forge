// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::Command;

/// Best-effort launch of the user's default browser at `url`.
/// Errors are ignored: every OAuth caller also prints the URL to stderr
/// so the user can copy-paste manually if the auto-open fails.
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();

    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", "", url])
        .spawn();

    // Linux, BSD, illumos, etc. — xdg-open is the freedesktop standard.
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

// Claude Code OAuth client — same as the official CLI
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";
const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const REDIRECT_URI: &str = "http://localhost:53692/callback";
const SCOPES: &str = "org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

// ChatGPT/Codex OAuth client id used by the public Codex CLI.
const CHATGPT_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CHATGPT_ISSUER: &str = "https://auth.openai.com";
const CHATGPT_REDIRECT_PORT: u16 = 1455;
const CHATGPT_REDIRECT_PATH: &str = "/auth/callback";
const CHATGPT_SCOPES: &str =
    "openid profile email offline_access api.connectors.read api.connectors.invoke";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64, // Unix timestamp (seconds)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatGptTokens {
    pub id_token: String,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub api_key: Option<String>,
    pub account_id: Option<String>,
    pub plan_type: Option<String>,
    pub expires_at: Option<u64>,
}

/// A model discovered from the ChatGPT Codex subscription model cache.
pub struct ChatGptCodexModel {
    pub id: String,
    pub display_name: String,
    pub context_window: usize,
    pub max_output_tokens: u32,
}

impl ChatGptTokens {
    fn is_expired(&self) -> bool {
        let Some(expires_at) = self.expires_at else {
            return false;
        };
        let now = unix_now();
        now >= expires_at.saturating_sub(60)
    }
}

impl OAuthTokens {
    fn is_expired(&self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // Refresh 60 seconds before actual expiry
        now >= self.expires_at.saturating_sub(60)
    }
}

pub fn auth_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home.join(".config").join("forge").join("auth.json"))
}

pub fn chatgpt_auth_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Could not find home directory")?;
    Ok(home
        .join(".config")
        .join("forge")
        .join("chatgpt_auth.json"))
}

pub fn load_tokens() -> Option<OAuthTokens> {
    let path = auth_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn load_chatgpt_tokens() -> Option<ChatGptTokens> {
    let path = chatgpt_auth_path().ok()?;
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

pub async fn fetch_chatgpt_codex_models() -> Vec<ChatGptCodexModel> {
    if let Some(models) = fetch_chatgpt_codex_models_from_cli().await {
        return models;
    }
    fetch_chatgpt_codex_models_from_cache()
}

async fn fetch_chatgpt_codex_models_from_cli() -> Option<Vec<ChatGptCodexModel>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        Command::new("codex").args(["debug", "models"]).output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_chatgpt_codex_models(&output.stdout)
}

fn fetch_chatgpt_codex_models_from_cache() -> Vec<ChatGptCodexModel> {
    let Some(home) = dirs::home_dir() else {
        return vec![];
    };
    let path = home.join(".codex").join("models_cache.json");
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(_) => return vec![],
    };
    parse_chatgpt_codex_models(content.as_bytes()).unwrap_or_default()
}

fn parse_chatgpt_codex_models(json_bytes: &[u8]) -> Option<Vec<ChatGptCodexModel>> {
    let json: serde_json::Value = match serde_json::from_slice(json_bytes) {
        Ok(json) => json,
        Err(_) => return None,
    };
    let models = match json.get("models").and_then(|v| v.as_array()) {
        Some(models) => models,
        None => return None,
    };

    let mut discovered = Vec::new();
    for model in models {
        if model
            .get("visibility")
            .and_then(|v| v.as_str())
            .is_some_and(|visibility| visibility == "hide")
        {
            continue;
        }
        let Some(id) = model.get("slug").and_then(|v| v.as_str()) else {
            continue;
        };
        let display_name = model
            .get("display_name")
            .and_then(|v| v.as_str())
            .unwrap_or(id)
            .to_string();
        let normal_context = model
            .get("context_window")
            .and_then(|v| v.as_u64())
            .unwrap_or(272_000);
        let max_context = model
            .get("max_context_window")
            .and_then(|v| v.as_u64())
            .unwrap_or(normal_context);
        let effective_percent = model
            .get("effective_context_window_percent")
            .and_then(|v| v.as_u64())
            .unwrap_or(100)
            .clamp(1, 100);
        let context_window = ((normal_context * effective_percent) / 100) as usize;
        let max_context_window = ((max_context * effective_percent) / 100) as usize;
        let max_output_tokens = model
            .get("max_output_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(16_384) as u32;

        if !discovered.iter().any(|existing: &ChatGptCodexModel| {
            existing.id == id && existing.context_window == context_window
        }) {
            discovered.push(ChatGptCodexModel {
                id: id.to_string(),
                display_name: display_name.clone(),
                context_window,
                max_output_tokens,
            });
        }
        if max_context_window > context_window
            && !discovered.iter().any(|existing: &ChatGptCodexModel| {
                existing.id == id && existing.context_window == max_context_window
            })
        {
            discovered.push(ChatGptCodexModel {
                id: id.to_string(),
                display_name: format!("{} (max context)", display_name),
                context_window: max_context_window,
                max_output_tokens,
            });
        }
    }

    Some(discovered)
}

fn save_tokens(tokens: &OAuthTokens) -> Result<()> {
    let path = auth_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(tokens)?;
    std::fs::write(&path, &content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn save_chatgpt_tokens(tokens: &ChatGptTokens) -> Result<()> {
    let path = chatgpt_auth_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(tokens)?;
    std::fs::write(&path, &content)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn generate_pkce() -> (String, String) {
    let mut verifier_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let hash = hasher.finalize();
    let challenge = URL_SAFE_NO_PAD.encode(hash);

    (verifier, challenge)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Get a valid access token, refreshing if expired. Returns error if not logged in.
pub async fn get_valid_token(http: &reqwest::Client) -> Result<String> {
    let mut tokens =
        load_tokens().context("Not logged in to Claude. Run `forge-agent --login` first.")?;

    if tokens.is_expired() {
        tokens = refresh_tokens(http, &tokens).await?;
        save_tokens(&tokens)?;
    }

    Ok(tokens.access_token.clone())
}

pub async fn get_valid_chatgpt_token(http: &reqwest::Client) -> Result<ChatGptTokens> {
    let mut tokens = load_chatgpt_tokens()
        .context("Not logged in to ChatGPT Codex. Run `/login --chatgpt` first.")?;

    if tokens.is_expired() {
        tokens = refresh_chatgpt_tokens(http, &tokens).await?;
        save_chatgpt_tokens(&tokens)?;
    }
    if tokens.api_key.is_none() {
        if let Ok(api_key) = obtain_chatgpt_api_key(http, &tokens.id_token).await {
            tokens.api_key = Some(api_key);
        }
        save_chatgpt_tokens(&tokens)?;
    }

    Ok(tokens)
}

pub async fn get_valid_chatgpt_token_force_refresh(
    http: &reqwest::Client,
) -> Result<ChatGptTokens> {
    let tokens = load_chatgpt_tokens()
        .context("Not logged in to ChatGPT Codex. Run `/login --chatgpt` first.")?;
    let refreshed = refresh_chatgpt_tokens(http, &tokens).await?;
    save_chatgpt_tokens(&refreshed)?;
    Ok(refreshed)
}

/// Force-refresh the token regardless of local expiry (used when server returns 401).
pub async fn get_valid_token_force_refresh(http: &reqwest::Client) -> Result<String> {
    let tokens =
        load_tokens().context("Not logged in to Claude. Run `forge-agent --login` first.")?;
    let refreshed = refresh_tokens(http, &tokens).await?;
    save_tokens(&refreshed)?;
    Ok(refreshed.access_token)
}

async fn refresh_tokens(http: &reqwest::Client, old_tokens: &OAuthTokens) -> Result<OAuthTokens> {
    let resp = http
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "refresh_token",
            "client_id": CLIENT_ID,
            "refresh_token": old_tokens.refresh_token,
        }))
        .send()
        .await
        .context("Token refresh request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token refresh failed ({}): {}", status, body);
    }

    parse_token_response(resp, &old_tokens.refresh_token).await
}

async fn refresh_chatgpt_tokens(
    http: &reqwest::Client,
    old_tokens: &ChatGptTokens,
) -> Result<ChatGptTokens> {
    let resp = http
        .post(format!("{}/oauth/token", CHATGPT_ISSUER))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=refresh_token&client_id={}&refresh_token={}",
            urlencoding::encode(CHATGPT_CLIENT_ID),
            urlencoding::encode(&old_tokens.refresh_token),
        ))
        .send()
        .await
        .context("ChatGPT token refresh request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("ChatGPT token refresh failed ({}): {}", status, body);
    }

    let mut tokens = parse_chatgpt_token_response(resp, Some(old_tokens)).await?;
    tokens.api_key = obtain_chatgpt_api_key(http, &tokens.id_token).await.ok();
    Ok(tokens)
}

async fn obtain_chatgpt_api_key(http: &reqwest::Client, id_token: &str) -> Result<String> {
    let resp = http
        .post(format!("{}/oauth/token", CHATGPT_ISSUER))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type={}&client_id={}&requested_token={}&subject_token={}&subject_token_type={}",
            urlencoding::encode("urn:ietf:params:oauth:grant-type:token-exchange"),
            urlencoding::encode(CHATGPT_CLIENT_ID),
            urlencoding::encode("openai-api-key"),
            urlencoding::encode(id_token),
            urlencoding::encode("urn:ietf:params:oauth:token-type:id_token"),
        ))
        .send()
        .await
        .context("ChatGPT API token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("ChatGPT API token exchange failed ({}): {}", status, body);
    }

    let json: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse ChatGPT API token exchange response")?;
    json["access_token"]
        .as_str()
        .map(|s| s.to_string())
        .context("Missing access_token in ChatGPT API token exchange response")
}

async fn parse_token_response(
    resp: reqwest::Response,
    fallback_refresh_token: &str,
) -> Result<OAuthTokens> {
    let json: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse token response")?;

    let access_token = json["access_token"]
        .as_str()
        .context("Missing access_token in response")?
        .to_string();
    // Some OAuth servers don't rotate the refresh token — keep the old one if absent.
    let refresh_token = json["refresh_token"]
        .as_str()
        .unwrap_or(fallback_refresh_token)
        .to_string();
    let expires_in = json["expires_in"].as_u64().unwrap_or(3600);

    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + expires_in;

    Ok(OAuthTokens {
        access_token,
        refresh_token,
        expires_at,
    })
}

async fn parse_chatgpt_token_response(
    resp: reqwest::Response,
    fallback: Option<&ChatGptTokens>,
) -> Result<ChatGptTokens> {
    let json: serde_json::Value = resp
        .json()
        .await
        .context("Failed to parse ChatGPT token response")?;

    let id_token = json["id_token"]
        .as_str()
        .or_else(|| fallback.map(|t| t.id_token.as_str()))
        .context("Missing id_token in ChatGPT token response")?
        .to_string();
    let access_token = json["access_token"]
        .as_str()
        .context("Missing access_token in ChatGPT token response")?
        .to_string();
    let refresh_token = json["refresh_token"]
        .as_str()
        .or_else(|| fallback.map(|t| t.refresh_token.as_str()))
        .context("Missing refresh_token in ChatGPT token response")?
        .to_string();

    let claims = jwt_auth_claims(&id_token);
    let account_id = claims
        .get("chatgpt_account_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| fallback.and_then(|t| t.account_id.clone()));
    let plan_type = claims
        .get("chatgpt_plan_type")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .or_else(|| fallback.and_then(|t| t.plan_type.clone()));
    let expires_at = jwt_exp(&access_token).or_else(|| jwt_exp(&id_token));

    Ok(ChatGptTokens {
        id_token,
        access_token,
        refresh_token,
        api_key: fallback.and_then(|t| t.api_key.clone()),
        account_id,
        plan_type,
        expires_at,
    })
}

fn jwt_payload(jwt: &str) -> Option<serde_json::Value> {
    let mut parts = jwt.split('.');
    let _header = parts.next()?;
    let payload = parts.next()?;
    let _sig = parts.next()?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_exp(jwt: &str) -> Option<u64> {
    jwt_payload(jwt)?.get("exp")?.as_u64()
}

fn jwt_auth_claims(jwt: &str) -> serde_json::Map<String, serde_json::Value> {
    jwt_payload(jwt)
        .and_then(|mut payload| {
            payload
                .get_mut("https://api.openai.com/auth")
                .and_then(|v| v.as_object_mut())
                .cloned()
        })
        .unwrap_or_default()
}

/// A Claude model discovered from the Anthropic models API.
pub struct AnthropicModel {
    pub id: String,
    pub display_name: String,
    pub context_window: usize,
    pub max_output_tokens: u32,
}

/// Fetch available models from the Anthropic API using the given OAuth/API token.
/// Returns an empty vec on any error (non-fatal — caller falls back to configured endpoints).
pub async fn fetch_anthropic_models(http: &reqwest::Client, token: &str) -> Vec<AnthropicModel> {
    let is_oauth = token.contains("sk-ant-oat");
    let mut models = Vec::new();
    let mut after_id: Option<String> = None;

    for _ in 0..20 {
        let mut req = http
            .get("https://api.anthropic.com/v1/models")
            .query(&[("limit", "100")])
            .header("anthropic-version", "2023-06-01");

        if let Some(cursor) = after_id.as_deref() {
            req = req.query(&[("after_id", cursor)]);
        }

        if is_oauth {
            req = req
                .header("authorization", format!("Bearer {}", token))
                .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
                .header("user-agent", "claude-cli/2.1.75")
                .header("x-app", "cli")
                .header("anthropic-dangerous-direct-browser-access", "true");
        } else {
            req = req.header("x-api-key", token);
        }

        let resp = match tokio::time::timeout(std::time::Duration::from_secs(8), req.send()).await {
            Ok(Ok(r)) => r,
            _ => break,
        };

        if !resp.status().is_success() {
            break;
        }

        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => break,
        };

        let data = match json.get("data").and_then(|d| d.as_array()) {
            Some(d) => d,
            None => break,
        };

        for m in data {
            if let Some(model) = parse_anthropic_model(m) {
                if !models
                    .iter()
                    .any(|existing: &AnthropicModel| existing.id == model.id)
                {
                    if model.context_window > 200_000 {
                        models.push(AnthropicModel {
                            id: model.id.clone(),
                            display_name: format!("{} (200k)", model.display_name),
                            context_window: 200_000,
                            max_output_tokens: model.max_output_tokens,
                        });
                    }
                    models.push(model);
                }
            }
        }

        let has_more = json
            .get("has_more")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let next_after_id = json
            .get("last_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);

        if !has_more || next_after_id.is_none() || next_after_id == after_id {
            break;
        }
        after_id = next_after_id;
    }

    models
}

fn parse_anthropic_model(m: &serde_json::Value) -> Option<AnthropicModel> {
    let id = m.get("id")?.as_str()?.to_string();
    let display_name = m
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or(&id)
        .to_string();
    let context_window = ["max_input_tokens", "context_window", "context_length"]
        .iter()
        .find_map(|field| m.get(field).and_then(|v| v.as_u64()))
        .unwrap_or(200_000) as usize;
    let max_output_tokens = m
        .get("max_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(16384) as u32;
    Some(AnthropicModel {
        id,
        display_name,
        context_window,
        max_output_tokens,
    })
}

/// Run the OAuth login flow interactively. Opens browser, waits for callback, saves tokens.
pub async fn login() -> Result<()> {
    let (verifier, challenge) = generate_pkce();

    let auth_url = format!(
        "{}?code=true&client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}",
        AUTHORIZE_URL,
        CLIENT_ID,
        urlencoding::encode(REDIRECT_URI),
        urlencoding::encode(SCOPES),
        challenge,
        verifier,
    );

    eprintln!("Opening browser for Claude authorization...");
    eprintln!("If the browser doesn't open, visit:\n\n  {}\n", auth_url);

    open_browser(&auth_url);

    let code = wait_for_callback().await?;

    let http = reqwest::Client::new();
    let resp = http
        .post(TOKEN_URL)
        .json(&serde_json::json!({
            "grant_type": "authorization_code",
            "client_id": CLIENT_ID,
            "code": code,
            "state": verifier,
            "redirect_uri": REDIRECT_URI,
            "code_verifier": verifier,
        }))
        .send()
        .await
        .context("Token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Token exchange failed ({}): {}", status, body);
    }

    let tokens = parse_token_response(resp, "").await?;
    save_tokens(&tokens)?;

    eprintln!("Login successful. Token saved to ~/.config/forge/auth.json");
    Ok(())
}

/// Run the ChatGPT/Codex OAuth login flow. This mirrors the public Codex CLI
/// browser flow and stores tokens separately from Claude credentials.
pub async fn login_chatgpt() -> Result<()> {
    let (verifier, challenge) = generate_pkce();
    let state = {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        URL_SAFE_NO_PAD.encode(bytes)
    };
    let redirect_uri = format!(
        "http://localhost:{}{}",
        CHATGPT_REDIRECT_PORT, CHATGPT_REDIRECT_PATH
    );

    let auth_url = format!(
        "{}/oauth/authorize?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&id_token_add_organizations=true&codex_cli_simplified_flow=true&state={}&originator=codex_cli_rs",
        CHATGPT_ISSUER,
        urlencoding::encode(CHATGPT_CLIENT_ID),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(CHATGPT_SCOPES),
        urlencoding::encode(&challenge),
        urlencoding::encode(&state),
    );

    eprintln!("Opening browser for ChatGPT Codex authorization...");
    eprintln!("If the browser doesn't open, visit:\n\n  {}\n", auth_url);

    open_browser(&auth_url);

    let code =
        wait_for_callback_on(CHATGPT_REDIRECT_PORT, CHATGPT_REDIRECT_PATH, Some(&state)).await?;

    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{}/oauth/token", CHATGPT_ISSUER))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&code_verifier={}",
            urlencoding::encode(&code),
            urlencoding::encode(&redirect_uri),
            urlencoding::encode(CHATGPT_CLIENT_ID),
            urlencoding::encode(&verifier),
        ))
        .send()
        .await
        .context("ChatGPT token exchange request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if body.contains("missing_codex_entitlement") {
            anyhow::bail!("ChatGPT login succeeded, but this account or workspace does not have Codex enabled.");
        }
        anyhow::bail!("ChatGPT token exchange failed ({}): {}", status, body);
    }

    let mut tokens = parse_chatgpt_token_response(resp, None).await?;
    tokens.api_key = obtain_chatgpt_api_key(&http, &tokens.id_token).await.ok();
    save_chatgpt_tokens(&tokens)?;

    eprintln!("ChatGPT Codex login successful. Token saved to ~/.config/forge/chatgpt_auth.json");
    Ok(())
}

/// Binds port 53692, waits for the OAuth redirect, returns the authorization code.
async fn wait_for_callback() -> Result<String> {
    wait_for_callback_on(53692, "/callback", None).await
}

async fn wait_for_callback_on(
    port: u16,
    expected_path: &str,
    expected_state: Option<&str>,
) -> Result<String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("Failed to bind port {} for OAuth callback", port))?;

    eprintln!(
        "Waiting for browser callback on http://localhost:{}{} ...",
        port, expected_path
    );

    let (mut stream, _) = listener.accept().await?;

    let mut buf = vec![0u8; 8192];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);

    // Parse: "GET /callback?code=XXX&state=YYY HTTP/1.1"
    let request_path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .context("OAuth callback was malformed")?;

    let (path, query) = request_path.split_once('?').unwrap_or((request_path, ""));
    if path != expected_path {
        anyhow::bail!(
            "OAuth callback path mismatch: expected {}, got {}",
            expected_path,
            path
        );
    }

    let params: std::collections::HashMap<String, String> = query
        .split('&')
        .filter_map(|part| {
            let (k, v) = part.split_once('=')?;
            let key = urlencoding::decode(k).ok()?.into_owned();
            let value = urlencoding::decode(v).ok()?.into_owned();
            Some((key, value))
        })
        .collect();

    if let Some(err) = params.get("error") {
        let desc = params
            .get("error_description")
            .map(String::as_str)
            .unwrap_or("");
        anyhow::bail!("OAuth callback error: {} {}", err, desc);
    }

    if let Some(expected) = expected_state {
        let actual = params.get("state").map(String::as_str);
        if actual != Some(expected) {
            anyhow::bail!("OAuth callback state mismatch");
        }
    }

    let code = params
        .get("code")
        .cloned()
        .context("OAuth callback did not contain a code parameter")?;

    let html = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h2>Login successful!</h2><p>You can close this window and return to forge.</p></body></html>";
    let _ = stream.write_all(html.as_bytes()).await;

    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::parse_chatgpt_codex_models;

    #[test]
    fn chatgpt_codex_parser_adds_metadata_max_context_variant() {
        let json = br#"{
            "models": [
                {
                    "slug": "model-a",
                    "display_name": "Model A",
                    "context_window": 1000,
                    "max_context_window": 4000,
                    "effective_context_window_percent": 50,
                    "visibility": "list"
                }
            ]
        }"#;

        let models = parse_chatgpt_codex_models(json).expect("models");

        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "model-a");
        assert_eq!(models[0].display_name, "Model A");
        assert_eq!(models[0].context_window, 500);
        assert_eq!(models[1].id, "model-a");
        assert_eq!(models[1].display_name, "Model A (max context)");
        assert_eq!(models[1].context_window, 2000);
    }

    #[test]
    fn chatgpt_codex_parser_skips_max_variant_when_not_larger() {
        let json = br#"{
            "models": [
                {
                    "slug": "model-b",
                    "display_name": "Model B",
                    "context_window": 2000,
                    "max_context_window": 2000,
                    "effective_context_window_percent": 95,
                    "visibility": "list"
                }
            ]
        }"#;

        let models = parse_chatgpt_codex_models(json).expect("models");

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].display_name, "Model B");
        assert_eq!(models[0].context_window, 1900);
    }
}
