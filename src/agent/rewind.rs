// SPDX-License-Identifier: Apache-2.0
use anyhow::{anyhow, Context, Result};
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
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
        if msg.contains("ignored by one of your .gitignore")
            || msg.contains("addIgnoredFile")
        {
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

fn is_git_worktree(project_root: &Path) -> bool {
    git_output(project_root, &["rev-parse", "--is-inside-work-tree"])
        .map(|out| out.trim() == "true")
        .unwrap_or(false)
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

impl RewindLock {
    fn acquire(project_root: &Path) -> Result<Self> {
        let forge_dir = project_root.join(".forge");
        std::fs::create_dir_all(&forge_dir)?;
        let path = forge_dir.join("rewind.lock");
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| {
                format!(
                    "Another Forge session is snapshotting or rewinding this worktree ({})",
                    path.display()
                )
            })?;
        writeln::write_pid(file)?;
        Ok(Self { path })
    }
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
}
