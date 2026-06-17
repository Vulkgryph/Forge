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

    // Fallback to manual patch application for non-git repos
    // Parse the unified diff manually
    parse_and_apply_diff(workspace_root, unified_diff)
}

fn parse_and_apply_diff(workspace_root: &str, unified_diff: &str) -> Result<()> {
    _ = Path::new(workspace_root);

    // Parse unified diff manually
    // Format: --- old_file\n+++ new_file\n@@ line info @@\n lines
    let lines: Vec<&str> = unified_diff.lines().collect();

    let mut current_file: Option<String> = None;
    let mut in_hunk = false;
    let mut hunk_lines: Vec<String> = Vec::new();

    for line in lines {
        if line.starts_with("--- ") {
            // Start of a new file patch
            current_file = None;
            in_hunk = false;
            hunk_lines.clear();

            // Extract filename from --- a/path or --- path
            if let Some(path) = line.strip_prefix("--- ") {
                let path = path.trim_start_matches("a/").trim();
                current_file = Some(path.to_string());
            }
        } else if line.starts_with("+++ ") {
            // Get the new filename
            if let Some(path) = line.strip_prefix("+++ ") {
                let path = path.trim_start_matches("b/").trim();
                if let Some(ref old_file) = current_file {
                    if old_file != path {
                        // File was renamed
                    }
                }
                current_file = Some(path.to_string());
            }
        } else if line.starts_with("@@ ") {
            // Start of a hunk
            in_hunk = true;
            hunk_lines.clear();
        } else if in_hunk {
            if line.starts_with("+") {
                // Added line
                hunk_lines.push(line[1..].to_string());
            } else if line.starts_with("-") {
                // Removed line - skip (we don't implement deletion)
                hunk_lines.push(line[1..].to_string());
            } else if line.starts_with("\\") {
                // No newline at end
                continue;
            } else {
                // Context line
                hunk_lines.push(line[1..].to_string());
            }
        }
    }

    // For now, just return an error saying manual patching isn't fully implemented
    // In production, you'd want to properly implement this
    Err(anyhow::anyhow!(
        "Manual patch application not fully implemented. Use a git repo for reliable patching."
    ))
}

