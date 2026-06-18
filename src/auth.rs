// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

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
    /// If we got a 429 from the refresh endpoint, the unix timestamp until
    /// which we should not retry. Cleared on next successful refresh.
    /// Default = 0 (never rate-limited).
    #[serde(default)]
    pub rate_limited_until: u64,
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
    /// Same as OAuthTokens::rate_limited_until.
    #[serde(default)]
    pub rate_limited_until: u64,
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
    // 1) Live backend query using the user's ChatGPT OAuth token. This is
    //    the source of truth — the codex CLI's local cache seeds from this
    //    same endpoint. We don't need the CLI installed; we just need the
    //    `client_version` query parameter the backend requires, and the
    //    same originator/user-agent the CLI sends.
    if let Some(models) = fetch_chatgpt_codex_models_from_backend().await {
        return models;
    }

    // 2) Local Codex CLI cache, if the user happens to have it installed
    //    AND the network call above failed (offline, rate-limited, etc.).
    if let Some(models) = fetch_chatgpt_codex_models_from_cli().await {
        return models;
    }
    let cached = fetch_chatgpt_codex_models_from_cache();
    if !cached.is_empty() {
        return cached;
    }

    // 3) Last-ditch hardcoded fallback. Better than letting "default" reach
    //    the API and getting a 400. The agent's model picker can flip to
    //    something more current if these slugs eventually retire.
    vec![
        ChatGptCodexModel {
            id: "gpt-5-codex".to_string(),
            display_name: "GPT-5 Codex".to_string(),
            context_window: 272_000,
            max_output_tokens: 16_384,
        },
        ChatGptCodexModel {
            id: "gpt-5".to_string(),
            display_name: "GPT-5".to_string(),
            context_window: 272_000,
            max_output_tokens: 16_384,
        },
    ]
}

async fn fetch_chatgpt_codex_models_from_cli() -> Option<Vec<ChatGptCodexModel>> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        tokio::process::Command::new("codex")
            .args(["debug", "models"])
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }

    parse_chatgpt_codex_models(&output.stdout)
}

/// Resolve the `client_version` to send to the Codex backend. OpenAI's
/// model-catalog endpoint requires this query parameter and rejects
/// outdated values. Forge keeps it current automatically — no codex CLI
/// install or manual intervention required.
///
/// Resolution order (first hit wins):
///   1. Forge's own cache at `~/.config/forge/codex_client_version.json`
///      if the cached value is less than 7 days old.
///   2. Live query to `api.github.com/repos/openai/codex/releases/latest`
///      (no auth required, 60 req/hr rate limit is comfortable). Result
///      is written to the cache.
///   3. Stale cache, if the GitHub query failed (offline, rate-limited).
///   4. The codex CLI's own `~/.codex/models_cache.json` if it happens
///      to be present — bonus safety net for users who have the CLI.
///   5. Hardcoded baseline `0.140.0`.
async fn detect_codex_client_version() -> String {
    // Primary path: shared resolver (forge cache → GitHub → stale cache → fallback)
    let v = resolve_client_version("openai/codex", "codex_client_version.json", "0.140.0").await;
    // Bonus: if forge cache is missing AND GitHub failed AND we're falling back,
    // peek at the codex CLI's own cache if the user happens to have it installed.
    // We only bother when v matches the hardcoded fallback, since otherwise
    // we already have something fresher.
    if v == "0.140.0" {
        if let Some(home) = dirs::home_dir() {
            let path = home.join(".codex").join("models_cache.json");
            if let Ok(content) = std::fs::read_to_string(path) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(s) = json.get("client_version").and_then(|v| v.as_str()) {
                        return s.to_string();
                    }
                }
            }
        }
    }
    v
}

#[derive(Serialize, Deserialize)]
struct ClientVersionCache {
    version: String,
    fetched_at: u64,
}

impl ClientVersionCache {
    fn is_stale(&self, max_age_secs: u64) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        now.saturating_sub(self.fetched_at) > max_age_secs
    }
}

fn client_version_cache_path(filename: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".config").join("forge").join(filename))
}

fn read_version_cache(filename: &str) -> Option<ClientVersionCache> {
    let path = client_version_cache_path(filename)?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn write_version_cache(filename: &str, version: &str) -> std::io::Result<()> {
    let path = client_version_cache_path(filename)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "no home dir"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let fetched_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cache = ClientVersionCache {
        version: version.to_string(),
        fetched_at,
    };
    let content = serde_json::to_string_pretty(&cache)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(&path, content)
}

/// Query GitHub's "latest release" endpoint for a public repo. Returns the
/// version string (stripped of `rust-` and leading `v` prefixes) or None on
/// failure.
async fn fetch_latest_github_release(repo: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    let resp = client
        .get(&url)
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "forge-agent")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: serde_json::Value = resp.json().await.ok()?;
    // `name` is usually cleaned ("0.141.0"); `tag_name` may be prefixed
    // ("rust-v0.141.0" / "v2.1.181"). Prefer the cleaner of the two.
    let clean = |s: &str| -> String {
        s.trim()
            .trim_start_matches("rust-")
            .trim_start_matches('v')
            .to_string()
    };
    if let Some(name) = body.get("name").and_then(|v| v.as_str()) {
        let c = clean(name);
        if !c.is_empty() && c.chars().next().map_or(false, |c| c.is_ascii_digit()) {
            return Some(c);
        }
    }
    if let Some(tag) = body.get("tag_name").and_then(|v| v.as_str()) {
        let c = clean(tag);
        if !c.is_empty() {
            return Some(c);
        }
    }
    None
}

/// Generic "resolve current client version" helper for any GitHub-published
/// CLI we're impersonating. Used by both Codex and Claude detection.
async fn resolve_client_version(repo: &str, cache_file: &str, fallback: &str) -> String {
    const CACHE_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;

    if let Some(cached) = read_version_cache(cache_file) {
        if !cached.is_stale(CACHE_MAX_AGE_SECS) {
            return cached.version;
        }
    }
    if let Some(latest) = fetch_latest_github_release(repo).await {
        let _ = write_version_cache(cache_file, &latest);
        return latest;
    }
    if let Some(cached) = read_version_cache(cache_file) {
        return cached.version;
    }
    fallback.to_string()
}

/// Resolve the Claude Code client version Forge should pose as. Anthropic's
/// API rejects (or rate-limits) requests with very old user-agents; tracking
/// the upstream CLI version keeps us looking current.
pub async fn claude_client_version() -> String {
    use tokio::sync::OnceCell;
    static CACHED: OnceCell<String> = OnceCell::const_new();
    CACHED
        .get_or_init(|| async {
            resolve_client_version("anthropics/claude-code", "claude_client_version.json", "2.1.75")
                .await
        })
        .await
        .clone()
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

/// Query the ChatGPT Codex backend's model catalog directly using the user's
/// OAuth tokens. The official codex CLI seeds its local cache from this same
/// endpoint, but Forge users typically don't have that CLI installed.
///
/// We pose as codex CLI in the request headers — same originator id the
/// official tool uses, matching User-Agent — so the backend treats us
/// identically. Tokens are obtained via the same OAuth flow with
/// `originator=codex_cli_rs` already baked into the auth URL.
async fn fetch_chatgpt_codex_models_from_backend() -> Option<Vec<ChatGptCodexModel>> {
    let tokens = load_chatgpt_tokens()?;
    let bearer = tokens.api_key.as_deref().or(Some(tokens.access_token.as_str()))?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .build()
        .ok()?;

    // The backend requires a `client_version` query parameter; missing it
    // returns 400 ("Field required: query.client_version"). OpenAI accepts
    // older versions for a long time but eventually drops support — so
    // we want this value to stay reasonably current.
    //
    // Strategy: if the user happens to have the official codex CLI
    // installed, read the version it last cached. That CLI auto-updates,
    // so forge auto-picks-up the latest accepted value for free. If
    // they don't have the CLI, fall back to a baseline pinned here that
    // we bump in releases as needed.
    let codex_client_version = detect_codex_client_version().await;

    // Try the most likely endpoints. /codex/models is the documented one; the
    // bare /models exists too but historically requires different params.
    // Take the first that returns a parseable list with at least one model.
    let urls = [
        format!(
            "https://chatgpt.com/backend-api/codex/models?client_version={}",
            codex_client_version
        ),
        format!(
            "https://chatgpt.com/backend-api/models?client_version={}",
            codex_client_version
        ),
    ];

    for url in urls {
        let Ok(resp) = http
            .get(&url)
            .bearer_auth(bearer)
            .header("accept", "application/json")
            .header("user-agent", format!("codex_cli_rs/{}", codex_client_version))
            .header("originator", "codex_cli_rs")
            .send()
            .await
        else {
            continue;
        };

        if !resp.status().is_success() {
            continue;
        }
        let Ok(body) = resp.bytes().await else { continue };
        if let Some(models) = parse_chatgpt_codex_models(&body) {
            if !models.is_empty() {
                return Some(models);
            }
        }
    }

    None
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

    // Known internal-use models that the Codex backend lists but that aren't
    // meant for general chat. `codex-auto-review` is the code-review sub-model
    // the codex CLI uses internally during reviews — it's not callable as a
    // regular chat target.
    const INTERNAL_SLUGS: &[&str] = &["codex-auto-review"];

    let mut discovered = Vec::new();
    for model in models {
        // ChatGPT marks some models as visibility: "hide" — these are usually
        // deprecated, internal, or otherwise hidden from the default ChatGPT
        // web UI, but they remain callable via the Codex API for users who
        // know the slug. Surface them in Forge's picker too EXCEPT for slugs
        // we know are internal sub-models (see INTERNAL_SLUGS above).
        let _ = model.get("visibility");
        let Some(id) = model.get("slug").and_then(|v| v.as_str()) else {
            continue;
        };
        if INTERNAL_SLUGS.contains(&id) {
            continue;
        }
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

/// Default backoff applied to OAuth refresh attempts that hit 429 without a
/// `Retry-After` header. Anthropic's rate-limit window for this endpoint is
/// roughly 5 minutes in practice.
const DEFAULT_REFRESH_BACKOFF_SECS: u64 = 300;

/// Parse the `Retry-After` header (RFC 7231: seconds-as-integer OR HTTP-date)
/// into a number of seconds from now. Returns None if absent or unparseable;
/// the caller can apply a default backoff.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    // Numeric form: "300"
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(secs);
    }
    // HTTP-date form (e.g. "Wed, 21 Oct 2024 07:28:00 GMT") — best-effort,
    // not bothering with a full parser; just fall back to default.
    None
}

async fn refresh_tokens(http: &reqwest::Client, old_tokens: &OAuthTokens) -> Result<OAuthTokens> {
    // Short-circuit if we're still inside a previous 429 backoff window.
    let now = unix_now();
    if old_tokens.rate_limited_until > now {
        let secs = old_tokens.rate_limited_until - now;
        anyhow::bail!(
            "Refresh is rate-limited by the auth server. Wait ~{}s, or run forge --login to get fresh tokens.",
            secs
        );
    }

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
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();

        if status.as_u16() == 429 {
            // Persist the cooldown so subsequent forge startups don't hammer
            // the endpoint and dig the rate-limit hole deeper.
            let retry_after = parse_retry_after(&headers).unwrap_or(DEFAULT_REFRESH_BACKOFF_SECS);
            let until = now + retry_after;
            let mut t = old_tokens.clone();
            t.rate_limited_until = until;
            let _ = save_tokens(&t);
            anyhow::bail!(
                "Refresh rate-limited by the auth server (HTTP 429). Cooldown ~{}s. Wait, or run forge --login to get fresh tokens.",
                retry_after
            );
        }
        anyhow::bail!(parse_oauth_error_body(status.as_u16(), &body));
    }

    parse_token_response(resp, &old_tokens.refresh_token).await
}

async fn refresh_chatgpt_tokens(
    http: &reqwest::Client,
    old_tokens: &ChatGptTokens,
) -> Result<ChatGptTokens> {
    let now = unix_now();
    if old_tokens.rate_limited_until > now {
        let secs = old_tokens.rate_limited_until - now;
        anyhow::bail!(
            "Refresh is rate-limited by the auth server. Wait ~{}s, or run forge --login-chatgpt to get fresh tokens.",
            secs
        );
    }

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
        let headers = resp.headers().clone();
        let body = resp.text().await.unwrap_or_default();

        if status.as_u16() == 429 {
            let retry_after = parse_retry_after(&headers).unwrap_or(DEFAULT_REFRESH_BACKOFF_SECS);
            let until = now + retry_after;
            let mut t = old_tokens.clone();
            t.rate_limited_until = until;
            let _ = save_chatgpt_tokens(&t);
            anyhow::bail!(
                "Refresh rate-limited by the auth server (HTTP 429). Cooldown ~{}s. Wait, or run forge --login-chatgpt to get fresh tokens.",
                retry_after
            );
        }
        anyhow::bail!(parse_oauth_error_body(status.as_u16(), &body));
    }

    let mut tokens = parse_chatgpt_token_response(resp, Some(old_tokens)).await?;
    tokens.api_key = obtain_chatgpt_api_key(http, &tokens.id_token).await.ok();
    Ok(tokens)
}

/// Convert an OAuth error response body into a short, human-readable string.
/// Handles both the OpenAI-style `{"error": {"code": ..., "message": ...}}`
/// shape and the RFC 6749 flat `{"error": "...", "error_description": "..."}`
/// shape. Falls back to the raw body if it isn't recognizable JSON.
fn parse_oauth_error_body(status: u16, body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        // OpenAI nested shape
        if let Some(err) = value.get("error").and_then(|e| e.as_object()) {
            let code = err.get("code").and_then(|v| v.as_str());
            let message = err.get("message").and_then(|v| v.as_str());
            if let (Some(c), Some(m)) = (code, message) {
                return format!("{} (code: {})", m, c);
            }
            if let Some(m) = message {
                return m.to_string();
            }
        }
        // RFC 6749 flat shape
        if let (Some(err), desc) = (
            value.get("error").and_then(|v| v.as_str()),
            value.get("error_description").and_then(|v| v.as_str()),
        ) {
            return match desc {
                Some(d) => format!("{} (code: {})", d, err),
                None => err.to_string(),
            };
        }
    }
    format!("HTTP {}: {}", status, body.trim())
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
        // Fresh tokens → clear any prior rate-limit cooldown.
        rate_limited_until: 0,
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
        // Fresh tokens → clear any prior rate-limit cooldown.
        rate_limited_until: 0,
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
            let ua = format!("claude-cli/{}", claude_client_version().await);
            req = req
                .header("authorization", format!("Bearer {}", token))
                .header("anthropic-beta", "claude-code-20250219,oauth-2025-04-20")
                .header("user-agent", ua)
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

/// Run the OAuth login flow. Opens browser, waits for callback, saves tokens.
///
/// `interactive` controls the paste-the-code fallback: when true (called from
/// `forge-agent --login` standalone), if the localhost port is busy we drop into
/// stdin-paste mode. When false (called from the headless protocol session where
/// stdin is already owned by the JSON message reader), we bail with a clear
/// error pointing at the standalone command — competing for stdin would hang
/// silently otherwise.
pub async fn login(interactive: bool) -> Result<()> {
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

    let code = wait_for_callback(interactive).await?;

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
pub async fn login_chatgpt(interactive: bool) -> Result<()> {
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
        wait_for_callback_on(CHATGPT_REDIRECT_PORT, CHATGPT_REDIRECT_PATH, Some(&state), interactive).await?;

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
async fn wait_for_callback(interactive: bool) -> Result<String> {
    wait_for_callback_on(53692, "/callback", None, interactive).await
}

/// Wait for an OAuth authorization code via two parallel paths, whichever wins first:
///   1. The browser hits the localhost TCP listener (works on local machines and
///      when SSH port forwarding is set up).
///   2. The user pastes the code (or full callback URL) into stdin. This is the
///      fallback for remote machines without port forwarding, firewall-blocked
///      ports, airgapped hosts, OR when something else on the machine is already
///      using the OAuth callback port.
///
/// If the TCP bind fails (port already in use), we still proceed in paste-only
/// mode rather than aborting — the user can complete login by copying the
/// callback URL from their browser's "site can't be reached" page.
async fn wait_for_callback_on(
    port: u16,
    expected_path: &str,
    expected_state: Option<&str>,
    interactive: bool,
) -> Result<String> {
    let bind_result = TcpListener::bind(("127.0.0.1", port)).await;

    match bind_result {
        Ok(listener) => {
            eprintln!(
                "Waiting for browser callback on http://localhost:{}{} ...",
                port, expected_path
            );
            eprintln!();
            eprintln!("---");
            eprintln!("If the redirect can't reach this machine (remote SSH without port forwarding,");
            eprintln!("firewall, etc.), use the manual paste flow:");
            eprintln!();
            eprintln!("  1. Paste the URL ABOVE into your browser and approve the login.");
            eprintln!("  2. After approving, your browser will try to load");
            eprintln!("       http://localhost:{}{}?code=...&state=...", port, expected_path);
            eprintln!("     and show \"site can't be reached\". THAT is the page you want.");
            eprintln!("  3. Copy the URL from your browser's address bar (the localhost one,");
            eprintln!("     NOT the auth.openai.com / claude.ai one) and paste it below.");
            eprintln!();
            eprintln!("Waiting for browser callback OR pasted URL:");
            eprintln!();

            let expected_state_owned = expected_state.map(str::to_string);
            let expected_path_owned = expected_path.to_string();

            tokio::select! {
                result = accept_oauth_callback(listener, expected_path_owned, expected_state_owned.clone()) => result,
                result = read_code_from_stdin(expected_state_owned) => result,
            }
        }
        Err(e) => {
            // Port is in use. The handling forks based on whether we have
            // exclusive access to stdin (`interactive=true`, the standalone
            // forge-agent --login path) or whether stdin is owned by the
            // headless JSON protocol reader (`interactive=false`, the
            // /login-in-TUI path).
            let holder = identify_port_holder(port);
            match holder {
                Some(ref h) => {
                    eprintln!();
                    eprintln!("Port {} is already in use ({}: {}).", port, e, h);
                    if let Some(suggestion) = suggestion_for_holder(h) {
                        eprintln!("  {}", suggestion);
                    }
                }
                None => {
                    eprintln!();
                    eprintln!("Port {} is already in use ({}).", port, e);
                    eprintln!("  To find the process holding it:");
                    if cfg!(target_os = "windows") {
                        eprintln!("    Get-NetTCPConnection -LocalPort {} -State Listen", port);
                    } else {
                        eprintln!("    lsof -nP -iTCP:{} -sTCP:LISTEN", port);
                    }
                }
            }
            eprintln!();

            if !interactive {
                // Inside the headless protocol session — stdin is owned by the
                // JSON message reader, so the paste fallback would hang
                // competing for input. Bail with a clear error instead.
                let cmd = match port {
                    53692 => "forge --login",
                    1455 => "forge --login-chatgpt",
                    _ => "forge --login",
                };
                anyhow::bail!(
                    "Port {} is busy and the manual paste flow isn't available from inside forge. \
                     Quit forge (/quit) and run `{}` from your shell — that path supports paste-the-URL \
                     and works even when the callback port is occupied.",
                    port,
                    cmd
                );
            }

            eprintln!("The automatic browser callback can't be received here, but you can");
            eprintln!("still complete login manually:");
            eprintln!();
            eprintln!("  1. Paste the URL ABOVE into your browser and approve the login.");
            eprintln!("  2. After approving, your browser will try to load");
            eprintln!("       http://localhost:{}{}?code=...&state=...", port, expected_path);
            eprintln!("     and show \"site can't be reached\". That's expected.");
            eprintln!("  3. Copy the URL from your browser's address bar and paste it below.");
            eprintln!();

            let expected_state_owned = expected_state.map(str::to_string);
            read_code_from_stdin(expected_state_owned).await
        }
    }
}

/// Best-effort probe of what process is holding a TCP listener port. Returns
/// a short human-readable string like "PID 12345 (Code Helper)" or None if
/// the probe failed (lsof / netstat unavailable, etc.).
fn identify_port_holder(port: u16) -> Option<String> {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("lsof")
            .args([
                "-nP",
                &format!("-iTCP:{}", port),
                "-sTCP:LISTEN",
                "-F",
                "pcn",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let mut pid: Option<String> = None;
        let mut command: Option<String> = None;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix('p') {
                pid = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix('c') {
                command = Some(rest.to_string());
            }
        }
        let pid = pid?;
        // lsof escapes spaces in process names as \x20 — undo for readability.
        let cmd = command.map(|c| c.replace("\\x20", " "));
        Some(match cmd {
            Some(c) => format!("PID {} ({})", pid, c),
            None => format!("PID {}", pid),
        })
    }
    #[cfg(windows)]
    {
        // PowerShell one-liner: find the listener, look up the process name.
        let cmd = format!(
            "$c = Get-NetTCPConnection -LocalPort {} -State Listen -ErrorAction SilentlyContinue | Select-Object -First 1; if ($c) {{ $p = Get-Process -Id $c.OwningProcess -ErrorAction SilentlyContinue; if ($p) {{ \"PID $($p.Id) ($($p.ProcessName))\" }} else {{ \"PID $($c.OwningProcess)\" }} }}",
            port
        );
        let output = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", &cmd])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() { None } else { Some(text) }
    }
}

/// If we recognize the conflicting process, suggest a specific resolution.
fn suggestion_for_holder(holder: &str) -> Option<&'static str> {
    let lower = holder.to_lowercase();
    if lower.contains("claude") {
        Some("Looks like Claude Code is also running. Quit it for a one-click login, or use the manual flow below.")
    } else if lower.contains("codex") {
        Some("Looks like the codex CLI is also running. Quit it for a one-click login, or use the manual flow below.")
    } else if lower.contains("code helper") || lower.contains("code\\x20h") {
        Some("VS Code is holding this port for one of its internal helpers. Quit VS Code for a one-click login, or just use the manual flow below — it's only a few extra clicks.")
    } else {
        None
    }
}

async fn accept_oauth_callback(
    listener: TcpListener,
    expected_path: String,
    expected_state: Option<String>,
) -> Result<String> {
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

    let code = extract_code_from_query(query, expected_state.as_deref())?;

    let html = "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h2>Login successful!</h2><p>You can close this window and return to forge.</p></body></html>";
    let _ = stream.write_all(html.as_bytes()).await;

    Ok(code)
}

/// Read either a bare authorization code or a full callback URL from stdin
/// and extract the code parameter from it.
async fn read_code_from_stdin(expected_state: Option<String>) -> Result<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();

    // Read lines until we get something usable. Empty lines (just Enter) are
    // ignored so the user can correct stray newlines without aborting.
    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .context("Failed to read paste from stdin")?;
        if n == 0 {
            // EOF — stdin was closed without input. Let the TCP listener handle it.
            std::future::pending::<()>().await;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // If it looks like a full URL, pull the code out of the query string.
        if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
            let query = trimmed.split_once('?').map(|(_, q)| q).unwrap_or("");

            // Detect a common mistake: pasting the authorize URL (the one we
            // told the user to OPEN in their browser) instead of the callback
            // URL their browser ended up on after approving. The authorize URL
            // contains client_id and scope; the callback URL contains code.
            let is_authorize_url = query.contains("client_id=")
                || query.contains("response_type=")
                || trimmed.contains("/oauth/authorize")
                || trimmed.contains("claude.ai/oauth")
                || trimmed.contains("auth.openai.com");
            let has_code = query.contains("code=");

            if is_authorize_url && !has_code {
                anyhow::bail!(
                    "That looks like the authorize URL (the one you were supposed to OPEN \
                     in your browser), not the callback URL.\n\n  \
                     After approving in your browser, your address bar will switch to \
                     something starting with `http://localhost:...?code=...&state=...`. \
                     That \"site can't be reached\" page IS the right one — copy its URL \
                     and paste THAT here.\n\n  \
                     Re-run forge --login to try again."
                );
            }

            return extract_code_from_query(query, expected_state.as_deref());
        }
        // Otherwise assume the user pasted just the code value.
        if let Some(expected) = expected_state.as_deref() {
            eprintln!(
                "(note: state was not validated — paste the full URL to verify state={})",
                expected
            );
        }
        return Ok(trimmed.to_string());
    }
}

fn extract_code_from_query(query: &str, expected_state: Option<&str>) -> Result<String> {
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

    params
        .get("code")
        .cloned()
        .context("OAuth callback did not contain a code parameter")
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
