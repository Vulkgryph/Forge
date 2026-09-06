// SPDX-License-Identifier: Apache-2.0
//! A working area the agent owns.
//!
//! Everything the agent writes otherwise lands in the user's workspace, so a
//! throwaway probe script, a scratch copy of a file, or a one-off test harness
//! either becomes litter in a real project or does not get written at all. This
//! gives it somewhere to put them: a per-session directory under the system
//! temporary directory that is created on demand, swept by age, and belongs to
//! no project.
//!
//! The point of keeping it outside the workspace is that nothing in here is the
//! user's. That is what makes it safe to let the agent write here without
//! asking, and it is also the limit of the guarantee — copying a file *out* of
//! the lab into a real directory is an ordinary write and is approved like one.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The directory the per-session labs live under.
pub fn base_dir() -> PathBuf {
    std::env::temp_dir().join("forge-lab")
}

/// One session's working area.
#[derive(Clone, Debug)]
pub struct Scratchpad {
    root: PathBuf,
}

impl Scratchpad {
    /// Creates the lab for `session_id`, or returns the error that stopped it.
    ///
    /// Called once per session. A failure here is not fatal anywhere — the
    /// agent simply carries on without a lab, exactly as it did before there
    /// was one — so the caller is free to ignore it.
    pub fn create(session_id: &str) -> std::io::Result<Self> {
        // The shared directory first, and checked before anything is written
        // through it — see `make_private`. On Linux this is usually `/tmp`,
        // which every account on the machine can write to.
        let base = base_dir();
        std::fs::create_dir_all(&base)?;
        make_private(&base)?;

        let root = base.join(sanitize(session_id));
        std::fs::create_dir_all(&root)?;
        make_private(&root)?;
        // Canonical from the start: on macOS the temporary directory lives
        // under `/var`, which is a symlink to `/private/var`, so a path derived
        // from `temp_dir()` and a path the agent actually wrote to can be the
        // same directory spelled two ways. `contains` would then say no.
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Ok(Self { root })
    }

    /// Wraps a directory that already exists. Test-only: a real lab is made by
    /// `create`, which also makes it private.
    #[cfg(test)]
    pub fn at(root: PathBuf) -> Self {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `candidate` is inside the lab.
    ///
    /// This is the whole security boundary of the write exemption, so it
    /// resolves the path first rather than comparing strings: `lab/../../etc/
    /// passwd` starts with the lab's path as text while naming somewhere else
    /// entirely. Comparison is by path component, so a sibling directory whose
    /// name merely begins the same way — `forge-lab-old` next to `forge-lab` —
    /// is not inside it either.
    pub fn contains(&self, candidate: &Path) -> bool {
        let candidate = resolve(candidate);
        candidate.starts_with(&self.root)
    }
}

/// Removes labs whose contents have not been touched for `keep`.
///
/// Sessions end in every way a process can, so a lab that is deleted only on a
/// clean exit is a lab that accumulates. Age is the one signal that survives a
/// crash, a kill, and a reboot. `now` is a parameter so the sweep can be tested
/// without waiting a week.
///
/// Returns how many were removed. Anything it cannot read or delete is left
/// alone: this runs at startup, and failing to tidy up is never a reason to
/// refuse to start.
pub fn sweep(base: &Path, keep: Duration, now: SystemTime) -> usize {
    let Ok(entries) = std::fs::read_dir(base) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        // `symlink_metadata`, so a symlink pointing at a real directory is not
        // followed and then recursively deleted.
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if !meta.is_dir() {
            continue;
        }
        // Never delete a directory that is not ours. On a shared `/tmp` the
        // sweep would otherwise be a way to remove other people's files.
        #[cfg(unix)]
        if !owned_by_us(&meta) {
            continue;
        }
        let age = newest_mtime(&entry.path(), meta.modified().ok())
            .and_then(|t| now.duration_since(t).ok());
        if age.is_some_and(|age| age >= keep) && std::fs::remove_dir_all(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// The most recent modification time anywhere in `dir`.
///
/// A directory's own mtime only changes when an entry is added or removed, so a
/// lab being actively written to all week can still look a week old. Asking its
/// contents is what makes "not touched for seven days" mean what it says.
fn newest_mtime(dir: &Path, own: Option<SystemTime>) -> Option<SystemTime> {
    let mut newest = own;
    let mut stack = vec![dir.to_path_buf()];
    // Bounded: a lab with a huge tree in it should not turn startup into a
    // full filesystem walk. Anything deeper keeps the times found so far, which
    // can only make the directory look younger — erring towards keeping it.
    let mut budget = 4096;
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            if budget == 0 {
                return newest;
            }
            budget -= 1;
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if let Ok(t) = meta.modified() {
                if newest.is_none_or(|n| t > n) {
                    newest = Some(t);
                }
            }
            if meta.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    newest
}

/// Makes a directory the current user's alone, and refuses one that is not.
///
/// This matters on Linux and nowhere else in practice. macOS gives each account
/// its own temporary directory under `/var/folders`, and Windows puts it in the
/// user's profile, but on Linux `TMPDIR` is usually unset and `std::env::temp_dir`
/// answers `/tmp` — a directory shared with every other account on the machine.
///
/// Two things follow from that, and both are worse here than they would be for
/// an ordinary temporary file, because writes into this directory are approved
/// without asking:
///
/// * Default permissions would leave the agent's scratch work — which routinely
///   contains pieces of the user's private code — readable by anyone logged in.
/// * If someone else creates `/tmp/forge-lab` first as a symlink, `create_dir_all`
///   follows it, and the agent's auto-approved writes land wherever it points.
///
/// So the directory is narrowed to `0700`, and one that is a symlink or belongs
/// to another user is refused outright, which leaves the session with no lab
/// rather than an unsafe one. There is an unavoidable window between the check
/// and the use; this raises the cost of the attack rather than closing it, and
/// the sweep does the same check before deleting anything.
#[cfg(unix)]
fn make_private(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let meta = path.symlink_metadata()?;
    if meta.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "{} is a symlink, not a directory",
            path.display()
        )));
    }
    if !owned_by_us(&meta) {
        return Err(std::io::Error::other(format!(
            "{} belongs to another user",
            path.display()
        )));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn make_private(path: &Path) -> std::io::Result<()> {
    // Windows puts the temporary directory inside the user's own profile, so it
    // is already private; there is no shared-`/tmp` equivalent to defend
    // against. The symlink check still earns its place.
    if path.symlink_metadata()?.file_type().is_symlink() {
        return Err(std::io::Error::other(format!(
            "{} is a symlink, not a directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn owned_by_us(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: `getuid` takes no arguments, touches no memory, and cannot fail.
    meta.uid() == unsafe { libc::getuid() }
}

/// A session id as a single safe directory name.
fn sanitize(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect();
    if cleaned.is_empty() { "session".to_string() } else { cleaned }
}

/// Resolves a path as far as the filesystem allows, then lexically.
///
/// `canonicalize` alone is not enough: the path being checked is usually a file
/// that does not exist yet, which is an error rather than an answer. So the
/// deepest part that does exist is canonicalized — picking up symlinks and the
/// real spelling of the temporary directory — and what remains is appended with
/// `.` and `..` folded away.
fn resolve(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(real) = std::fs::canonicalize(&existing) {
            let mut out = real;
            for part in rest.iter().rev() {
                out.push(part);
            }
            return lexical(&out);
        }
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                existing = parent.to_path_buf();
            }
            _ => return lexical(path),
        }
    }
}

/// Folds `.` and `..` away without touching the filesystem.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                // Never climb past the root: `/..` is `/`.
                if !matches!(out.components().next_back(), Some(Component::RootDir) | None) {
                    out.pop();
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forge-lab-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn a_file_in_the_lab_is_in_the_lab() {
        let base = scratch("contains");
        let pad = Scratchpad::at(base.clone());
        assert!(pad.contains(&base.join("probe.py")));
        assert!(pad.contains(&base.join("nested/deeper/notes.md")));
        assert!(pad.contains(&base));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The exemption is only as good as this: a path that merely *starts with*
    /// the lab's name as text can name anywhere on the disk.
    #[test]
    fn climbing_out_of_the_lab_is_not_in_the_lab() {
        let base = scratch("escape");
        let pad = Scratchpad::at(base.clone());

        assert!(!pad.contains(&base.join("../../etc/passwd")));
        assert!(!pad.contains(&base.join("..")));
        assert!(!pad.contains(Path::new("/etc/passwd")));
        // A sibling whose name begins the same way — string prefixes say yes.
        let sibling = base.with_file_name(format!(
            "{}-old",
            base.file_name().unwrap().to_string_lossy()
        ));
        assert!(!pad.contains(&sibling.join("secrets")));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Descending back in after climbing out is still in.
    #[test]
    fn a_path_that_leaves_and_returns_is_in_the_lab() {
        let base = scratch("round-trip");
        let pad = Scratchpad::at(base.clone());
        let name = base.file_name().unwrap().to_string_lossy().to_string();
        assert!(pad.contains(&base.join("..").join(&name).join("probe.py")));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn old_labs_are_swept_and_recent_ones_are_kept() {
        let base = scratch("sweep");
        std::fs::create_dir_all(base.join("session-a")).unwrap();
        std::fs::write(base.join("session-a/probe.py"), b"print()").unwrap();
        std::fs::create_dir_all(base.join("session-b")).unwrap();

        // Nothing is a week old yet.
        assert_eq!(sweep(&base, Duration::from_secs(7 * 86400), SystemTime::now()), 0);
        assert!(base.join("session-a").is_dir());

        // Seen from eight days in the future, both are.
        let later = SystemTime::now() + Duration::from_secs(8 * 86400);
        assert_eq!(sweep(&base, Duration::from_secs(7 * 86400), later), 2);
        assert!(!base.join("session-a").exists());
        assert!(!base.join("session-b").exists());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A lab written to all week is not a week old, even though its own
    /// directory mtime has not moved since the day it was made.
    #[test]
    fn a_lab_still_being_used_is_not_swept() {
        let base = scratch("still-used");
        let lab = base.join("session-a");
        std::fs::create_dir_all(lab.join("deep")).unwrap();
        std::fs::write(lab.join("deep/notes.md"), b"in use").unwrap();

        // The newest file was written now, so from just over the threshold the
        // lab is still young enough to keep.
        let later = SystemTime::now() + Duration::from_secs(3 * 86400);
        assert_eq!(sweep(&base, Duration::from_secs(7 * 86400), later), 0);
        assert!(lab.is_dir());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The lab is made on demand, lives under the shared base directory, and
    /// recognises its own contents.
    #[test]
    fn creating_a_lab_makes_a_usable_directory() {
        let id = format!("test-create-{}", std::process::id());
        let pad = Scratchpad::create(&id).expect("create");

        assert!(pad.root().is_dir(), "not created: {}", pad.root().display());
        assert!(
            resolve(pad.root()).starts_with(resolve(&base_dir())),
            "lab is outside the base directory: {}",
            pad.root().display()
        );
        assert!(pad.contains(&pad.root().join("probe.py")));

        // Writing into it works, which is the entire point.
        std::fs::write(pad.root().join("probe.py"), b"print()").unwrap();
        assert!(pad.root().join("probe.py").is_file());

        // Asking twice for the same session gives the same lab back rather
        // than failing on the directory already being there.
        let again = Scratchpad::create(&id).expect("second create");
        assert_eq!(again.root(), pad.root());

        let _ = std::fs::remove_dir_all(pad.root());
    }

    /// On Linux the base directory is usually `/tmp`, shared with every other
    /// account on the machine. The agent's scratch work routinely contains
    /// pieces of the user's private code, and writes here are auto-approved.
    #[cfg(unix)]
    #[test]
    fn the_lab_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;
        let id = format!("test-perms-{}", std::process::id());
        let pad = Scratchpad::create(&id).expect("create");

        let mode = pad.root().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "lab is {mode:o}, not private");
        let base_mode = base_dir().metadata().unwrap().permissions().mode() & 0o777;
        assert_eq!(base_mode, 0o700, "base directory is {base_mode:o}, not private");

        let _ = std::fs::remove_dir_all(pad.root());
    }

    /// Someone else creating the base directory first, as a symlink, would
    /// otherwise redirect every auto-approved write the agent makes.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_directory_is_refused() {
        let tmp = scratch("symlink-base");
        let elsewhere = tmp.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let link = tmp.join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        let err = make_private(&link).expect_err("a symlink must be refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        // A real directory in the same place is fine.
        assert!(make_private(&elsewhere).is_ok());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn a_session_id_becomes_one_safe_directory_name() {
        assert_eq!(sanitize("2026-09-06T12:00:00Z"), "2026-09-06T12-00-00Z");
        assert_eq!(sanitize("../../etc"), "------etc");
        assert_eq!(sanitize(""), "session");
    }
}
