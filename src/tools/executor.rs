// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use walkdir::WalkDir;

/// Result of spawning a shell command.
pub struct SpawnedCommand {
    pub child: tokio::process::Child,
    pub command: String,
    /// PTY master fd — present on Unix when PTY allocation succeeded.
    /// stdout and stderr are multiplexed here; write input here too.
    #[cfg(unix)]
    pub pty_master: Option<std::os::fd::OwnedFd>,
}

use super::custom::{load_custom_tools, CustomTool};
use super::patch;
use super::web;
use crate::api::ApiClient;

#[derive(Debug, Clone)]
pub struct TodoItem {
    pub text: String,
    pub status: String, // "pending", "in_progress", "done"
}

pub struct ToolExecutor {
    project_root: PathBuf,
    custom_tools: Vec<CustomTool>,
    todos: Mutex<Vec<TodoItem>>,
}

impl ToolExecutor {
    pub fn new(project_root: PathBuf) -> Self {
        Self {
            custom_tools: load_custom_tools(&project_root),
            project_root,
            todos: Mutex::new(Vec::new()),
        }
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn classify_tool(name: &str) -> ToolKind {
        match name {
            "read_file" | "list_directory" | "search_code" | "glob_files" | "todo_write"
            | "write_plan" | "exit_plan_mode" | "ask_question" => ToolKind::Read,
            "enter_plan_mode" => ToolKind::Execute,
            "apply_patch" => ToolKind::Write, // <-- FIX: Added apply_patch
            "write_file" | "edit_file" => ToolKind::Write,
            "web_search" | "web_fetch" => ToolKind::Execute,
            "shell_exec" | "delegate_task" => ToolKind::Execute,
            _ => ToolKind::Unknown,
        }
    }

    pub fn classify_tool_name(&self, name: &str) -> ToolKind {
        self.custom_tools
            .iter()
            .find(|tool| tool.name == name)
            .map(|tool| tool.kind)
            .unwrap_or_else(|| Self::classify_tool(name))
    }

    pub fn tool_definitions(&self) -> Vec<crate::api::types::ToolDefinition> {
        let mut tools = super::definitions::get_tool_definitions();
        tools.extend(self.custom_tools.iter().map(|tool| tool.definition()));
        tools
    }

    pub fn tool_definitions_filtered(
        &self,
        allowed: &[String],
    ) -> Vec<crate::api::types::ToolDefinition> {
        self.tool_definitions()
            .into_iter()
            .filter(|td| allowed.contains(&td.function.name))
            .collect()
    }

    pub fn plan_mode_tools(&self) -> Vec<crate::api::types::ToolDefinition> {
        super::definitions::get_plan_mode_tools()
    }

    pub fn toggleable_tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = super::definitions::get_toggleable_tool_names()
            .into_iter()
            .map(str::to_string)
            .collect();
        names.extend(self.custom_tools.iter().map(|tool| tool.name.clone()));
        names.sort();
        names.dedup();
        names
    }

    pub fn ensure_todo_item(&self, text: &str) -> usize {
        let mut todos = self.todos.lock().unwrap();
        if let Some((idx, _)) = todos
            .iter()
            .enumerate()
            .find(|(_, item)| item.text.eq_ignore_ascii_case(text))
        {
            return idx;
        }
        let index = todos.len();
        todos.push(TodoItem {
            text: text.to_string(),
            status: "pending".to_string(),
        });
        index
    }

    pub fn clear_todos(&self) {
        self.todos.lock().unwrap().clear();
    }

    pub async fn execute(
        &self,
        name: &str,
        args: &serde_json::Value,
        summarizer: Option<(&ApiClient, &str)>,
    ) -> Result<String> {
        match name {
            "read_file" => self.read_file(args).await,
            "list_directory" => self.list_directory(args).await,
            "search_code" => self.search_code(args).await,
            "write_file" => self.write_file(args).await,
            "edit_file" => self.edit_file(args).await,
            "apply_patch" => self.apply_patch(args).await,
            "shell_exec" => {
                // Shell exec is handled by the agent for streaming + cancellation.
                // Fallback for direct calls (e.g. subagents):
                self.run_command(args).await
            }
            "glob_files" => self.glob_files(args).await,
            "todo_write" => self.todo_write(args).await,
            "web_search" => web::web_search(args).await,
            "web_fetch" => web::web_fetch(args, summarizer).await,
            "project_overview" => self.project_overview().await,
            _ => {
                if let Some(tool) = self.custom_tools.iter().find(|tool| tool.name == name) {
                    self.run_custom_tool(tool, args).await
                } else {
                    Ok(format!("Unknown tool: {}", name))
                }
            }
        }
    }

    async fn run_custom_tool(&self, tool: &CustomTool, args: &serde_json::Value) -> Result<String> {
        use tokio::io::AsyncWriteExt;
        use tokio::process::Command;

        let args_json = serde_json::to_string(args)?;
        let mut command = Command::new(&tool.script_path);
        command
            .kill_on_drop(true)
            .current_dir(&self.project_root)
            .env("FORGE_PROJECT_ROOT", &self.project_root)
            .env("FORGE_WORKING_DIR", &self.project_root)
            .env("FORGE_TOOL_NAME", &tool.name)
            .env("FORGE_TOOL_ARGS", &args_json)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to run custom tool {}", tool.name))?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(args_json.as_bytes()).await?;
        }

        let output = match tokio::time::timeout(
            std::time::Duration::from_secs(tool.timeout_secs),
            child.wait_with_output(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Ok(format!(
                    "Tool error: custom tool {} timed out after {}s",
                    tool.name, tool.timeout_secs
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut result = String::new();
        if !stdout.trim().is_empty() {
            result.push_str(stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            if !result.is_empty() {
                result.push_str("\n\n");
            }
            result.push_str("stderr:\n");
            result.push_str(stderr.trim_end());
        }
        if result.is_empty() {
            result.push_str("(custom tool produced no output)");
        }
        if !output.status.success() {
            result = format!(
                "Tool error: custom tool {} exited with {}\n{}",
                tool.name, output.status, result
            );
        }
        Ok(result)
    }

    async fn read_file(&self, args: &serde_json::Value) -> Result<String> {
        let rel_path = args["path"].as_str().context("Missing 'path' argument")?;
        let full_path = self.resolve_path(rel_path)?;

        if !full_path.exists() {
            return Ok(format!("Error: File not found: {}", rel_path));
        }
        if !full_path.is_file() {
            return Ok(format!("Error: Not a file: {}", rel_path));
        }

        let content = std::fs::read_to_string(&full_path)
            .with_context(|| format!("Failed to read {}", rel_path))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        let requested_start = args
            .get("start_line")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let requested_end = args
            .get("end_line")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        if let (Some(start_line), Some(end_line)) = (requested_start, requested_end) {
            if start_line > end_line {
                return Ok(format!(
                    "Error: Invalid line range for {}: start_line ({}) is after end_line ({}).",
                    rel_path, start_line, end_line
                ));
            }
        }

        let start = requested_start
            .map(|v| v.saturating_sub(1))
            .unwrap_or(0)
            .min(total);
        let end = requested_end.unwrap_or(total).min(total);

        let mut output = format!("File: {} ({} lines total)\n", rel_path, total);
        if start > 0 || end < total {
            output.push_str(&format!("Showing lines {}-{}\n", start + 1, end));
        }
        output.push_str("---\n");

        for (i, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>5} | {}\n", start + i + 1, line));
        }

        Ok(output)
    }

    async fn list_directory(&self, args: &serde_json::Value) -> Result<String> {
        let rel_path = args["path"].as_str().unwrap_or(".");
        let max_depth = args.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(1) as usize;

        let full_path = self.resolve_path(rel_path)?;

        if !full_path.exists() {
            return Ok(format!("Error: Directory not found: {}", rel_path));
        }
        if !full_path.is_dir() {
            return Ok(format!("Error: Not a directory: {}", rel_path));
        }

        let mut output = format!("Directory: {}/\n---\n", rel_path);
        let mut entries: Vec<String> = Vec::new();

        for entry in WalkDir::new(&full_path)
            .max_depth(max_depth)
            .min_depth(1)
            .sort_by_file_name()
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            let name = entry
                .path()
                .strip_prefix(&full_path)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();

            if name.starts_with('.') && name != "." {
                continue;
            }

            if entry.file_type().is_dir() {
                let count = std::fs::read_dir(entry.path())
                    .map(|rd| rd.count())
                    .unwrap_or(0);
                entries.push(format!("  📁 {}/  ({} items)", name, count));
            } else {
                let size = entry
                    .metadata()
                    .map(|m| format_size(m.len()))
                    .unwrap_or_else(|_| "?".to_string());
                entries.push(format!("  📄 {}  ({})", name, size));
            }
        }

        if entries.is_empty() {
            output.push_str("  (empty directory)\n");
        } else {
            for entry in &entries {
                output.push_str(entry);
                output.push('\n');
            }
        }

        Ok(output)
    }

    async fn search_code(&self, args: &serde_json::Value) -> Result<String> {
        let query = args["query"].as_str().context("Missing 'query' argument")?;
        let search_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let file_pattern = args.get("file_pattern").and_then(|v| v.as_str());
        let fixed_string = args
            .get("fixed_string")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(30) as usize;

        let full_path = self.resolve_path(search_path)?;

        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--line-number")
            .arg("--no-heading")
            .arg("--color=never")
            .arg("--max-count=5")
            .arg(format!("--max-filesize=1M"));

        if fixed_string {
            cmd.arg("--fixed-strings");
        }

        if let Some(pattern) = file_pattern {
            cmd.arg("--glob").arg(pattern);
        }

        cmd.arg(query).arg(&full_path);

        let output = cmd.output().context("Failed to run ripgrep (rg)")?;
        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut results: Vec<String> = Vec::new();
        for line in stdout.lines().take(max_results) {
            let display_line = line
                .replace(full_path.to_str().unwrap_or(""), "")
                .trim_start_matches('/')
                .to_string();
            results.push(display_line);
        }

        if results.is_empty() {
            Ok(format!("No results found for: {}", query))
        } else {
            let mut out = format!(
                "Search results for '{}' ({} matches):\n---\n",
                query,
                results.len()
            );
            for r in &results {
                out.push_str(r);
                out.push('\n');
            }
            if stdout.lines().count() > max_results {
                out.push_str(&format!(
                    "\n... (truncated, showing {} of {} results)",
                    max_results,
                    stdout.lines().count()
                ));
            }
            Ok(out)
        }
    }

    async fn write_file(&self, args: &serde_json::Value) -> Result<String> {
        let rel_path = args["path"].as_str().context("Missing 'path' argument")?;
        let content = args["content"]
            .as_str()
            .context("Missing 'content' argument")?;
        let full_path = self.resolve_path(rel_path)?;

        if let Some(parent) = full_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let existed = full_path.exists();
        let old_content = if existed {
            std::fs::read_to_string(&full_path).ok()
        } else {
            None
        };

        std::fs::write(&full_path, content)?;

        if let Some(old) = old_content {
            // Return structured diff for updates
            Ok(format_edit_diff(rel_path, &old, content))
        } else {
            let line_count = content.lines().count();
            Ok(format!("WRITE:{}\n{} lines", rel_path, line_count))
        }
    }

    async fn edit_file(&self, args: &serde_json::Value) -> Result<String> {
        let rel_path = args["path"].as_str().context("Missing 'path' argument")?;
        let old_string = args["old_string"]
            .as_str()
            .context("Missing 'old_string' argument")?;
        let new_string = args["new_string"]
            .as_str()
            .context("Missing 'new_string' argument")?;
        let full_path = self.resolve_path(rel_path)?;

        if !full_path.exists() {
            return Ok(format!("Error: File not found: {}", rel_path));
        }

        let content = std::fs::read_to_string(&full_path)?;
        let count = content.matches(old_string).count();

        if count == 0 {
            return Ok(format!(
                "Error: Could not find the specified string in {}",
                rel_path
            ));
        }
        if count > 1 {
            return Ok(format!(
                "Error: Found {} occurrences of the string in {}. Must be unique.",
                count, rel_path
            ));
        }

        let new_content = content.replacen(old_string, new_string, 1);
        std::fs::write(&full_path, &new_content)?;

        Ok(format_edit_diff(rel_path, &content, &new_content))
    }

    async fn apply_patch(&self, args: &serde_json::Value) -> Result<String> {
        let diff = args
            .get("unified_diff")
            .or_else(|| args.get("diff"))
            .and_then(|v| v.as_str())
            .context("Missing 'unified_diff' argument")?;
        let workspace = self.project_root.to_string_lossy().to_string();

        // Read affected files before patching for diff display
        let file_path = extract_patch_file_path(diff);

        let old_content = if let Some(ref fp) = file_path {
            let full = self.resolve_path(fp)?;
            std::fs::read_to_string(&full).ok()
        } else {
            None
        };

        patch::apply_patch(&workspace, diff).await?;

        // Read the file after patching
        if let Some(ref fp) = file_path {
            let full = self.resolve_path(fp)?;
            if let Ok(new_content) = std::fs::read_to_string(&full) {
                if let Some(old) = old_content {
                    return Ok(format_edit_diff(fp, &old, &new_content));
                } else {
                    let line_count = new_content.lines().count();
                    return Ok(format!("WRITE:{}\n{} lines", fp, line_count));
                }
            }
        }

        // Fallback: parse diff directly for structured output
        Ok(format_patch_diff(diff))
    }

    /// Spawn a shell command with streaming I/O.
    /// On Unix, allocates a PTY so programs like sudo/ssh see a real terminal.
    /// Falls back to piped I/O on PTY allocation failure or on Windows.
    pub fn spawn_command(&self, args: &serde_json::Value) -> Result<SpawnedCommand> {
        let command = args["command"]
            .as_str()
            .context("Missing 'command' argument")?;
        let working_dir = args
            .get("working_dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".");
        let full_dir = self.resolve_path(working_dir)?;

        #[cfg(unix)]
        {
            if let Some(spawned) = self.try_spawn_pty(command, &full_dir) {
                return Ok(spawned);
            }
        }

        // Fallback: piped I/O (Windows, or if PTY allocation fails on Unix)
        let child = if std::env::consts::OS == "windows" {
            let cmd_exe = std::env::var("COMSPEC")
                .unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string());
            let mut cmd = tokio::process::Command::new(&cmd_exe);
            cmd.arg("/C")
                .arg(command)
                .current_dir(&full_dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            cmd.spawn().with_context(|| {
                format!(
                    "Failed to spawn cmd.exe (COMSPEC={}). Command: {}. Working dir: {}",
                    cmd_exe,
                    command,
                    full_dir.display()
                )
            })?
        } else {
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&full_dir)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .with_context(|| format!("Failed to spawn sh: {}", command))?
        };

        Ok(SpawnedCommand {
            child,
            command: command.to_string(),
            #[cfg(unix)]
            pty_master: None,
        })
    }

    #[cfg(unix)]
    fn try_spawn_pty(&self, command: &str, full_dir: &std::path::Path) -> Option<SpawnedCommand> {
        use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};

        // Allocate master/slave PTY pair
        let pty = nix::pty::openpty(None, None).ok()?;

        // Wrap in OwnedFd so the parent side is cleaned up automatically.
        let master_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(pty.master.into_raw_fd()) };
        let slave_fd: OwnedFd = unsafe { OwnedFd::from_raw_fd(pty.slave.into_raw_fd()) };
        let slave_raw = slave_fd.as_raw_fd();

        // Set FD_CLOEXEC on the original slave so it auto-closes in the child at exec.
        // The dup'd copies (for stdin/stdout/stderr) do NOT inherit CLOEXEC and survive exec.
        let _ = nix::fcntl::fcntl(
            slave_raw,
            nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
        );

        // Dup slave three times to get distinct Stdio handles for stdin/stdout/stderr.
        let dup_slave = || -> Option<std::process::Stdio> {
            nix::unistd::dup(slave_raw)
                .ok()
                .map(|fd| unsafe { std::process::Stdio::from_raw_fd(fd) })
        };
        let stdin_s = dup_slave()?;
        let stdout_s = dup_slave()?;
        let stderr_s = dup_slave()?;

        // Use tokio::process::Command (which DerefMut's to std::process::Command,
        // allowing pre_exec via std::os::unix::process::CommandExt).
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(full_dir)
            .stdin(stdin_s)
            .stdout(stdout_s)
            .stderr(stderr_s);

        // pre_exec runs in the child after fork, before exec.
        // slave_raw is accessible here (CLOEXEC only fires on exec, not fork).
        unsafe {
            cmd.pre_exec(move || {
                // New session → slave becomes the controlling terminal.
                nix::unistd::setsid()
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                // TIOCSCTTY: attach slave as controlling terminal.
                // Ignore error — may fail in containers/namespaces; programs still get a PTY.
                libc::ioctl(slave_raw, libc::TIOCSCTTY.into(), 0);
                Ok(())
            });
        }

        let child = cmd.spawn().ok()?;
        // slave_fd (OwnedFd) drops here → closes slave in parent.
        // Master now has the only remaining reference to the PTY.
        drop(slave_fd);

        Some(SpawnedCommand {
            child,
            command: command.to_string(),
            pty_master: Some(master_fd),
        })
    }

    async fn run_command(&self, args: &serde_json::Value) -> Result<String> {
        let command = args["command"]
            .as_str()
            .context("Missing 'command' argument")?;

        let working_dir = args
            .get("working_dir")
            .and_then(|v| v.as_str())
            .unwrap_or(".");

        let full_dir = self.resolve_path(working_dir)?;

        let output = if std::env::consts::OS == "windows" {
            let cmd_exe = std::env::var("COMSPEC")
                .unwrap_or_else(|_| r"C:\Windows\System32\cmd.exe".to_string());
            tokio::process::Command::new(&cmd_exe)
                .arg("/C")
                .arg(command)
                .current_dir(&full_dir)
                .output()
                .await
                .with_context(|| format!("Failed to execute with {}: {}", cmd_exe, command))?
        } else {
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(command)
                .current_dir(&full_dir)
                .output()
                .await
                .with_context(|| format!("Failed to execute: {}", command))?
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let exit_code = output.status.code().unwrap_or(-1);

        let mut result = format!("$ {}\nExit code: {}\n", command, exit_code);
        if !stdout.is_empty() {
            let truncated: String = stdout.chars().take(4000).collect();
            result.push_str(&format!("--- stdout ---\n{}\n", truncated));
            if stdout.len() > 4000 {
                result.push_str("... (truncated)\n");
            }
        }
        if !stderr.is_empty() {
            let truncated: String = stderr.chars().take(2000).collect();
            result.push_str(&format!("--- stderr ---\n{}\n", truncated));
        }

        Ok(result)
    }

    async fn project_overview(&self) -> Result<String> {
        let mut output = String::from("Project Overview\n===\n\n");

        // Count files by extension
        let mut ext_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let mut total_files = 0usize;
        let mut total_dirs = 0usize;

        for entry in WalkDir::new(&self.project_root)
            .max_depth(10)
            .into_iter()
            .filter_entry(|e| {
                let name = e.file_name().to_string_lossy();
                !name.starts_with('.')
                    && name != "node_modules"
                    && name != "target"
                    && name != "__pycache__"
                    && name != "venv"
                    && name != ".git"
            })
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if entry.file_type().is_dir() {
                total_dirs += 1;
            } else {
                total_files += 1;
                if let Some(ext) = entry.path().extension() {
                    *ext_counts
                        .entry(ext.to_string_lossy().to_string())
                        .or_insert(0) += 1;
                }
            }
        }

        output.push_str(&format!("Root: {}\n", self.project_root.display()));
        output.push_str(&format!(
            "Files: {}, Directories: {}\n\n",
            total_files, total_dirs
        ));

        // Sort by count descending
        let mut sorted_exts: Vec<_> = ext_counts.into_iter().collect();
        sorted_exts.sort_by(|a, b| b.1.cmp(&a.1));

        output.push_str("Languages/file types:\n");
        for (ext, count) in sorted_exts.iter().take(15) {
            output.push_str(&format!("  .{:<12} {} files\n", ext, count));
        }

        // Check for key files
        output.push_str("\nKey files detected:\n");
        let key_files = [
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "requirements.txt",
            "Makefile",
            "Dockerfile",
            "docker-compose.yml",
            "README.md",
            ".gitignore",
            "go.mod",
            "pom.xml",
            "build.gradle",
        ];
        for kf in &key_files {
            let path = self.project_root.join(kf);
            if path.exists() {
                output.push_str(&format!("  ✓ {}\n", kf));
            }
        }

        Ok(output)
    }

    async fn glob_files(&self, args: &serde_json::Value) -> Result<String> {
        let pattern = args["pattern"]
            .as_str()
            .context("Missing 'pattern' argument")?;
        let search_path = args.get("path").and_then(|v| v.as_str());

        let base_dir = match search_path {
            Some(p) => self.resolve_path(p)?,
            None => self.project_root.clone(),
        };

        if !base_dir.exists() {
            return Ok(format!(
                "Error: Directory not found: {}",
                base_dir.display()
            ));
        }

        let mut matches: Vec<String> = Vec::new();
        let max_results = 200;
        let max_duration = std::time::Duration::from_secs(8);
        let started_at = std::time::Instant::now();
        let matcher = match glob::Pattern::new(pattern) {
            Ok(pattern) => pattern,
            Err(e) => return Ok(format!("Error: Invalid glob pattern '{}': {}", pattern, e)),
        };
        let mut timed_out = false;
        let mut visited = 0usize;

        let should_descend = |entry: &walkdir::DirEntry| {
            if entry.depth() == 0 || !entry.file_type().is_dir() {
                return true;
            }
            let name = entry.file_name().to_string_lossy();
            !matches!(
                name.as_ref(),
                ".git"
                    | ".forge"
                    | "node_modules"
                    | "target"
                    | "dist"
                    | "build"
                    | ".next"
                    | ".cache"
                    | "__pycache__"
            )
        };

        for entry in WalkDir::new(&base_dir)
            .follow_links(false)
            .into_iter()
            .filter_entry(should_descend)
        {
            if matches.len() >= max_results {
                break;
            }
            if started_at.elapsed() >= max_duration {
                timed_out = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry.file_type().is_file() {
                continue;
            }
            visited += 1;
            let path = entry.path();
            let relative_to_base = path.strip_prefix(&base_dir).unwrap_or(path);
            if matcher.matches_path(relative_to_base) {
                let display = path
                    .strip_prefix(&self.project_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                matches.push(display);
            }
        }

        matches.sort();

        if matches.is_empty() {
            let mut output = format!("No files found matching: {}", pattern);
            if timed_out {
                output.push_str(&format!(
                    "\nSearch stopped after {}s; visited {} files. Try a narrower path.",
                    max_duration.as_secs(),
                    visited
                ));
            }
            Ok(output)
        } else {
            let total = matches.len();
            let mut output = format!("Found {} files matching '{}':\n", total, pattern);
            for m in &matches {
                output.push_str(m);
                output.push('\n');
            }
            if total >= max_results {
                output.push_str(&format!("\n(results capped at {})", max_results));
            }
            if timed_out {
                output.push_str(&format!(
                    "\nSearch stopped after {}s; visited {} files. Try a narrower path.",
                    max_duration.as_secs(),
                    visited
                ));
            }
            Ok(output)
        }
    }

    async fn todo_write(&self, args: &serde_json::Value) -> Result<String> {
        let action = args["action"]
            .as_str()
            .context("Missing 'action' argument")?;

        match action {
            "add" => {
                let text = args["text"]
                    .as_str()
                    .context("Missing 'text' for add action")?;
                let mut todos = self.todos.lock().unwrap();
                let index = todos.len();
                todos.push(TodoItem {
                    text: text.to_string(),
                    status: "pending".to_string(),
                });
                Ok(format!("Added todo [{}]: {}", index, text))
            }
            "update" => {
                let index = args["index"]
                    .as_u64()
                    .context("Missing 'index' for update action")?
                    as usize;
                let status = args["status"]
                    .as_str()
                    .context("Missing 'status' for update action")?;

                if !matches!(status, "pending" | "in_progress" | "done") {
                    return Ok(format!(
                        "Error: Invalid status '{}'. Must be pending, in_progress, or done.",
                        status
                    ));
                }

                let mut todos = self.todos.lock().unwrap();
                if index >= todos.len() {
                    return Ok(format!("Error: No todo at index {}", index));
                }
                todos[index].status = status.to_string();
                Ok(format!(
                    "Updated todo [{}] → {}: {}",
                    index, status, todos[index].text
                ))
            }
            "list" => {
                let todos = self.todos.lock().unwrap();
                if todos.is_empty() {
                    return Ok("No todos yet.".to_string());
                }
                let mut output = format!("Todos ({} items):\n", todos.len());
                for (i, item) in todos.iter().enumerate() {
                    let marker = match item.status.as_str() {
                        "done" => "[x]",
                        "in_progress" => "[~]",
                        _ => "[ ]",
                    };
                    output.push_str(&format!("  {} {} {}\n", marker, i, item.text));
                }
                Ok(output)
            }
            _ => Ok(format!(
                "Error: Unknown action '{}'. Use add, update, or list.",
                action
            )),
        }
    }

    pub fn resolve_path(&self, rel_path: &str) -> Result<PathBuf> {
        // Absolute paths are used as-is; relative paths resolve against project_root.
        // The agent can roam freely — project_root is just the default base, not a jail.
        let path = if Path::new(rel_path).is_absolute() {
            PathBuf::from(rel_path)
        } else {
            self.project_root.join(rel_path)
        };

        // Clean up the path (resolve . and .. without requiring existence)
        let mut cleaned = PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::ParentDir => {
                    cleaned.pop();
                }
                _ => {
                    cleaned.push(component);
                }
            }
        }

        Ok(cleaned)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Write,
    Execute,
    Unknown,
}

/// Extract the target file path from a unified diff (the +++ line).
fn extract_patch_file_path(diff: &str) -> Option<String> {
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim_start_matches("b/").trim();
            if !path.is_empty() && path != "/dev/null" {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Build a structured DIFF: result by comparing old and new file content.
/// Format: DIFF:{path}\n+{added} -{removed}\n{context and diff lines}
fn format_edit_diff(rel_path: &str, old_content: &str, new_content: &str) -> String {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    // Find the first and last differing lines
    let common_prefix = old_lines
        .iter()
        .zip(new_lines.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let old_suffix_start = old_lines.len();
    let new_suffix_start = new_lines.len();
    let common_suffix = old_lines
        .iter()
        .rev()
        .zip(new_lines.iter().rev())
        .take_while(|(a, b)| a == b)
        .take_while(|_| true)
        .count()
        .min(old_suffix_start - common_prefix)
        .min(new_suffix_start - common_prefix);

    let old_changed_end = old_suffix_start - common_suffix;
    let new_changed_end = new_suffix_start - common_suffix;

    let removed = old_changed_end - common_prefix;
    let added = new_changed_end - common_prefix;

    let mut output = format!("DIFF:{}\n+{} -{}\n", rel_path, added, removed);

    // Context lines before (up to 3)
    let ctx_start = common_prefix.saturating_sub(3);
    for i in ctx_start..common_prefix {
        output.push_str(&format!("  {:>4} {}\n", i + 1, new_lines[i]));
    }

    // Removed lines
    for i in common_prefix..old_changed_end {
        output.push_str(&format!("- {:>4} {}\n", i + 1, old_lines[i]));
    }

    // Added lines
    for i in common_prefix..new_changed_end {
        output.push_str(&format!("+ {:>4} {}\n", i + 1, new_lines[i]));
    }

    // Context lines after (up to 3)
    let ctx_end = (new_changed_end + 3).min(new_lines.len());
    for i in new_changed_end..ctx_end {
        output.push_str(&format!("  {:>4} {}\n", i + 1, new_lines[i]));
    }

    output
}

/// Build a structured DIFF: result from a raw unified diff string.
fn format_patch_diff(diff: &str) -> String {
    let file_path = extract_patch_file_path(diff).unwrap_or_else(|| "unknown".to_string());
    let mut added = 0usize;
    let mut removed = 0usize;
    let mut diff_lines = Vec::new();
    let mut in_hunk = false;

    for line in diff.lines() {
        if line.starts_with("@@ ") {
            in_hunk = true;
            continue;
        }
        if line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("diff ")
            || line.starts_with("index ")
        {
            continue;
        }
        if !in_hunk {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
            diff_lines.push(format!("+ {}", &line[1..]));
        } else if line.starts_with('-') {
            removed += 1;
            diff_lines.push(format!("- {}", &line[1..]));
        } else if line.starts_with(' ') {
            diff_lines.push(format!("  {}", &line[1..]));
        } else if line.starts_with('\\') {
            // "\ No newline at end of file" — skip
        } else {
            diff_lines.push(format!("  {}", line));
        }
    }

    let mut output = format!("DIFF:{}\n+{} -{}\n", file_path, added, removed);
    // Limit to 50 lines to avoid huge outputs
    for dl in diff_lines.iter().take(50) {
        output.push_str(dl);
        output.push('\n');
    }
    if diff_lines.len() > 50 {
        output.push_str("  ...(truncated)\n");
    }
    output
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{}B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::{ToolExecutor, ToolKind};
    use serde_json::json;

    #[tokio::test]
    async fn read_file_rejects_reversed_line_range() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("sample.txt"), "one\ntwo\nthree\n").expect("write sample");

        let executor = ToolExecutor::new(dir.path().to_path_buf());
        let result = executor
            .execute(
                "read_file",
                &json!({
                    "path": "sample.txt",
                    "start_line": 3,
                    "end_line": 2
                }),
                None,
            )
            .await
            .expect("read_file result");

        assert!(result.contains("Error: Invalid line range"));
        assert!(result.contains("start_line (3) is after end_line (2)"));
    }

    #[tokio::test]
    async fn custom_tool_loads_and_receives_json_args() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tools_dir = dir.path().join(".agent").join("tools");
        std::fs::create_dir_all(&tools_dir).expect("tools dir");
        std::fs::write(
            tools_dir.join("echo_custom.json"),
            r#"{
              "name": "echo_custom",
              "description": "Echo custom tool args",
              "kind": "read",
              "script": "echo_custom.sh",
              "parameters": {
                "type": "object",
                "properties": {
                  "message": { "type": "string" }
                },
                "required": ["message"]
              }
            }"#,
        )
        .expect("write tool config");
        std::fs::write(
            tools_dir.join("echo_custom.sh"),
            "#!/bin/sh\npython3 -c 'import json,sys; print(json.load(sys.stdin)[\"message\"])'\n",
        )
        .expect("write tool script");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = tools_dir.join("echo_custom.sh");
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(script, perms).unwrap();
        }

        let executor = ToolExecutor::new(dir.path().to_path_buf());
        assert_eq!(executor.classify_tool_name("echo_custom"), ToolKind::Read);
        assert!(executor
            .tool_definitions()
            .iter()
            .any(|tool| tool.function.name == "echo_custom"));

        let result = executor
            .execute("echo_custom", &json!({ "message": "hello custom" }), None)
            .await
            .expect("custom tool result");

        assert_eq!(result.trim(), "hello custom");
    }
}
