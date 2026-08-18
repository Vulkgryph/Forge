//! Update checker — one best-effort GET to the GitHub Releases API on
//! startup, in a background thread, gated on `Settings::check_for_updates`
//! (opt-in, off by default). Never surfaces an error; a check that fails
//! (offline, rate-limited, GitHub down) is indistinguishable from "already
//! up to date" from the UI's point of view.

use std::sync::mpsc;

pub struct UpdateAvailable {
    pub latest_version: String,
    pub url: String,
}

/// Spawns the check on a background thread and returns immediately; poll
/// the receiver from the draw loop the same way other background tasks in
/// this codebase are drained (e.g. `ssh_connect_rx`).
pub fn spawn_check() -> mpsc::Receiver<Option<UpdateAvailable>> {
    let (tx, rx) = mpsc::channel();
    let current = env!("CARGO_PKG_VERSION").to_string();
    std::thread::spawn(move || {
        let _ = tx.send(check_once(&current));
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

fn check_once(current_version: &str) -> Option<UpdateAvailable> {
    let body: serde_json::Value = ureq::get(
        &format!("https://api.github.com/repos/{RELEASES_REPO}/releases/latest"),
    )
    .set("User-Agent", "forge-ide-update-check")
    .timeout(std::time::Duration::from_secs(8))
    .call()
    .ok()?
    .into_json()
    .ok()?;

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
    use super::is_newer;

    /// The repository has to be the live one. Every installed copy asks it for
    /// updates on startup, and the retired standalone checkout would answer
    /// nothing forever — or, worse, with releases from before the merge.
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
}
