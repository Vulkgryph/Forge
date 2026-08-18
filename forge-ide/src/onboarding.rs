//! Native first-run setup for forge-agent's own config
//! (`~/.config/forge/config.toml`) — lets a user pick a provider and enter
//! credentials through Forge IDE's GUI instead of `install.sh` or a text
//! editor. Writes the same file forge-agent, forge-tui, and every agent
//! panel tab already read — nothing IDE-specific about the format.

use std::path::PathBuf;

/// The credential for an endpoint, held here and lent over the tunnel rather
/// than sent to the agent.
///
/// ChatGPT Codex is the case this exists for: its credential is an OAuth access
/// token in a file beside the config, not a key in it, so there has never been
/// anything for a config copy or a `switch_model` to carry. Reading it here
/// means the token stays on this machine — and, in time, that this is also
/// where it gets refreshed, which a copy on another machine could never do.
pub fn credential_for(endpoint_type: &str, api_key: &str) -> Option<(String, Vec<(String, String)>)> {
    if endpoint_type != "chatgpt_codex" {
        return (!api_key.is_empty()).then(|| (api_key.to_string(), Vec::new()));
    }
    let path = dirs::home_dir()?
        .join(".config")
        .join("forge")
        .join("chatgpt_auth.json");
    let text = std::fs::read_to_string(path).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    let token = doc.get("access_token")?.as_str()?.to_string();
    if token.is_empty() {
        return None;
    }
    let mut extra = Vec::new();
    if let Some(account) = doc.get("account_id").and_then(|v| v.as_str()) {
        // Both spellings, as forge-agent sends them.
        extra.push(("ChatGPT-Account-ID".to_string(), account.to_string()));
        extra.push(("chatgpt-account-id".to_string(), account.to_string()));
    }
    Some((token, extra))
}

/// This machine's default endpoint, as a `switch_model` payload including its
/// key.
///
/// For an agent running on another machine. That machine has no reason to hold
/// these credentials — and on a box you do not administer, every reason not to
/// — so the client keeps them and hands one over when it picks an endpoint.
/// `switch_model` is applied in memory by the agent and written nowhere, so the
/// key lives only in that process for as long as it runs.
pub fn local_default_endpoint() -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(forge_config_path()?).ok()?;
    let doc: toml::Value = text.parse().ok()?;
    let models = doc.get("models")?;
    let endpoints = models.get("endpoints")?.as_array()?;
    let default = models.get("default").and_then(|v| v.as_str());

    // The configured default, or the first endpoint that could work at all.
    // An endpoint on this machine's own localhost is skipped: over there it
    // names a service on *that* machine, where there is nothing listening.
    let usable = |ep: &toml::Value| {
        ep.get("base_url")
            .and_then(|v| v.as_str())
            .is_some_and(|u| !u.contains("127.0.0.1") && !u.contains("localhost"))
    };
    let chosen = endpoints
        .iter()
        .find(|ep| {
            default.is_some_and(|d| ep.get("name").and_then(|v| v.as_str()) == Some(d)) && usable(ep)
        })
        .or_else(|| endpoints.iter().find(|ep| usable(ep)))?;

    let get = |k: &str| chosen.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let num = |k: &str, d: u64| chosen.get(k).and_then(|v| v.as_integer()).unwrap_or(d as i64);
    Some(serde_json::json!({
        "name":               get("name"),
        "base_url":           get("base_url"),
        "model_id":           get("model_id"),
        "max_context_tokens": num("max_context_tokens", 200_000),
        "max_output_tokens":  num("max_output_tokens", 16_384),
        "endpoint_type":      get("endpoint_type"),
        "api_key":            chosen.get("api_key").and_then(|v| v.as_str()).unwrap_or(""),
    }))
}

fn forge_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".config").join("forge").join("config.toml"))
}

/// The exact shape of the endpoint forge-agent's own `AppConfig::default()`
/// writes the first time it runs with no config file at all — recognizing
/// it lets us tell "genuinely never configured" apart from "user really
/// does want a local server at this address", without needing to track our
/// own separate "have we shown the wizard" flag in a file we don't own.
const DEFAULT_PLACEHOLDER_NAME: &str = "local";
const DEFAULT_PLACEHOLDER_URL: &str = "http://127.0.0.1:1234/v1";

/// True if forge-agent has no real provider configured yet: no config file,
/// an empty endpoint list, or just the untouched auto-created placeholder.
pub fn needs_setup() -> bool {
    let Some(path) = forge_config_path() else { return false };
    let Ok(text) = std::fs::read_to_string(&path) else { return true };
    let Ok(value) = text.parse::<toml::Value>() else { return true };
    let endpoints = value.get("models").and_then(|m| m.get("endpoints")).and_then(|e| e.as_array());
    match endpoints {
        None => true,
        Some(eps) if eps.is_empty() => true,
        Some(eps) if eps.len() == 1 => {
            eps[0].get("name").and_then(|v| v.as_str()) == Some(DEFAULT_PLACEHOLDER_NAME)
                && eps[0].get("base_url").and_then(|v| v.as_str()) == Some(DEFAULT_PLACEHOLDER_URL)
        }
        _ => false,
    }
}

pub struct NewEndpoint {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model_id: String,
    pub max_context_tokens: usize,
    /// "open_ai" or "anthropic" — matches forge's own `EndpointType` serde tags.
    pub endpoint_type: &'static str,
}

/// Adds `ep` to `models.endpoints`, drops the untouched placeholder (if
/// that's all that was there), and sets it as `models.default`. Reads and
/// re-serializes the whole file as a generic `toml::Value` rather than
/// forge-agent's own config structs (a separate crate we don't depend on)
/// so unrelated fields (disabled_tools, subagents, other endpoints a user
/// already configured by hand) survive untouched.
pub fn add_endpoint(ep: NewEndpoint) -> Result<(), String> {
    let path = forge_config_path().ok_or("could not find home directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let mut doc: toml::Value = if path.exists() {
        let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        text.parse().map_err(|e: toml::de::Error| format!("existing config.toml is invalid: {e}"))?
    } else {
        toml::Value::Table(Default::default())
    };

    let table = doc.as_table_mut().ok_or("config.toml root is not a table")?;
    let models = table.entry("models".to_string())
        .or_insert_with(|| toml::Value::Table(Default::default()));
    let models_table = models.as_table_mut().ok_or("models is not a table")?;
    let endpoints_val = models_table.entry("endpoints".to_string())
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let endpoints_arr = endpoints_val.as_array_mut().ok_or("models.endpoints is not an array")?;

    endpoints_arr.retain(|e| {
        !(e.get("name").and_then(|v| v.as_str()) == Some(DEFAULT_PLACEHOLDER_NAME)
            && e.get("base_url").and_then(|v| v.as_str()) == Some(DEFAULT_PLACEHOLDER_URL))
    });

    let mut entry = toml::value::Table::new();
    entry.insert("name".into(), ep.name.clone().into());
    entry.insert("base_url".into(), ep.base_url.into());
    if let Some(key) = ep.api_key {
        if !key.is_empty() { entry.insert("api_key".into(), key.into()); }
    }
    entry.insert("model_id".into(), ep.model_id.into());
    entry.insert("max_context_tokens".into(), (ep.max_context_tokens as i64).into());
    entry.insert("endpoint_type".into(), ep.endpoint_type.into());
    endpoints_arr.push(toml::Value::Table(entry));

    models_table.insert("default".to_string(), ep.name.into());

    // agent/subagents tables need to exist with forge-agent's own required
    // fields the first time this file is ever created — after that, leave
    // them alone entirely (forge-agent's own serde defaults fill in any
    // fields we don't touch on future loads).
    if !table.contains_key("agent") {
        let mut agent = toml::value::Table::new();
        agent.insert("auto_approve_reads".into(), true.into());
        agent.insert("auto_approve_writes".into(), false.into());
        agent.insert("max_history_messages".into(), 200i64.into());
        agent.insert("compaction_threshold".into(), 150i64.into());
        table.insert("agent".to_string(), toml::Value::Table(agent));
    }

    let text = toml::to_string_pretty(&doc).map_err(|e| e.to_string())?;
    std::fs::write(&path, text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `HOME` is process-global — `cargo test` runs tests on multiple
    /// threads within the same process by default, so without this, these
    /// tests race each other's `HOME` overrides (confirmed: they pass with
    /// `--test-threads=1` and fail intermittently without it). Held for the
    /// lifetime of `TempHome` so only one of these tests touches `HOME` at
    /// a time, regardless of how `cargo test` schedules them.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct TempHome {
        dir: std::path::PathBuf,
        _guard: std::sync::MutexGuard<'static, ()>,
    }
    impl TempHome {
        fn new(tag: &str) -> Self {
            let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!("forge_ide_onboarding_test_{tag}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            unsafe { std::env::set_var("HOME", &dir); }
            Self { dir, _guard: guard }
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.dir); }
    }

    #[test]
    fn needs_setup_when_no_config_file() {
        let _home = TempHome::new("no_config");
        assert!(needs_setup());
    }

    #[test]
    fn add_endpoint_writes_and_clears_setup_need() {
        let _home = TempHome::new("add_endpoint");
        assert!(needs_setup());

        add_endpoint(NewEndpoint {
            name: "claude".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: Some("sk-test-123".into()),
            model_id: "claude-sonnet-4-6".into(),
            max_context_tokens: 200_000,
            endpoint_type: "anthropic",
        }).unwrap();

        assert!(!needs_setup());

        let path = forge_config_path().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let value: toml::Value = text.parse().unwrap();
        assert_eq!(value["models"]["default"].as_str(), Some("claude"));
        let eps = value["models"]["endpoints"].as_array().unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0]["name"].as_str(), Some("claude"));
        assert_eq!(eps[0]["api_key"].as_str(), Some("sk-test-123"));
        assert_eq!(eps[0]["endpoint_type"].as_str(), Some("anthropic"));
        assert_eq!(eps[0]["max_context_tokens"].as_integer(), Some(200_000));
        assert!(value["agent"]["auto_approve_reads"].as_bool().unwrap());
    }

    #[test]
    fn add_endpoint_replaces_untouched_placeholder() {
        let _home = TempHome::new("replace_placeholder");
        let path = forge_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"
[models]
default = "local"

[[models.endpoints]]
name = "local"
base_url = "http://127.0.0.1:1234/v1"
model_id = "auto"
max_context_tokens = 32768

[agent]
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 200
compaction_threshold = 150
"#).unwrap();
        assert!(needs_setup());

        add_endpoint(NewEndpoint {
            name: "local".into(),
            base_url: "http://127.0.0.1:8081/v1".into(),
            api_key: None,
            model_id: "Qwen3-Coder".into(),
            max_context_tokens: 65536,
            endpoint_type: "open_ai",
        }).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: toml::Value = text.parse().unwrap();
        let eps = value["models"]["endpoints"].as_array().unwrap();
        assert_eq!(eps.len(), 1, "the untouched placeholder should be replaced, not appended to");
        assert_eq!(eps[0]["base_url"].as_str(), Some("http://127.0.0.1:8081/v1"));
        assert!(eps[0].get("api_key").is_none(), "no api_key means the key should be entirely absent, not an empty string");
    }

    #[test]
    fn add_endpoint_preserves_unrelated_existing_config() {
        let _home = TempHome::new("preserve_existing");
        let path = forge_config_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"
[models]
default = "existing"

[[models.endpoints]]
name = "existing"
base_url = "https://api.x.ai/v1"
model_id = "grok-4.5"
api_key = "xai-should-survive"
max_context_tokens = 500000

[agent]
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 200
compaction_threshold = 150
disabled_tools = ["web_search"]
"#).unwrap();
        assert!(!needs_setup(), "a real pre-existing endpoint should never trigger the wizard");

        add_endpoint(NewEndpoint {
            name: "claude".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: Some("sk-new".into()),
            model_id: "claude-sonnet-4-6".into(),
            max_context_tokens: 200_000,
            endpoint_type: "anthropic",
        }).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        let value: toml::Value = text.parse().unwrap();
        let eps = value["models"]["endpoints"].as_array().unwrap();
        assert_eq!(eps.len(), 2, "should add alongside the real existing endpoint, not replace it");
        assert!(eps.iter().any(|e| e["name"].as_str() == Some("existing") && e["api_key"].as_str() == Some("xai-should-survive")));
        assert_eq!(
            value["agent"]["disabled_tools"].as_array().unwrap()[0].as_str(),
            Some("web_search"),
            "unrelated existing agent settings should survive untouched"
        );
    }
}

// ── ChatGPT Codex OAuth login ────────────────────────────────────────────
//
// `forge-agent --login-chatgpt` is a self-contained, non-headless CLI flow:
// it prints a URL, tries to open a browser itself, and races a local
// callback listener against a stdin-paste fallback for restricted networks
// (see forge-agent's own auth::login_chatgpt). On success it writes both
// `~/.config/forge/chatgpt_auth.json` and the corresponding endpoint into
// `config.toml` itself — nothing here needs to touch config.toml for this
// path, only drive the subprocess and surface its plain-text output.

pub enum LoginEvent {
    Line(String),
}

pub struct CodexLogin {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    pub rx: std::sync::mpsc::Receiver<LoginEvent>,
}

impl CodexLogin {
    pub fn spawn() -> Result<Self, String> {
        use std::process::{Command, Stdio};
        let mut child = Command::new(crate::agent_panel::resolve_forge_agent_path())
            .arg("--login-chatgpt")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not spawn forge-agent: {e}"))?;

        let stdin = child.stdin.take().ok_or("no stdin pipe")?;
        let stdout = child.stdout.take().ok_or("no stdout pipe")?;
        let stderr = child.stderr.take().ok_or("no stderr pipe")?;

        let (tx, rx) = std::sync::mpsc::channel();
        let tx_out = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx_out.send(LoginEvent::Line(line)).is_err() { return; }
            }
        });
        let tx_err = tx.clone();
        std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
                if tx_err.send(LoginEvent::Line(line)).is_err() { return; }
            }
        });

        Ok(Self { child, stdin, rx })
    }

    /// Sends one line to the child's stdin — the paste-fallback path when
    /// the automatic OAuth callback can't reach this machine.
    pub fn send_line(&mut self, line: &str) {
        use std::io::Write;
        let _ = writeln!(self.stdin, "{line}");
        let _ = self.stdin.flush();
    }

    /// Non-blocking check for exit — call once per frame.
    pub fn poll_exit(&mut self) -> Option<bool> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.success()),
            _ => None,
        }
    }
}

// ── Wizard UI state ───────────────────────────────────────────────────────

pub struct LocalForm {
    pub base_url: String,
    pub model_id: String,
    pub max_context_tokens: String,
}

impl Default for LocalForm {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:1234/v1".to_string(),
            model_id: "auto".to_string(),
            max_context_tokens: "32768".to_string(),
        }
    }
}

#[derive(Default)]
pub struct KeyForm {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model_id: String,
    pub max_context_tokens: String,
}

pub enum OnboardingStep {
    ProviderPicker,
    Local(LocalForm),
    Anthropic(KeyForm),
    DirectApiKey(KeyForm),
    Codex {
        login: CodexLogin,
        log: Vec<String>,
        paste_input: String,
        done: Option<bool>,
    },
    /// Terminal states shown briefly before the wizard closes itself.
    Done { message: String },
    Error { message: String },
}
