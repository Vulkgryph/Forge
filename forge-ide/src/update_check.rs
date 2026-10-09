//! Background GitHub release checks, on startup and hourly when enabled.
//! Failures are reported to Output without discarding an already known update.

use std::sync::mpsc;

#[derive(Debug)]
pub struct UpdateAvailable {
    pub latest_version: String,
    pub url: String,
}

pub const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3600);

pub fn check_due(enabled: bool, pending: bool, elapsed: Option<std::time::Duration>) -> bool {
    enabled && !pending && elapsed.is_none_or(|age| age >= CHECK_INTERVAL)
}

/// Spawns the check on a background thread and returns immediately; poll
/// the receiver from the draw loop the same way other background tasks in
/// this codebase are drained (e.g. `ssh_connect_rx`).
/// What to tell someone who asked for a check, given what it found.
///
/// Only the no-update case strictly needs saying — a newer release raises the
/// banner by itself — but both are returned from one place so the two can never
/// drift into disagreeing about the version they name.
///
/// Here rather than in the draw loop so it can be asserted without standing up
/// an application: the bug this exists for is a menu item that answered
/// nothing, and "answered nothing" is exactly what a test of the draw loop
/// would have found hardest to see.
pub fn manual_result_message(result: Option<&UpdateAvailable>, current: &str) -> String {
    match result {
        Some(update) => format!("Forge {} is available", update.latest_version),
        None => format!("Forge {current} is the latest release"),
    }
}

pub fn spawn_check() -> mpsc::Receiver<Result<Option<UpdateAvailable>, String>> {
    let (tx, rx) = mpsc::channel();
    let current = env!("CARGO_PKG_VERSION").to_string();
    std::thread::spawn(move || {
        let _ = tx.send(check_once(&current));
        crate::wake::wake();
    });
    rx
}

/// The repository releases are published from.
///
/// One constant, because there were two spellings of it and they were both the
/// retired standalone checkout — `windingcreek/Forge-IDE`, which the top-level
/// README describes as superseded by this monorepo. Every installed copy asked
/// that repository for its updates, which at best answers nothing forever and
/// at worst answers with releases from before the merge.
const RELEASES_REPO: &str = "Vulkgryph/Forge";

fn check_once(current_version: &str) -> Result<Option<UpdateAvailable>, String> {
    let body: serde_json::Value = ureq::get(
        &format!("https://api.github.com/repos/{RELEASES_REPO}/releases/latest"),
    )
    .set("User-Agent", "forge-ide-update-check")
    .timeout(std::time::Duration::from_secs(8))
    .call()
    .map_err(|e| e.to_string())?
    .into_json()
    .map_err(|e| e.to_string())?;

    Ok(release_from_response(current_version, &body))
}

fn release_from_response(current_version: &str, body: &serde_json::Value) -> Option<UpdateAvailable> {
    if body.get("draft").and_then(|v| v.as_bool()) == Some(true)
        || body.get("prerelease").and_then(|v| v.as_bool()) == Some(true) { return None; }

    let tag = body.get("tag_name")?.as_str()?;
    let latest = tag.trim_start_matches('v');
    let url = body
        .get("html_url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://github.com/{RELEASES_REPO}/releases/latest"));

    if is_newer(latest, current_version) {
        Some(UpdateAvailable { latest_version: latest.to_string(), url })
    } else {
        None
    }
}

/// Plain dotted-numeric comparison (`"1.2.10" > "1.2.9"`) — good enough for
/// this project's own tags, not a general semver parser (no pre-release/
/// build-metadata handling, which its own tags don't use).
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    };
    let (l, c) = (parse(latest), parse(current));
    for i in 0..l.len().max(c.len()) {
        let (lv, cv) = (l.get(i).copied().unwrap_or(0), c.get(i).copied().unwrap_or(0));
        if lv != cv { return lv > cv; }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::{is_newer, manual_result_message, UpdateAvailable};

    /// The repository has to be the live one. Every installed copy asks it for
    /// updates on startup, and the retired standalone checkout would answer
    /// nothing forever — or, worse, with releases from before the merge.
    /// A manual check has to answer even when there is nothing to report.
    /// Silence is what made the missing menu item indistinguishable from a
    /// broken one.
    #[test]
    fn a_manual_check_says_so_when_there_is_nothing_to_report() {
        let msg = manual_result_message(None, "0.6.1");
        assert!(msg.contains("0.6.1"), "{msg}");
        assert!(msg.contains("latest"), "{msg}");
        assert!(!msg.is_empty());
    }

    /// And names the new version when there is one, rather than the current.
    #[test]
    fn a_manual_check_names_the_new_version() {
        let update = UpdateAvailable {
            latest_version: "0.7.0".to_string(),
            url: "https://example.invalid/r".to_string(),
        };
        let msg = manual_result_message(Some(&update), "0.6.1");
        assert!(msg.contains("0.7.0"), "{msg}");
        assert!(!msg.contains("0.6.1"), "it reported the version you already have: {msg}");
    }

    #[test]
    fn updates_are_checked_against_the_monorepo() {
        assert_eq!(super::RELEASES_REPO, "Vulkgryph/Forge");
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.9", "0.1.9"));
        assert!(!is_newer("0.1.8", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
    }

    #[test]
    fn release_payload_produces_an_actionable_notice_only_for_a_new_release() {
        let mut body = serde_json::json!({"tag_name": "v0.5.2", "html_url": "https://github.com/Vulkgryph/Forge/releases/tag/v0.5.2"});
        let notice = super::release_from_response("0.5.1", &body).unwrap();
        assert_eq!(notice.latest_version, "0.5.2");
        assert!(notice.url.ends_with("/v0.5.2"));
        assert!(super::release_from_response("0.5.2", &body).is_none());
        body["draft"] = true.into();
        assert!(super::release_from_response("0.5.1", &body).is_none());
    }

    #[test]
    fn checks_repeat_while_open_but_respect_opt_out_and_in_flight_requests() {
        use super::{check_due, CHECK_INTERVAL};
        assert!(check_due(true, false, None));
        assert!(!check_due(false, false, None));
        assert!(!check_due(true, true, Some(CHECK_INTERVAL)));
        assert!(!check_due(true, false, Some(std::time::Duration::from_secs(30))));
        assert!(check_due(true, false, Some(CHECK_INTERVAL)));
    }
}
