// SPDX-License-Identifier: Apache-2.0
//! `.forge/` inside somebody else's repository.
//!
//! Forge keeps working state next to the project it is working on: session
//! transcripts, plans, rewind snapshots, a search index. That is the right
//! place for it — the state belongs to this project and not to the machine —
//! but it means Forge is writing into a directory that is usually under
//! version control, and one that Forge does not own.
//!
//! Measured in a project the agent set up for itself, after a couple of
//! searches:
//!
//! ```text
//! $ git status --short
//! ?? .forge/
//!
//! $ find .forge -type f
//! .forge/search-index/                                    1.5 MB
//! .forge/sessions/20260915_140302_809/conversation.jsonl
//! ```
//!
//! So `git add .` in a user's own repository stages a multi-megabyte binary
//! index *and the conversation transcripts*, and on a public repository that
//! gets published. Nobody asked for either.
//!
//! The fix is for the directory to exclude itself. A `.gitignore` containing
//! `*` inside `.forge/` makes git ignore everything in it, including the
//! `.gitignore`, without touching a file the user maintains — editing their
//! root `.gitignore` would be Forge making a commit-shaped decision on their
//! behalf, and in a repository where that file is reviewed it would be a
//! surprise.
//!
//! Rewind already excludes `.forge` from its own snapshots by pathspec. That
//! covers Forge's own git operations and nothing else; this covers the user's.

use std::path::{Path, PathBuf};

/// The directory Forge keeps its per-project state in.
pub const DIR: &str = ".forge";

/// What goes in the self-excluding `.gitignore`.
///
/// `*` rather than a list: anything Forge writes here is Forge's own working
/// state, so a rule per file is a rule to forget to add. The trailing comment
/// is for whoever finds the file and wonders.
const IGNORE_ALL: &str = "# Forge's working state for this project — index, sessions, plans,\n\
                          # snapshots. Ignored wholesale so it is never committed by accident.\n\
                          *\n";

/// Create `.forge/` under `project_root` if needed, and make sure it excludes
/// itself from git.
///
/// Returns the directory. Failure to create it is an error; failure to write
/// the ignore file is not — the state is still usable, and refusing to search
/// because a `.gitignore` could not be written would be the wrong trade.
pub fn ensure(project_root: &Path) -> std::io::Result<PathBuf> {
    let dir = project_root.join(DIR);
    std::fs::create_dir_all(&dir)?;
    ensure_ignored(&dir);
    Ok(dir)
}

/// Create the parent of `path` as part of `.forge/`, ignore-file and all.
///
/// For the callers that know the file they want rather than the directory.
pub fn ensure_parent_of(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent)?;
    // The ignore belongs at the top of `.forge`, not beside every file inside
    // it, so walk up to the `.forge` component if this is under one.
    let mut at = Some(parent);
    while let Some(dir) = at {
        if dir.file_name().is_some_and(|n| n == DIR) {
            ensure_ignored(dir);
            return Ok(());
        }
        at = dir.parent();
    }
    Ok(())
}

/// Write the self-excluding `.gitignore` unless it is already there.
///
/// Never overwrites: someone may have edited it, and clobbering a file in
/// their repository on every run is worse than an out-of-date comment.
fn ensure_ignored(forge_dir: &Path) {
    let path = forge_dir.join(".gitignore");
    if path.exists() {
        return;
    }
    let _ = std::fs::write(path, IGNORE_ALL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forge-workdir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The whole point: a project the agent works in must not end up with
    /// Forge's index and conversation logs staged by `git add .`.
    #[test]
    fn the_directory_excludes_itself() {
        let root = temp("excludes");
        let dir = ensure(&root).unwrap();
        let ignore = dir.join(".gitignore");
        assert!(ignore.exists(), "no .gitignore was written");
        let body = std::fs::read_to_string(&ignore).unwrap();
        assert!(
            body.lines().any(|l| l.trim() == "*"),
            "the ignore does not exclude everything: {body:?}",
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Verified against git itself rather than by reading the file back,
    /// because the claim is about what git does and not about what was
    /// written. A rule that looked right and did not take would pass a
    /// content assertion.
    #[test]
    fn git_really_ignores_it() {
        let root = temp("git");
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            std::fs::remove_dir_all(&root).ok();
            return; // No git here; the content test above still applies.
        }
        let dir = ensure(&root).unwrap();
        std::fs::write(dir.join("search-index-stray"), b"binary").unwrap();
        std::fs::create_dir_all(dir.join("sessions/x")).unwrap();
        std::fs::write(dir.join("sessions/x/conversation.jsonl"), b"{}").unwrap();

        let out = std::process::Command::new("git")
            .args(["status", "--short"])
            .current_dir(&root)
            .output()
            .unwrap();
        let status = String::from_utf8_lossy(&out.stdout);
        assert!(
            !status.contains(".forge"),
            "git still sees .forge:\n{status}",
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// The ignore goes at the top of `.forge`, not beside whatever file the
    /// caller happened to be creating.
    #[test]
    fn a_nested_file_still_puts_the_ignore_at_the_top() {
        let root = temp("nested");
        let deep = root.join(DIR).join("sessions").join("abc").join("log.jsonl");
        ensure_parent_of(&deep).unwrap();
        assert!(root.join(DIR).join(".gitignore").exists(), "ignore not at the top");
        assert!(
            !deep.parent().unwrap().join(".gitignore").exists(),
            "an ignore was scattered next to the file",
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// An edited ignore file is left alone. Rewriting a file in someone's
    /// repository on every run is worse than a stale comment in it.
    #[test]
    fn an_existing_ignore_is_not_overwritten() {
        let root = temp("keep");
        let dir = ensure(&root).unwrap();
        std::fs::write(dir.join(".gitignore"), "# mine\n*\n").unwrap();
        ensure(&root).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join(".gitignore")).unwrap(), "# mine\n*\n");
        std::fs::remove_dir_all(&root).ok();
    }

    /// A path outside `.forge` gets its directory made and no ignore file,
    /// since this must not scatter `.gitignore`s around a project.
    #[test]
    fn paths_outside_the_forge_directory_get_no_ignore() {
        let root = temp("outside");
        let other = root.join("build").join("out.bin");
        ensure_parent_of(&other).unwrap();
        assert!(other.parent().unwrap().exists());
        assert!(!root.join("build").join(".gitignore").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
