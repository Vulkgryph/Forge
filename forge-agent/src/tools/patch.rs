// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use std::path::Path;
use tempfile;

const MAX_PATCH_SIZE: usize = 512_000; // 500KB

const FORBIDDEN_PATHS: &[&str] = &[".git/", "target/", "node_modules/", "__pycache__/", ".env"];

fn validate_patch(unified_diff: &str) -> Result<()> {
    if unified_diff.len() > MAX_PATCH_SIZE {
        anyhow::bail!(
            "Patch too large ({} bytes, max {})",
            unified_diff.len(),
            MAX_PATCH_SIZE
        );
    }

    for line in unified_diff.lines() {
        if line.starts_with("+++ ") || line.starts_with("--- ") {
            let path = line[4..]
                .trim_start_matches("a/")
                .trim_start_matches("b/")
                .trim();
            for forbidden in FORBIDDEN_PATHS {
                if path.starts_with(forbidden) || path == forbidden.trim_end_matches('/') {
                    anyhow::bail!("Patch targets forbidden path: {}", path);
                }
            }
        }
    }

    Ok(())
}

pub async fn apply_patch(workspace_root: &str, unified_diff: &str) -> Result<()> {
    validate_patch(unified_diff)?;

    let workspace_path = Path::new(workspace_root);

    // First, try git apply if we're in a git repo
    if workspace_path.join(".git").exists() {
        let mut cmd = tokio::process::Command::new("git");
        cmd.args(&["apply", "--verbose", "--whitespace=nowarn"])
            .current_dir(workspace_root);

        // Create a temporary file to pass the patch content
        let temp_file = tempfile::NamedTempFile::new()?;
        std::fs::write(temp_file.path(), unified_diff)?;

        // For git apply, we need to provide the patch file as an argument
        let output = cmd
            .arg(temp_file.path())
            .output()
            .await
            .context("Failed to run git apply")?;

        if output.status.success() {
            return Ok(());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!(
            "git apply failed:\n{}\nHint: apply_patch requires a complete git-style unified diff with valid ---/+++ headers and @@ hunks. For small targeted changes, use edit_file with an exact unique old_string instead of retrying a partial patch.",
            stderr
        ));
    }

    // Outside a git repository, this tool does not work.
    //
    // It used to parse the diff's hunks here and then return an error
    // unconditionally — sixty lines of work thrown away, behind a comment
    // saying manual application was "not fully implemented". The behaviour was
    // the error; the parsing only made it look like something else was
    // intended. Failing immediately says the same thing sooner, and the
    // limitation is now written down where a user reads it rather than left in
    // a comment for whoever opens this file.
    //
    // Rarely reached in practice: the agent initialises a repository the first
    // time a turn changes anything in a project that has none, so that revert
    // checkpoints have git behind them. This is the path when even that failed
    // — no git binary, or a directory nobody may write to.
    let _ = unified_diff;
    Err(anyhow::anyhow!(
        "apply_patch needs a git repository: it applies patches with `git apply`, \
         and this workspace has no .git. Either initialise one, or use edit_file \
         with an exact unique old_string for the change you want."
    ))
}
