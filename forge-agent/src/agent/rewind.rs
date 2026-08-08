// SPDX-License-Identifier: Apache-2.0
use anyhow::{anyhow, Context, Result};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct RewindCheckpoint {
    pub id: String,
    pub preview: String,
    pub message_count: usize,
    pub history_len: usize,
    pub log_offset: u64,
    pub keep_on_restore: bool,
    pub snapshot_commit: Option<String>,
    #[allow(dead_code)] // git ref name kept alongside snapshot_commit for future ref-based lookup
    pub snapshot_ref: Option<String>,
    pub git_base_head: Option<String>,
    pub git_stash_sha: Option<String>,
    pub worktree_snapshots: Vec<GitWorktreeSnapshot>,
    pub file_snapshots: Vec<FileSnapshot>,
}

#[derive(Debug, Clone)]
pub struct GitTurnSnapshot {
    pub commit: String,
    pub ref_name: String,
}

#[derive(Debug, Clone)]
pub struct GitWorktreeSnapshot {
    pub root: PathBuf,
    pub commit: String,
    pub ref_name: String,
}

#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub path: PathBuf,
    pub before_content: Option<String>,
    pub after_content: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct RewindFileStat {
    pub path: String,
    pub added: u32,
    pub removed: u32,
}

#[derive(Debug, Clone, Default)]
pub struct RewindDiffSummary {
    pub files: Vec<RewindFileStat>,
    pub total_added: u32,
    pub total_removed: u32,
}

pub fn restore_git_checkpoint(
    project_root: &Path,
    snapshot_commit: Option<&str>,
    git_base_head: Option<&str>,
    git_stash_sha: Option<&str>,
    worktree_snapshots: &[GitWorktreeSnapshot],
) -> Result<()> {
    if !worktree_snapshots.is_empty() {
        for snapshot in worktree_snapshots {
            restore_worktree_to_commit(&snapshot.root, &snapshot.commit)?;
        }
        return Ok(());
    }

    if !is_git_worktree(project_root) {
        return Ok(());
    }
    let _lock = RewindLock::acquire(project_root)?;

    if let Some(commit) = snapshot_commit.filter(|commit| !commit.trim().is_empty()) {
        git_output(project_root, &["read-tree", "--reset", "-u", commit])?;
    } else {
        match git_base_head {
            Some(head) if !head.trim().is_empty() => {
                git_output(project_root, &["read-tree", "--reset", "-u", head])?;
            }
            _ => {
                git_output(project_root, &["reset", "--hard"])?;
            }
        }
    }
    git_output(
        project_root,
        &["clean", "-fd", "-e", ".forge", "-e", ".agent"],
    )?;

    if let Some(sha) = git_stash_sha {
        if !sha.trim().is_empty() {
            git_output(project_root, &["stash", "apply", "--index", sha])
                .or_else(|_| git_output(project_root, &["stash", "apply", sha]))
                .with_context(|| format!("Failed to apply rewind snapshot {sha}"))?;
        }
    }

    Ok(())
}

pub fn restore_file_snapshots(file_snapshots: &[FileSnapshot]) -> Result<()> {
    for snapshot in file_snapshots.iter().rev() {
        match &snapshot.before_content {
            Some(content) => {
                if let Some(parent) = snapshot.path.parent() {
                    std::fs::create_dir_all(parent).with_context(|| {
                        format!(
                            "Failed to create parent directory for {}",
                            snapshot.path.display()
                        )
                    })?;
                }
                std::fs::write(&snapshot.path, content)
                    .with_context(|| format!("Failed to restore {}", snapshot.path.display()))?;
            }
            None => {
                if snapshot.path.exists() {
                    std::fs::remove_file(&snapshot.path)
                        .with_context(|| format!("Failed to remove {}", snapshot.path.display()))?;
                }
            }
        }
    }
    Ok(())
}

pub fn first_parent_commit(project_root: &Path, commit: &str) -> Result<Option<String>> {
    let out = git_output(project_root, &["rev-list", "--parents", "-n", "1", commit])?;
    let mut parts = out.split_whitespace();
    let _commit = parts.next();
    Ok(parts.next().map(str::to_string))
}

pub fn file_snapshot_diff_summary(file_snapshots: &[FileSnapshot]) -> RewindDiffSummary {
    let mut summary = RewindDiffSummary::default();
    for snapshot in file_snapshots {
        if snapshot.before_content == snapshot.after_content {
            continue;
        }
        let before = snapshot.before_content.as_deref().unwrap_or("");
        let after = snapshot.after_content.as_deref().unwrap_or("");
        let added = count_added_lines(before, after);
        let removed = count_added_lines(after, before);
        summary.total_added += added;
        summary.total_removed += removed;
        summary.files.push(RewindFileStat {
            path: snapshot.path.to_string_lossy().to_string(),
            added,
            removed,
        });
    }
    summary
}

pub fn merge_diff_summary(target: &mut RewindDiffSummary, extra: RewindDiffSummary) {
    target.total_added += extra.total_added;
    target.total_removed += extra.total_removed;
    target.files.extend(extra.files);
}

fn count_added_lines(old: &str, new: &str) -> u32 {
    let old_lines: std::collections::HashSet<&str> = old.lines().collect();
    new.lines()
        .filter(|line| !old_lines.contains(*line))
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

pub fn diff_summary(
    project_root: &Path,
    snapshot_commit: Option<&str>,
    git_base_head: Option<&str>,
    git_stash_sha: Option<&str>,
    worktree_snapshots: &[GitWorktreeSnapshot],
) -> Result<RewindDiffSummary> {
    if !worktree_snapshots.is_empty() {
        let mut summary = RewindDiffSummary::default();
        for snapshot in worktree_snapshots {
            let mut root_summary = diff_summary_for_args(
                &snapshot.root,
                &[
                    "diff",
                    "--numstat",
                    &snapshot.commit,
                    "--",
                    ".",
                    ":(exclude,top).forge",
                    ":(exclude,top).agent",
                ],
            )?;
            let root_label = snapshot
                .root
                .file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
                .unwrap_or_else(|| snapshot.root.to_string_lossy().to_string());
            for stat in &mut root_summary.files {
                stat.path = format!("{}/{}", root_label, stat.path);
            }
            summary.total_added += root_summary.total_added;
            summary.total_removed += root_summary.total_removed;
            summary.files.extend(root_summary.files);
        }
        return Ok(summary);
    }

    if !is_git_worktree(project_root) {
        return Ok(RewindDiffSummary::default());
    }

    let args = if let Some(commit) = snapshot_commit.filter(|commit| !commit.trim().is_empty()) {
        vec![
            "diff",
            "--numstat",
            commit,
            "--",
            ".",
            ":(exclude,top).forge",
            ":(exclude,top).agent",
        ]
    } else if let Some(sha) = git_stash_sha.filter(|sha| !sha.trim().is_empty()) {
        vec![
            "diff",
            "--numstat",
            sha,
            "--",
            ".",
            ":(exclude,top).forge",
            ":(exclude,top).agent",
        ]
    } else if let Some(head) = git_base_head.filter(|head| !head.trim().is_empty()) {
        vec![
            "diff",
            "--numstat",
            head,
            "--",
            ".",
            ":(exclude,top).forge",
            ":(exclude,top).agent",
        ]
    } else {
        vec![
            "diff",
            "--numstat",
            "HEAD",
            "--",
            ".",
            ":(exclude,top).forge",
            ":(exclude,top).agent",
        ]
    };

    diff_summary_for_args(project_root, &args)
}

fn diff_summary_for_args(project_root: &Path, args: &[&str]) -> Result<RewindDiffSummary> {
    let out = git_output(project_root, args)?;
    let mut summary = RewindDiffSummary::default();

    for line in out.lines() {
        let mut parts = line.splitn(3, '\t');
        let added = parts.next().unwrap_or("0");
        let removed = parts.next().unwrap_or("0");
        let path = parts.next().unwrap_or("").to_string();
        if path.is_empty() {
            continue;
        }

        let added = added.parse::<u32>().unwrap_or(0);
        let removed = removed.parse::<u32>().unwrap_or(0);
        summary.total_added += added;
        summary.total_removed += removed;
        summary.files.push(RewindFileStat {
            path,
            added,
            removed,
        });
    }

    let untracked = git_output(
        project_root,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            ".",
            ":(exclude,top).forge",
            ":(exclude,top).agent",
        ],
    )?;
    for path in untracked.lines().filter(|line| !line.trim().is_empty()) {
        if summary.files.iter().any(|stat| stat.path == path) {
            continue;
        }
        let line_count = std::fs::read_to_string(project_root.join(path))
            .map(|content| content.lines().count().max(1) as u32)
            .unwrap_or(0);
        summary.total_added += line_count;
        summary.files.push(RewindFileStat {
            path: path.to_string(),
            added: line_count,
            removed: 0,
        });
    }

    Ok(summary)
}

/// Ensures `project_root` is inside a git repository, initializing a fresh
/// one there if not — so rewind checkpoints have real git backing from the
/// very first turn that changes anything, instead of a project that was
/// never version controlled silently having nothing meaningful to restore
/// to. Only touches `project_root` itself, not any nested worktree a turn
/// might separately have touched (those are already skipped individually
/// when they aren't a git repo — see `create_turn_snapshots`).
/// Returns `Ok(true)` if a new repo was just created (so the caller can let
/// the user know), `Ok(false)` if one already existed.
pub fn ensure_git_repo(project_root: &Path) -> Result<bool> {
    if git_worktree_root_for_path(project_root).is_some() {
        return Ok(false);
    }
    // Never turn a home directory, a filesystem root, or a container of
    // unrelated trees into a repo — see `is_unsafe_git_root`. Returning
    // `Ok(false)` (rather than an error) keeps this quiet: the project simply
    // goes without git backing, exactly as it would for any other project we
    // can't initialize.
    if is_unsafe_git_root(project_root) {
        return Ok(false);
    }
    git_output(project_root, &["init"])?;
    Ok(true)
}

/// Directories forge must never treat as a project's git worktree, and never
/// `git init` into: a filesystem root, the user's home directory, and the
/// containers that homes and volumes are mounted under.
///
/// Two distinct hazards, both of which have actually happened:
///
///   * `git init` run on `$HOME` turns the user's entire home directory into
///     a repository they never asked for. Worse, it's silent and sticky —
///     from then on *every* project inside it that has no `.git` of its own
///     resolves its worktree root by walking *up* to `$HOME`, so forge stops
///     operating on the project entirely.
///
///   * Once that's happened (or the user genuinely does have a home-level
///     repo), every snapshot, restore and `git clean -fd` forge runs is
///     scoped to that root instead of the project. Restore is destructive;
///     a home directory is not somewhere to point it.
///
/// Reporting these as "not a worktree" makes the whole rewind/snapshot layer
/// no-op for such a project, which is the safe default: checkpoints fall back
/// to the per-file snapshots forge keeps regardless.
fn is_unsafe_git_root(path: &Path) -> bool {
    // Both forms matter. The literal path is what a caller passed; the
    // resolved one is what it actually points at — `/tmp` vs `/private/tmp`,
    // or a symlinked home, would otherwise slip past an equality check. macOS
    // firmlinks mean neither alone is enough: `/home` canonicalizes to
    // `/System/Volumes/Data/home`, so checking only the resolved path misses
    // the name every caller actually uses, and checking only the literal
    // misses the resolved one.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());

    // A filesystem root ("/", "C:\") has no parent.
    if path.parent().is_none() || resolved.parent().is_none() {
        return true;
    }

    if let Some(home) = dirs::home_dir() {
        let resolved_home = std::fs::canonicalize(&home).unwrap_or_else(|_| home.clone());
        if path == home || resolved == resolved_home {
            return true;
        }
    }

    // A repo at one of these levels spans every user, or every mount — never
    // a single project.
    const CONTAINERS: [&str; 5] = ["/Users", "/home", "/Volumes", "/mnt", "/media"];
    // The macOS data-volume firmlink prefix, so the resolved form of `/home`
    // is recognized as `/home` rather than something under `/System`.
    const MACOS_DATA_VOLUME: &str = "/System/Volumes/Data";

    [path, resolved.as_path()].iter().any(|candidate| {
        candidate
            .to_str()
            .map(|s| s.strip_prefix(MACOS_DATA_VOLUME).unwrap_or(s))
            .is_some_and(|s| CONTAINERS.contains(&s))
    })
}

pub fn git_worktree_root_for_path(path: &Path) -> Option<PathBuf> {
    let dir = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    git_output(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(|out| PathBuf::from(out.trim()))
        .filter(|path| !path.as_os_str().is_empty())
        // The single chokepoint every caller resolves a worktree root
        // through, so filtering here is what makes an unsafe root invisible
        // to snapshotting, restore and `is_git_worktree` alike.
        .filter(|root| !is_unsafe_git_root(root))
}

pub fn create_turn_snapshots(
    roots: &[PathBuf],
    session_id: &str,
    turn_id: &str,
    parent_for_root: impl Fn(&Path) -> Option<String>,
) -> Result<Vec<GitWorktreeSnapshot>> {
    let mut snapshots = Vec::new();
    for root in roots {
        if !is_git_worktree(root) {
            continue;
        }
        let parent = parent_for_root(root);
        if let Some(snapshot) = create_turn_snapshot(root, session_id, turn_id, parent.as_deref())?
        {
            snapshots.push(GitWorktreeSnapshot {
                root: canonical_worktree_root(root),
                commit: snapshot.commit,
                ref_name: snapshot.ref_name,
            });
        }
    }
    Ok(snapshots)
}

pub fn create_turn_snapshot(
    project_root: &Path,
    session_id: &str,
    turn_id: &str,
    parent_commit: Option<&str>,
) -> Result<Option<GitTurnSnapshot>> {
    if !is_git_worktree(project_root) {
        return Ok(None);
    }
    let _lock = RewindLock::acquire(project_root)?;

    let git_dir = git_output(project_root, &["rev-parse", "--git-dir"])?
        .trim()
        .to_string();
    let index_path = project_root
        .join(git_dir)
        .join(format!("forge-snapshot-{}.index", turn_id));
    let index_path_string = index_path.to_string_lossy().to_string();

    let head = git_output(project_root, &["rev-parse", "--verify", "HEAD"])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|out| !out.is_empty());

    if let Some(head) = head.as_deref() {
        git_output_with_env(
            project_root,
            &["read-tree", head],
            &[("GIT_INDEX_FILE", index_path_string.as_str())],
        )?;
    }

    // git add can fail when the project's .gitignore lists .forge / .agent
    // AND git decides our exclude pathspecs aren't enough to suppress the
    // "addIgnoredFile" check. Snapshotting is best-effort — if this fails,
    // skip the snapshot for this turn rather than scaring the user.
    let add_result = git_output_with_env(
        project_root,
        &[
            "add",
            "-A",
            "--",
            ".",
            ":(exclude,top).forge",
            ":(exclude,top).agent",
        ],
        &[("GIT_INDEX_FILE", index_path_string.as_str())],
    );
    if let Err(err) = add_result {
        let msg = err.to_string();
        // Recognized non-fatal cases: the user has .forge/.agent in their
        // .gitignore but the pathspec exclusion didn't fully suppress git's
        // safety check. Snapshot is skipped silently for this turn.
        if msg.contains("ignored by one of your .gitignore") || msg.contains("addIgnoredFile") {
            let _ = std::fs::remove_file(&index_path);
            return Ok(None);
        }
        return Err(err);
    }
    let tree = git_output_with_env(
        project_root,
        &["write-tree"],
        &[("GIT_INDEX_FILE", index_path_string.as_str())],
    )?
    .trim()
    .to_string();
    let _ = std::fs::remove_file(&index_path);

    let mut args = vec!["commit-tree", tree.as_str()];
    if let Some(parent) = parent_commit
        .filter(|parent| !parent.trim().is_empty())
        .or(head.as_deref())
    {
        args.push("-p");
        args.push(parent);
    }
    args.push("-m");
    args.push("forge rewind snapshot");
    let commit = git_output_with_env(
        project_root,
        &args,
        &[
            ("GIT_AUTHOR_NAME", "Forge"),
            ("GIT_AUTHOR_EMAIL", "forge@local"),
            ("GIT_COMMITTER_NAME", "Forge"),
            ("GIT_COMMITTER_EMAIL", "forge@local"),
        ],
    )?
    .trim()
    .to_string();

    let ref_name = format!(
        "refs/forge/rewind/{}/{}",
        sanitize_ref_component(session_id),
        sanitize_ref_component(turn_id)
    );
    git_output(
        project_root,
        &["update-ref", ref_name.as_str(), commit.as_str()],
    )?;

    Ok(Some(GitTurnSnapshot { commit, ref_name }))
}

/// Deliberately resolved through `git_worktree_root_for_path` rather than
/// asking git `--is-inside-work-tree` directly: that answers "is there a repo
/// somewhere above me", which is true for a project sitting inside an
/// accidental `$HOME` repo. Going through the shared resolver means the
/// `is_unsafe_git_root` filter applies here too, so every snapshot and
/// restore path gated on this function no-ops for such a project instead of
/// operating on the home directory.
fn is_git_worktree(project_root: &Path) -> bool {
    git_worktree_root_for_path(project_root).is_some()
}

fn canonical_worktree_root(project_root: &Path) -> PathBuf {
    git_worktree_root_for_path(project_root).unwrap_or_else(|| project_root.to_path_buf())
}

fn restore_worktree_to_commit(project_root: &Path, commit: &str) -> Result<()> {
    if !is_git_worktree(project_root) {
        return Ok(());
    }
    let _lock = RewindLock::acquire(project_root)?;
    git_output(project_root, &["read-tree", "--reset", "-u", commit])?;
    git_output(
        project_root,
        &["clean", "-fd", "-e", ".forge", "-e", ".agent"],
    )?;
    Ok(())
}

fn git_output(project_root: &Path, args: &[&str]) -> Result<String> {
    git_output_with_env(project_root, args, &[])
}

fn git_output_with_env(project_root: &Path, args: &[&str], env: &[(&str, &str)]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .envs(env.iter().copied())
        .current_dir(project_root)
        .output()
        .with_context(|| format!("Failed to run git {}", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(anyhow!(
            "git {} failed: {}{}",
            args.join(" "),
            stderr.trim(),
            if stdout.trim().is_empty() {
                String::new()
            } else {
                format!("\n{}", stdout.trim())
            }
        ));
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn sanitize_ref_component(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

struct RewindLock {
    path: std::path::PathBuf,
}

/// How long a lock file with nothing readable in it is still believed. A lock
/// is created and then written to, so one glimpsed in between is genuinely
/// held — but only for the instant that takes.
const UNWRITTEN_GRACE: Duration = Duration::from_secs(10);

/// Past this, a lock is treated as abandoned whatever it says. Snapshotting
/// takes seconds; nothing legitimately holds this for an hour. This is also
/// the only recovery on platforms where the owning process can't be checked.
const ABANDONED: Duration = Duration::from_secs(60 * 60);

impl RewindLock {
    fn acquire(project_root: &Path) -> Result<Self> {
        let forge_dir = project_root.join(".forge");
        std::fs::create_dir_all(&forge_dir)?;
        let path = forge_dir.join("rewind.lock");

        let held = || {
            anyhow!(
                "Another Forge session is snapshotting or rewinding this worktree ({})",
                path.display()
            )
        };

        match Self::create(&path) {
            Ok(lock) => return Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).context(held()),
        }

        // The lock is removed when the guard drops, which never happens if the
        // process is killed rather than asked to stop — and Reload Window kills
        // the agent by definition. A leftover file would otherwise refuse every
        // snapshot in that project for good, with nothing to point the user at
        // but a path to delete by hand.
        if !lock_is_stale(&std::fs::read_to_string(&path).unwrap_or_default(), lock_age(&path)) {
            return Err(held());
        }
        // Racing another process that decided the same thing is harmless: one
        // of them wins `create_new` and the other reports the lock as held,
        // which by then it is.
        let _ = std::fs::remove_file(&path);
        Self::create(&path).map_err(|_| held())
    }

    fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        writeln::write_pid(file)?;
        Ok(Self { path: path.to_path_buf() })
    }
}

/// How long ago the lock file was written, or zero if that can't be told —
/// the conservative answer, since age is only ever used to justify breaking it.
fn lock_age(path: &Path) -> Duration {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or_default()
}

/// Whether an existing lock is a leftover rather than a live claim.
fn lock_is_stale(contents: &str, age: Duration) -> bool {
    if age > ABANDONED {
        return true;
    }
    match contents.trim().parse::<i32>() {
        Ok(pid) => !process_exists(pid),
        // Nothing readable: either a lock from a forge old enough not to have
        // written a pid, or one whose owner died between creating the file and
        // writing to it.
        Err(_) => age > UNWRITTEN_GRACE,
    }
}

/// Whether a process with this id is still around. Signal 0 performs the
/// permission checks and finds the process without delivering anything;
/// `EPERM` means it exists and belongs to another user, which still counts.
#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } != 0 {
        return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }
    // A zombie answers signal 0 exactly like a running process — it still has a
    // process table entry, it just has nothing left to run. This is not a
    // corner case here: forge's agent is a child of the editor, so when the
    // agent dies it stays a zombie until the editor reaps it, and the editor
    // outliving its agent is the normal shape of the bug this whole check is
    // for. Without this, the lock would be honoured until the *editor* quit.
    !is_zombie(pid)
}

/// `ps` rather than a per-platform peek into the kernel's process table:
/// macOS wants `sysctl(KERN_PROC_PID)` and a `kinfo_proc`, Linux wants
/// `/proc/<pid>/stat`, and this runs only on the rare path where a lock is
/// already there. Anything unexpected is read as "not a zombie", which errs
/// towards respecting the lock.
#[cfg(unix)]
fn is_zombie(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim_start().starts_with('Z'))
        .unwrap_or(false)
}

/// No cheap equivalent here, so a lock is only ever broken by `ABANDONED`.
#[cfg(not(unix))]
fn process_exists(_pid: i32) -> bool {
    true
}

impl Drop for RewindLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

mod writeln {
    use std::fs::File;
    use std::io::Write;

    pub fn write_pid(mut file: File) -> std::io::Result<()> {
        writeln!(file, "{}", std::process::id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {} failed: {}\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }

    fn init_repo(path: &Path, name: &str) {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init"]);
        git(path, &["config", "user.email", "forge@test.local"]);
        git(path, &["config", "user.name", "Forge Test"]);
        std::fs::write(path.join("file.txt"), format!("{name} base\n")).unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-m", "base"]);
    }

    /// A pid that is definitely not running: a real child, reaped.
    fn dead_pid() -> i32 {
        let mut child = Command::new("true").spawn().expect("spawn");
        child.wait().expect("wait");
        child.id() as i32
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_stale() {
        // Reload Window kills the agent, so its guard never drops and the file
        // stays. Before this, that refused every snapshot in the project until
        // someone deleted it by hand.
        let pid = dead_pid();
        assert!(lock_is_stale(&format!("{pid}\n"), Duration::from_secs(1)));
    }

    #[test]
    fn a_lock_held_by_an_unreaped_child_is_stale() {
        // The exact shape seen in the wild: the editor's agent died, the editor
        // had not reaped it yet, and the leftover lock named a pid that
        // `kill(pid, 0)` still reported as alive.
        let mut child = Command::new("true").spawn().expect("spawn");
        // Wait for it to actually finish without reaping it, so it is a zombie
        // and not merely a process that has not got going yet.
        let pid = child.id() as i32;
        while !is_zombie(pid) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(lock_is_stale(&format!("{pid}\n"), Duration::from_secs(1)));
        child.wait().expect("reap");
    }

    #[test]
    fn a_lock_held_by_a_live_process_is_not_stale() {
        let mine = std::process::id();
        assert!(!lock_is_stale(&format!("{mine}\n"), Duration::from_secs(1)));
        assert!(!lock_is_stale(&format!("{mine}\n"), UNWRITTEN_GRACE * 10));
    }

    #[test]
    fn an_unwritten_lock_is_believed_only_briefly() {
        // The window between creating the file and writing the pid into it.
        assert!(!lock_is_stale("", Duration::from_secs(0)));
        assert!(lock_is_stale("", UNWRITTEN_GRACE + Duration::from_secs(1)));
    }

    #[test]
    fn a_lock_is_abandoned_eventually_whatever_it_says() {
        // The only recovery where the owning process cannot be checked, and a
        // backstop against a pid that has been recycled by something long-lived.
        let mine = std::process::id();
        assert!(lock_is_stale(&format!("{mine}"), ABANDONED + Duration::from_secs(1)));
    }

    #[test]
    fn acquire_takes_over_a_lock_whose_owner_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::create_dir_all(root.join(".forge")).unwrap();
        let lock = root.join(".forge/rewind.lock");
        std::fs::write(&lock, format!("{}\n", dead_pid())).unwrap();

        let guard = RewindLock::acquire(root).expect("stale lock should be taken over");
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap().trim(),
            std::process::id().to_string(),
            "the lock should now name us"
        );
        drop(guard);
        assert!(!lock.exists(), "dropping the guard should remove it");
    }

    #[test]
    fn acquire_refuses_a_lock_that_is_genuinely_held() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let _held = RewindLock::acquire(root).expect("first acquire");
        let err = match RewindLock::acquire(root) {
            Ok(_) => panic!("second acquire must fail while the first is held"),
            Err(e) => e,
        };
        assert!(
            err.to_string().contains("Another Forge session"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn multi_worktree_snapshots_restore_all_touched_repos() {
        let temp = tempfile::tempdir().unwrap();
        let repo_a = temp.path().join("repo-a");
        let repo_b = temp.path().join("repo-b");
        init_repo(&repo_a, "a");
        init_repo(&repo_b, "b");

        std::fs::write(repo_a.join("file.txt"), "a first\n").unwrap();
        std::fs::write(repo_b.join("file.txt"), "b first\n").unwrap();
        let first = create_turn_snapshots(
            &[repo_a.clone(), repo_b.clone()],
            "session",
            "turn-1",
            |_| None,
        )
        .unwrap();
        assert_eq!(first.len(), 2);

        std::fs::write(repo_a.join("file.txt"), "a second\n").unwrap();
        std::fs::write(repo_b.join("file.txt"), "b second\n").unwrap();

        for snapshot in &first {
            restore_worktree_to_commit(&snapshot.root, &snapshot.commit).unwrap();
        }

        assert_eq!(
            std::fs::read_to_string(repo_a.join("file.txt")).unwrap(),
            "a first\n"
        );
        assert_eq!(
            std::fs::read_to_string(repo_b.join("file.txt")).unwrap(),
            "b first\n"
        );
    }

    #[test]
    fn file_snapshots_restore_non_git_edits() {
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("existing.txt");
        let created = temp.path().join("created.txt");
        std::fs::write(&existing, "before\n").unwrap();

        let snapshots = vec![
            FileSnapshot {
                path: existing.clone(),
                before_content: Some("before\n".to_string()),
                after_content: Some("after\n".to_string()),
            },
            FileSnapshot {
                path: created.clone(),
                before_content: None,
                after_content: Some("new\n".to_string()),
            },
        ];

        std::fs::write(&existing, "after\n").unwrap();
        std::fs::write(&created, "new\n").unwrap();
        let summary = file_snapshot_diff_summary(&snapshots);
        assert_eq!(summary.files.len(), 2);

        restore_file_snapshots(&snapshots).unwrap();
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "before\n");
        assert!(!created.exists());
    }

    #[test]
    fn home_and_filesystem_roots_are_never_worktree_roots() {
        assert!(is_unsafe_git_root(Path::new("/")));
        assert!(is_unsafe_git_root(Path::new("/Users")));
        assert!(is_unsafe_git_root(Path::new("/home")));
        assert!(is_unsafe_git_root(Path::new("/Volumes")));
        if let Some(home) = dirs::home_dir() {
            assert!(
                is_unsafe_git_root(&home),
                "the home directory must never be treated as a project worktree root"
            );
        }

        let project = tempfile::tempdir().unwrap();
        assert!(
            !is_unsafe_git_root(project.path()),
            "an ordinary project directory must still be usable"
        );
    }

    #[test]
    fn ensure_git_repo_refuses_to_initialize_a_home_directory() {
        let Some(home) = dirs::home_dir() else { return };
        let had_git_before = home.join(".git").exists();

        assert!(
            !ensure_git_repo(&home).unwrap(),
            "must never report having initialized a home directory"
        );
        assert_eq!(
            home.join(".git").exists(),
            had_git_before,
            "ensure_git_repo must not create a repository in the home directory"
        );
    }

    #[test]
    fn ensure_git_repo_initializes_the_project_directory_itself() {
        let project = tempfile::tempdir().unwrap();

        assert!(ensure_git_repo(project.path()).unwrap());
        assert!(
            project.path().join(".git").exists(),
            "the repo belongs in the project directory, not anywhere above it"
        );
    }
}
