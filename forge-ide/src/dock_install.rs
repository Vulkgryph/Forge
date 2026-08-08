// SPDX-License-Identifier: Apache-2.0
//! Adding Forge IDE to the macOS Dock.
//!
//! The Dock keeps its permanent icons in `com.apple.dock`'s `persistent-apps`
//! array, and re-reads that list only when it restarts — so adding an icon means
//! appending an entry and then restarting the Dock, which is what every
//! "add to Dock" script does and why the Dock blinks when you do it.
//!
//! There is no public API for this. `defaults` is the supported way to write the
//! preference, so that is what this shells out to; the alternative is writing the
//! plist by hand and racing `cfprefsd`, which caches it.
//!
//! The parts that are easy to get wrong — deriving the bundle from the running
//! executable, noticing an icon that is already there, building the entry — are
//! separated out and tested. What is left is three commands.

use std::path::{Path, PathBuf};

/// What happened, so the caller can say something true.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Added,
    /// It was already there. Adding again would put a second identical icon in
    /// the Dock, which is not what "add to Dock" means to anyone.
    AlreadyThere,
}

/// The `.app` the running executable lives in.
///
/// `Forge IDE.app/Contents/MacOS/forge-ide` — three levels up. `None` when the
/// binary is not inside a bundle, which is the normal case for `cargo run`: the
/// Dock can only hold an application, so there is nothing to add.
fn bundle_from_exe(exe: &Path) -> Option<PathBuf> {
    let bundle = exe.parent()?.parent()?.parent()?;
    (bundle.extension()?.eq_ignore_ascii_case("app")).then(|| bundle.to_path_buf())
}

/// Whether the Dock already lists this application.
///
/// Every entry the Dock writes itself is a `file://` URL with the path
/// percent-encoded — `file:///Applications/Forge%20IDE.app/` — so a comparison
/// against a plain path never matches and the icon would be added again on every
/// press. Decoded back to a path before comparing, with the trailing slash the
/// Dock appends ignored.
fn already_listed(persistent_apps: &str, bundle: &Path) -> bool {
    let wanted = bundle.to_string_lossy();
    let wanted = wanted.trim_end_matches('/');
    persistent_apps
        .lines()
        // `defaults` prints the key quoted — `"_CFURLString" = "file:///path/";`
        .filter(|line| line.contains("_CFURLString\""))
        .filter_map(|line| line.split_once('='))
        .map(|(_, value)| value.trim().trim_end_matches(';').trim().trim_matches('"'))
        .map(url_to_path)
        .any(|listed| listed.trim_end_matches('/') == wanted)
}

/// `file:///Applications/Forge%20IDE.app/` back to a path.
///
/// Anything that is not a `file://` URL is returned as it stands: older entries
/// are written as plain paths, and a Dock that has both should match either.
fn url_to_path(value: &str) -> String {
    let body = value.strip_prefix("file://").unwrap_or(value);
    let bytes = body.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(byte) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A path as the `file://` URL the Dock stores.
///
/// Only unreserved characters survive unescaped; everything else becomes `%XX`.
/// A space is the one that matters here — the application is called "Forge IDE".
fn path_to_url(path: &str) -> String {
    let mut out = String::from("file://");
    for byte in path.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    // The Dock writes a trailing slash on a bundle; matching it keeps a
    // hand-added icon and one added here indistinguishable.
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// The `persistent-apps` entry for an application.
///
/// Written the way the Dock writes its own entries: a `file://` URL with
/// `_CFURLStringType` 15. Every entry in a real Dock has that shape — checked
/// against one with 34 icons, none of which used the plain-path form — and
/// matching it is what keeps the duplicate check above honest.
fn tile_entry(bundle: &Path) -> String {
    // The URL is the only variable, and it goes into XML. Percent-encoding has
    // already removed everything XML would care about, but a path is
    // user-controlled and this is cheap.
    let url = path_to_url(&bundle.to_string_lossy())
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<dict><key>tile-data</key><dict><key>file-data</key><dict>\
         <key>_CFURLString</key><string>{url}</string>\
         <key>_CFURLStringType</key><integer>15</integer>\
         </dict></dict></dict>"
    )
}

/// Put Forge IDE in the Dock, permanently.
///
/// `Err` carries something worth showing a user: this is a convenience, and a
/// failure should say what went wrong rather than fail silently.
#[cfg(target_os = "macos")]
pub fn add_to_dock() -> Result<Outcome, String> {
    use std::process::Command;

    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this program: {e}"))?;
    let bundle = bundle_from_exe(&exe).ok_or_else(|| {
        "Forge IDE is running from a build directory rather than an installed app, \
         and the Dock can only hold an application."
            .to_string()
    })?;

    let listed = Command::new("defaults")
        .args(["read", "com.apple.dock", "persistent-apps"])
        .output()
        .map_err(|e| format!("could not read the Dock's settings: {e}"))?;
    // A Dock with no permanent icons has no such key and `defaults` fails; that
    // is an empty list, not an error.
    let listed = String::from_utf8_lossy(&listed.stdout);
    if already_listed(&listed, &bundle) {
        return Ok(Outcome::AlreadyThere);
    }

    let written = Command::new("defaults")
        .args(["write", "com.apple.dock", "persistent-apps", "-array-add"])
        .arg(tile_entry(&bundle))
        .output()
        .map_err(|e| format!("could not write the Dock's settings: {e}"))?;
    if !written.status.success() {
        return Err(format!(
            "the Dock rejected the change: {}",
            String::from_utf8_lossy(&written.stderr).trim(),
        ));
    }

    // The Dock reads that list once, at startup. Without this the icon appears
    // only after the next login, which reads as the button having done nothing.
    Command::new("killall")
        .arg("Dock")
        .output()
        .map_err(|e| format!("added, but the Dock could not be restarted: {e}"))?;

    Ok(Outcome::Added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bundle_is_three_levels_above_the_binary() {
        let exe = Path::new("/Applications/Forge IDE.app/Contents/MacOS/forge-ide");
        assert_eq!(
            bundle_from_exe(exe),
            Some(PathBuf::from("/Applications/Forge IDE.app")),
        );
    }

    /// Running from a build directory has no bundle to add, and must not offer to
    /// put `target/release` in the Dock.
    #[test]
    fn a_loose_binary_has_no_bundle() {
        assert_eq!(bundle_from_exe(Path::new("/repo/target/release/forge-ide")), None);
        assert_eq!(bundle_from_exe(Path::new("forge-ide")), None);
    }

    /// The check that stops a second identical icon appearing every time the
    /// button is pressed. The sample is real `defaults read` output, because the
    /// first version of this was written against an invented one that used plain
    /// paths — a shape no entry in a real Dock actually has.
    #[test]
    fn an_application_already_in_the_dock_is_recognised() {
        let listed = r#"(
        {
            "tile-data" =             {
                "file-data" =                 {
                    "_CFURLString" = "file:///System/Applications/Apps.app/";
                    "_CFURLStringType" = 15;
                };
            };
        },
        {
            "tile-data" =             {
                "file-data" =                 {
                    "_CFURLString" = "file:///Applications/Forge%20IDE.app/";
                    "_CFURLStringType" = 15;
                };
            };
        }
)"#;
        assert!(
            already_listed(listed, Path::new("/Applications/Forge IDE.app")),
            "a percent-encoded file:// URL is the same application",
        );
        assert!(!already_listed(listed, Path::new("/Applications/Other.app")));
        assert!(!already_listed("", Path::new("/Applications/Forge IDE.app")));
    }

    /// Entries written as plain paths still match, so a Dock holding both forms
    /// does not gain a duplicate.
    #[test]
    fn a_plain_path_entry_also_matches() {
        let listed = r#""_CFURLString" = "/Applications/Forge IDE.app/";"#;
        assert!(already_listed(listed, Path::new("/Applications/Forge IDE.app")));
    }

    /// A URL and a path convert to each other without losing anything, spaces
    /// included — the application is called "Forge IDE".
    #[test]
    fn paths_and_urls_round_trip() {
        let path = "/Applications/Forge IDE.app";
        let url = path_to_url(path);
        assert_eq!(url, "file:///Applications/Forge%20IDE.app/");
        assert_eq!(url_to_path(&url).trim_end_matches('/'), path);
    }

    /// A path that is a prefix of another must not count as a match.
    #[test]
    fn a_similar_path_is_not_the_same_application() {
        let listed = r#""_CFURLString" = "/Applications/Forge IDE Nightly.app/";"#;
        assert!(!already_listed(listed, Path::new("/Applications/Forge IDE.app")));
    }

    /// Written the way the Dock writes its own entries, which is what makes the
    /// duplicate check above able to recognise it afterwards.
    #[test]
    fn the_entry_matches_the_form_the_dock_uses() {
        let entry = tile_entry(Path::new("/Applications/Forge IDE.app"));
        assert!(
            entry.contains("<string>file:///Applications/Forge%20IDE.app/</string>"),
            "got {entry}",
        );
        assert!(entry.contains("<key>_CFURLStringType</key><integer>15</integer>"));

        // And the round trip closes: an entry this wrote is found by the check.
        let listed = format!(r#""_CFURLString" = "file:///Applications/Forge%20IDE.app/";"#);
        assert!(already_listed(&listed, Path::new("/Applications/Forge IDE.app")));
    }

    /// A path with characters XML or a URL would object to survives both, and
    /// still names the same application afterwards.
    #[test]
    fn an_awkward_path_is_encoded_not_injected() {
        let path = Path::new("/Apps/A & B <beta>.app");
        let entry = tile_entry(path);
        assert!(!entry.contains("A & B <beta>"), "raw markup left in: {entry}");
        assert!(entry.contains("%20") && entry.contains("%26"), "encoded: {entry}");

        // The encoded URL still decodes to the path it came from, which is what
        // the duplicate check relies on.
        let url = path_to_url(&path.to_string_lossy());
        assert_eq!(url_to_path(&url).trim_end_matches('/'), "/Apps/A & B <beta>.app");
    }
}
