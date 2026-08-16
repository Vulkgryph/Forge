// SPDX-License-Identifier: Apache-2.0
//! Deciding what of a local Forge setup may travel to a remote machine.
//!
//! Running the agent on the machine you are working on, rather than tunnelling
//! every tool call, means that machine needs a Forge configuration. Copying the
//! local one wholesale is wrong twice over: some of it names services that only
//! exist here, and some of it is credentials that should not leave without
//! being asked for.
//!
//! This module makes the decision and produces the file. It performs no I/O and
//! knows nothing about SSH, so what may be sent is decided in one place and can
//! be tested without a remote machine.

/// Whether an endpoint's address means anything on another machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// A public host. Works from anywhere with a route to the internet.
    Public,
    /// Loopback. Names a service on whichever machine reads it, so sending it
    /// does not share your local model server — it points the remote at its own
    /// localhost, where there is most likely nothing listening. Copied, it
    /// looks configured and fails at the first request.
    Loopback,
    /// A private or tailnet address. Reachable only if the remote shares that
    /// network, which this cannot know from here.
    PrivateNetwork,
}

/// Classify an endpoint's `base_url` by whether it survives the trip.
pub fn classify(base_url: &str) -> Reach {
    let host = host_of(base_url);
    if host.eq_ignore_ascii_case("localhost") || host.eq_ignore_ascii_case("::1") {
        return Reach::Loopback;
    }
    let octets: Vec<u8> = host
        .split('.')
        .filter_map(|p| p.parse::<u8>().ok())
        .collect();
    if octets.len() != 4 || host.split('.').count() != 4 {
        // A name, not an IPv4 literal. Anything that resolves publicly is
        // public; a bare hostname from someone's LAN would need DNS the remote
        // may not share, but that is indistinguishable from here and guessing
        // against the user is worse than letting it through.
        return Reach::Public;
    }
    match (octets[0], octets[1]) {
        (127, _) => Reach::Loopback,
        (10, _) => Reach::PrivateNetwork,
        (192, 168) => Reach::PrivateNetwork,
        (169, 254) => Reach::PrivateNetwork,
        (172, b) if (16..=31).contains(&b) => Reach::PrivateNetwork,
        // Tailscale and other CGNAT overlays live in 100.64.0.0/10.
        (100, b) if (64..=127).contains(&b) => Reach::PrivateNetwork,
        _ => Reach::Public,
    }
}

/// The host part of a URL, without scheme, port, credentials or path.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    // A bracketed IPv6 literal keeps its colons; everything else loses its port.
    if let Some(inner) = rest.strip_prefix('[') {
        return inner.split(']').next().unwrap_or(inner);
    }
    rest.split(':').next().unwrap_or(rest)
}

/// What a remote setup would consist of, decided but not yet carried out.
#[derive(Debug, Clone)]
pub struct ConfigPlan {
    /// The configuration to write on the remote machine.
    pub toml: String,
    /// Endpoint names that travel.
    pub kept: Vec<String>,
    /// Endpoint names that do not, and why — shown to the user, since an
    /// endpoint quietly missing on the far side is a puzzle rather than a
    /// decision.
    pub dropped: Vec<(String, String)>,
    /// Things the user should know but that do not stop the setup.
    pub warnings: Vec<String>,
    /// Whether any API key is present in `toml`.
    pub carries_credentials: bool,
}

/// Build the configuration to install on a remote machine.
///
/// `with_credentials` is the user's answer to being asked, and is the only
/// thing that lets a key leave this machine. Without it the endpoints still
/// travel — their URLs, models and limits are the tedious part to retype — but
/// every key is stripped, and Forge on the far side will say it is not
/// configured until one is supplied there.
pub fn plan_config(local_toml: &str, with_credentials: bool) -> Result<ConfigPlan, String> {
    let mut doc: toml::Value =
        toml::from_str(local_toml).map_err(|e| format!("local config is not valid TOML: {e}"))?;

    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    let mut warnings = Vec::new();
    let mut carries_credentials = false;

    let endpoints = doc
        .get_mut("models")
        .and_then(|m| m.get_mut("endpoints"))
        .and_then(|e| e.as_array_mut());

    if let Some(endpoints) = endpoints {
        let mut surviving = Vec::new();
        for ep in endpoints.iter() {
            let name = ep
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("(unnamed)")
                .to_string();
            let url = ep.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
            match classify(url) {
                Reach::Loopback => {
                    dropped.push((
                        name,
                        format!("{url} is this machine's own localhost — on the remote it would \
                                 point at a service there, not at yours"),
                    ));
                    continue;
                }
                Reach::PrivateNetwork => {
                    warnings.push(format!(
                        "{name} is at {url}, a private or tailnet address — it will only work if \
                         the remote machine is on that network too"
                    ));
                }
                Reach::Public => {}
            }

            let mut ep = ep.clone();
            if !with_credentials {
                if let Some(t) = ep.as_table_mut() {
                    if t.get("api_key").and_then(|v| v.as_str()).is_some_and(|k| !k.is_empty()) {
                        t.insert("api_key".into(), toml::Value::String(String::new()));
                    }
                }
            } else if ep
                .get("api_key")
                .and_then(|v| v.as_str())
                .is_some_and(|k| !k.is_empty())
            {
                carries_credentials = true;
            }
            kept.push(name);
            surviving.push(ep);
        }
        *endpoints = surviving;
    }

    if kept.is_empty() {
        return Err(
            "none of the configured endpoints would work from the remote machine — they all \
             point at services on this one"
                .into(),
        );
    }

    // The default must name something that travelled, or Forge on the far side
    // starts pointing at an endpoint it does not have.
    if let Some(models) = doc.get_mut("models").and_then(|m| m.as_table_mut()) {
        let default_ok = models
            .get("default")
            .and_then(|v| v.as_str())
            .is_some_and(|d| kept.iter().any(|k| k == d));
        if !default_ok {
            let replacement = kept[0].clone();
            if let Some(old) = models.get("default").and_then(|v| v.as_str()) {
                warnings.push(format!(
                    "default model was {old}, which does not travel; using {replacement} instead"
                ));
            }
            models.insert("default".into(), toml::Value::String(replacement));
        }
    }

    let toml = toml::to_string_pretty(&doc).map_err(|e| format!("could not write config: {e}"))?;
    Ok(ConfigPlan { toml, kept, dropped, warnings, carries_credentials })
}

/// Where Forge keeps its configuration, relative to a home directory. The
/// agent resolves this itself with `dirs::config_dir()`; on the Linux machines
/// these workspaces run on that is `~/.config`.
pub const REMOTE_CONFIG_DIR: &str = ".config/forge";
pub const REMOTE_CONFIG_FILE: &str = ".config/forge/config.toml";

/// What was found on the remote machine, and therefore what to do about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteState {
    /// Forge is already set up there. Nothing is sent, and nothing is asked:
    /// an existing configuration is that machine's own and is not overwritten.
    Configured,
    /// No configuration found. The user is asked whether to mirror this
    /// machine's, with or without credentials.
    NeedsSetup,
}

/// Decide from a directory listing whether the remote already has Forge.
///
/// Taken from a listing rather than a read, so deciding this never involves
/// asking for the contents of a file that may hold someone's keys.
pub fn remote_state(config_dir_listing: Result<Vec<String>, String>) -> RemoteState {
    match config_dir_listing {
        Ok(names) if names.iter().any(|n| n == "config.toml") => RemoteState::Configured,
        _ => RemoteState::NeedsSetup,
    }
}

/// Files that are credentials in their own right, beside the config.
///
/// These are copied only when the user has agreed to send credentials, and
/// never rewritten — an OAuth token is not something to edit in transit.
pub const CREDENTIAL_FILES: &[&str] = &["auth.json", "chatgpt_auth.json"];

#[cfg(test)]
mod tests {
    use super::*;

    const LOCAL: &str = r#"
[models]
default = "Local Model"

[[models.endpoints]]
name = "Local Model"
base_url = "http://127.0.0.1:9000/v1"
api_key = "sk-local-secret"

[[models.endpoints]]
name = "Claude"
base_url = "https://api.anthropic.com"
api_key = "sk-ant-secret"

[[models.endpoints]]
name = "Tailnet Vision"
base_url = "http://100.99.208.82:8082"
api_key = "sk-tailnet-secret"

[agent]
context_strategy = "compaction"
"#;

    #[test]
    fn an_existing_remote_configuration_is_left_alone() {
        let listing = Ok(vec!["config.toml".to_string(), "auth.json".to_string()]);
        assert_eq!(remote_state(listing), RemoteState::Configured);
    }

    #[test]
    fn a_machine_without_forge_is_offered_setup() {
        assert_eq!(remote_state(Ok(vec![])), RemoteState::NeedsSetup);
        // A missing directory is the same answer as an empty one; both mean
        // there is nothing there to preserve.
        assert_eq!(remote_state(Err("no such file".into())), RemoteState::NeedsSetup);
    }

    #[test]
    fn a_directory_without_a_config_still_needs_setup() {
        // Something else living under .config/forge is not a configuration.
        let listing = Ok(vec!["ui-state.json".to_string()]);
        assert_eq!(remote_state(listing), RemoteState::NeedsSetup);
    }

    #[test]
    fn a_public_endpoint_travels() {
        assert_eq!(classify("https://api.anthropic.com"), Reach::Public);
        assert_eq!(classify("https://api.x.ai/v1"), Reach::Public);
        assert_eq!(classify("https://chatgpt.com/backend-api/codex"), Reach::Public);
    }

    #[test]
    fn loopback_is_not_a_shared_service() {
        // The trap this exists for: copied over, it looks configured and points
        // at nothing.
        assert_eq!(classify("http://127.0.0.1:9000/v1"), Reach::Loopback);
        assert_eq!(classify("http://localhost:1234"), Reach::Loopback);
        assert_eq!(classify("http://[::1]:8080/v1"), Reach::Loopback);
    }

    #[test]
    fn private_and_tailnet_addresses_are_conditional() {
        assert_eq!(classify("http://192.168.1.50:8080"), Reach::PrivateNetwork);
        assert_eq!(classify("http://10.0.0.5:8080"), Reach::PrivateNetwork);
        assert_eq!(classify("http://172.20.0.5:8080"), Reach::PrivateNetwork);
        // Tailscale's range, which is what this user's own config uses.
        assert_eq!(classify("http://100.99.208.82:8082"), Reach::PrivateNetwork);
        // 100.x outside the CGNAT block is ordinary public space.
        assert_eq!(classify("http://100.200.1.1"), Reach::Public);
    }

    #[test]
    fn without_permission_no_key_leaves_the_machine() {
        // The whole security question in one assertion.
        let plan = plan_config(LOCAL, false).unwrap();
        assert!(!plan.toml.contains("secret"), "a key escaped:\n{}", plan.toml);
        assert!(!plan.carries_credentials);
        assert!(plan.kept.iter().any(|k| k == "Claude"), "endpoints still travel: {:?}", plan.kept);
    }

    #[test]
    fn with_permission_the_keys_travel_but_only_for_endpoints_that_do() {
        let plan = plan_config(LOCAL, true).unwrap();
        assert!(plan.toml.contains("sk-ant-secret"), "the public endpoint keeps its key");
        assert!(
            !plan.toml.contains("sk-local-secret"),
            "a dropped endpoint must not leave its key behind:\n{}",
            plan.toml
        );
        assert!(plan.carries_credentials);
    }

    #[test]
    fn a_localhost_endpoint_is_dropped_and_said_so() {
        let plan = plan_config(LOCAL, true).unwrap();
        assert!(!plan.kept.iter().any(|k| k == "Local Model"));
        let (name, why) = plan.dropped.iter().find(|(n, _)| n == "Local Model").expect("dropped");
        assert_eq!(name, "Local Model");
        assert!(why.contains("localhost"), "the reason must be usable: {why}");
    }

    #[test]
    fn a_tailnet_endpoint_travels_with_a_warning() {
        // It may well work — this user's own vision model is on a tailnet — so
        // dropping it would be wrong. Saying nothing would be too.
        let plan = plan_config(LOCAL, true).unwrap();
        assert!(plan.kept.iter().any(|k| k == "Tailnet Vision"));
        assert!(
            plan.warnings.iter().any(|w| w.contains("Tailnet Vision") && w.contains("network")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn the_default_model_is_repointed_when_it_does_not_travel() {
        // Otherwise the remote starts up naming an endpoint it does not have.
        let plan = plan_config(LOCAL, true).unwrap();
        let doc: toml::Value = toml::from_str(&plan.toml).unwrap();
        let default = doc["models"]["default"].as_str().unwrap();
        assert!(plan.kept.iter().any(|k| k == default), "default {default:?} is not among {:?}", plan.kept);
        assert!(plan.warnings.iter().any(|w| w.contains("default model")), "{:?}", plan.warnings);
    }

    #[test]
    fn settings_that_are_not_endpoints_are_carried_over_untouched() {
        let plan = plan_config(LOCAL, false).unwrap();
        let doc: toml::Value = toml::from_str(&plan.toml).unwrap();
        assert_eq!(doc["agent"]["context_strategy"].as_str(), Some("compaction"));
    }

    #[test]
    fn a_setup_with_nothing_that_travels_is_refused() {
        // Better than writing a config the remote cannot use and reporting
        // success.
        let only_local = r#"
[models]
default = "Local"
[[models.endpoints]]
name = "Local"
base_url = "http://127.0.0.1:9000/v1"
"#;
        let err = plan_config(only_local, true).unwrap_err();
        assert!(err.contains("none of the configured endpoints"), "{err}");
    }
}

#[cfg(test)]
mod real_config {
    use super::*;

    /// Run against whatever is actually in `~/.config/forge/config.toml`.
    /// Ignored by default — it depends on the machine it runs on — but it is
    /// the only check that the rules hold against a configuration nobody wrote
    /// for a test. `cargo test -p forge-ide -- --ignored real_config`
    #[test]
    #[ignore = "reads the developer's own Forge configuration"]
    fn the_local_configuration_produces_a_sane_plan() {
        let path = dirs::home_dir().unwrap().join(".config/forge/config.toml");
        let Ok(local) = std::fs::read_to_string(&path) else {
            eprintln!("no config at {}", path.display());
            return;
        };
        let plan = plan_config(&local, false).expect("a plan");
        eprintln!("kept: {:?}", plan.kept);
        eprintln!("dropped: {:#?}", plan.dropped);
        eprintln!("warnings: {:#?}", plan.warnings);
        assert!(!plan.carries_credentials);
        for line in plan.toml.lines().filter(|l| l.contains("api_key")) {
            assert!(
                line.split('=').nth(1).is_some_and(|v| v.trim() == "\"\""),
                "a key survived a no-credentials plan: {line}"
            );
        }
    }
}
