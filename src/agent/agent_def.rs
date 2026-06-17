// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct AgentDefinition {
    pub name: String,
    pub description: String,
    pub tools: Vec<String>,
    pub model: AgentModel,
    pub max_turns: Option<usize>,
    pub system_prompt: String,
    pub source: AgentDefSource,
}

#[derive(Debug, Clone)]
pub enum AgentModel {
    Inherit,
    Named(String),
}

#[derive(Debug, Clone)]
pub enum AgentDefSource {
    BuiltIn,
    ProjectFile(PathBuf),
    GlobalFile(PathBuf),
}

fn builtin_agents() -> Vec<AgentDefinition> {
    vec![
        AgentDefinition {
            name: "explore".to_string(),
            description: "Read-only codebase exploration and code search".to_string(),
            tools: vec![
                "read_file".to_string(),
                "list_directory".to_string(),
                "search_code".to_string(),
                "glob_files".to_string(),
            ],
            model: AgentModel::Inherit,
            max_turns: None,
            system_prompt: r#"You are a codebase exploration agent with read-only access.

Your workflow:
1. Start broad: use list_directory to understand the project structure
2. Search for patterns: use search_code to find relevant code across the codebase
3. Read key files: use read_file to examine important files in detail
4. Follow references: when you find something interesting, trace its usage and dependencies

Strategy:
- Don't just read one file and stop. Explore multiple related files to build a complete picture.
- When searching, try multiple query patterns if the first doesn't find what you need.
- Read configuration files, entry points, and module definitions to understand architecture.
- Follow imports and function calls to understand how components connect.

Your final summary MUST include:
- Specific file paths and line numbers for every claim you make
- Code structure and how components relate to each other
- Direct answers to what was asked — not just a list of files you looked at
- Any patterns, conventions, or notable design decisions you observed

Format your output with clear sections and bullet points. Be comprehensive but organized."#.to_string(),
            source: AgentDefSource::BuiltIn,
        },
        AgentDefinition {
            name: "bash".to_string(),
            description: "Command execution specialist for running shell commands".to_string(),
            tools: vec![
                "shell_exec".to_string(),
                "read_file".to_string(),
            ],
            model: AgentModel::Inherit,
            max_turns: None,
            system_prompt: r#"You are a command execution agent.

Your workflow:
1. Understand the task and what commands are needed
2. Read relevant files for context if needed (Makefiles, package.json, Cargo.toml, etc.)
3. Run commands one at a time, checking output before proceeding
4. If a command fails, diagnose the error and try to fix it

Guidelines:
- Always check exit codes and stderr output
- For build/test commands, read the project config first to know the right invocation
- NEVER use shell_exec to create, write, or modify files. Do NOT use cat, echo, sed, awk, tee, printf, or heredocs to write file content. If you need to edit files, you must request those tools via tools_override.
- Don't run destructive commands (rm -rf, git push --force) without extreme caution
- If a command produces too much output, use flags to limit it (e.g. --quiet, head, tail)
- Chain related commands logically — don't run tests before building

Your final summary MUST include:
- Every command you ran and its exit code
- Key output from each command (not the full dump — just the important parts)
- Whether the task succeeded or failed, and why
- Any follow-up actions needed"#.to_string(),
            source: AgentDefSource::BuiltIn,
        },
        AgentDefinition {
            name: "plan".to_string(),
            description: "Software architect for designing implementation plans".to_string(),
            tools: vec![
                "read_file".to_string(),
                "list_directory".to_string(),
                "search_code".to_string(),
                "glob_files".to_string(),
            ],
            model: AgentModel::Inherit,
            max_turns: None,
            system_prompt: r#"You are a software architecture and planning agent.

Your workflow:
1. Understand the goal: what feature, fix, or refactor is being requested?
2. Explore the codebase structure: list directories, read key files
3. Find existing patterns: search for similar features already implemented
4. Identify all files that need to change
5. Design the implementation approach

Guidelines:
- Read the actual code, don't guess at the architecture
- Look for existing patterns and conventions — the plan should follow them
- Consider edge cases, error handling, and backwards compatibility
- Think about what could go wrong and flag risks
- Keep the plan actionable — another agent should be able to follow it step by step

Your final summary MUST be a structured plan with:
- **Goal**: One-sentence description of what we're building/fixing
- **Files to modify**: List of files with what changes each needs
- **New files** (if any): What they contain and why they're needed
- **Implementation steps**: Numbered, ordered steps with enough detail to execute
- **Dependencies**: What needs to happen before what
- **Risks/considerations**: Potential issues to watch for
- **Verification**: How to confirm the changes work (tests, manual checks)"#.to_string(),
            source: AgentDefSource::BuiltIn,
        },
        AgentDefinition {
            name: "general".to_string(),
            description: "General-purpose agent with full tool access".to_string(),
            tools: vec![
                "read_file".to_string(),
                "list_directory".to_string(),
                "search_code".to_string(),
                "glob_files".to_string(),
                "apply_patch".to_string(),
                "shell_exec".to_string(),
                "todo_write".to_string(),
                "web_search".to_string(),
                "web_fetch".to_string(),
            ],
            model: AgentModel::Inherit,
            max_turns: None,
            system_prompt: r#"You are a general-purpose coding agent with full tool access.

Your workflow:
1. Understand the task fully before making any changes
2. Read relevant files to understand the current state
3. Make targeted changes using apply_patch
4. Verify your changes by reading the modified files or running tests

Guidelines:
- ALWAYS read a file before editing it — never guess at its contents
- Make small, focused changes. Don't rewrite entire files when a targeted edit suffices.
- NEVER use shell_exec to create, write, or modify files. Do NOT use cat, echo, sed, awk, tee, printf, or heredocs. Always use apply_patch (preferred), edit_file, or write_file.
- After applying patches, read the file to confirm the edit was correct
- Run tests or build commands to verify nothing is broken
- If something fails, diagnose the error before retrying

Your final summary MUST include:
- What changes you made and why (with file paths)
- Whether verification (tests/build) passed
- Any issues encountered and how you resolved them
- Anything the parent agent should know or follow up on"#.to_string(),
            source: AgentDefSource::BuiltIn,
        },
    ]
}

/// Parse a markdown file with frontmatter into an AgentDefinition.
///
/// Format:
/// ```
/// ---
/// name: explore
/// description: Read-only codebase exploration
/// tools: [read_file, list_directory, search_code]
/// model: inherit
/// max_turns: 20
/// ---
/// System prompt body here...
/// ```
fn parse_agent_file(content: &str, source: AgentDefSource) -> Result<AgentDefinition> {
    let content = content.trim();
    if !content.starts_with("---") {
        anyhow::bail!("Agent definition must start with --- frontmatter delimiter");
    }

    let after_first = &content[3..];
    let end_idx = after_first
        .find("---")
        .context("Missing closing --- frontmatter delimiter")?;

    let frontmatter = &after_first[..end_idx];
    let body = after_first[end_idx + 3..].trim();

    let mut name = String::new();
    let mut description = String::new();
    let mut tools: Vec<String> = Vec::new();
    let mut model = AgentModel::Inherit;
    let mut max_turns: Option<usize> = None;

    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            match key {
                "name" => name = value.to_string(),
                "description" => description = value.to_string(),
                "tools" => {
                    // Parse [tool1, tool2, tool3]
                    let inner = value.trim_start_matches('[').trim_end_matches(']');
                    tools = inner
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
                "model" => {
                    model = if value == "inherit" {
                        AgentModel::Inherit
                    } else {
                        AgentModel::Named(value.to_string())
                    };
                }
                "max_turns" => {
                    if let Ok(n) = value.parse::<usize>() {
                        max_turns = Some(n);
                    }
                }
                _ => {} // ignore unknown fields
            }
        }
    }

    if name.is_empty() {
        anyhow::bail!("Agent definition missing 'name' field");
    }

    Ok(AgentDefinition {
        name,
        description,
        tools,
        model,
        max_turns,
        system_prompt: if body.is_empty() {
            "You are a helpful coding agent.".to_string()
        } else {
            body.to_string()
        },
        source,
    })
}

fn load_agents_from_dir(
    dir: &Path,
    source_fn: fn(PathBuf) -> AgentDefSource,
) -> Vec<AgentDefinition> {
    let mut agents = Vec::new();
    if !dir.is_dir() {
        return agents;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return agents,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().map_or(false, |ext| ext == "md") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                match parse_agent_file(&content, source_fn(path.clone())) {
                    Ok(def) => agents.push(def),
                    Err(e) => {
                        eprintln!(
                            "Warning: failed to parse agent file {}: {}",
                            path.display(),
                            e
                        );
                    }
                }
            }
        }
    }

    agents
}

/// Load agent definitions with precedence: project-local > global > built-in.
/// Later entries with the same name override earlier ones.
pub fn load_agent_definitions(project_root: &Path) -> Result<Vec<AgentDefinition>> {
    let mut defs_by_name = std::collections::HashMap::new();

    // 1. Start with built-in defaults
    for def in builtin_agents() {
        defs_by_name.insert(def.name.clone(), def);
    }

    // 2. Global agents override built-in
    if let Some(home) = dirs::home_dir() {
        let global_dir = home.join(".config").join("forge").join("agents");
        for def in load_agents_from_dir(&global_dir, AgentDefSource::GlobalFile) {
            defs_by_name.insert(def.name.clone(), def);
        }
    }

    // 3. Project-local agents override global/built-in
    let project_dir = project_root.join(".agent").join("agents");
    for def in load_agents_from_dir(&project_dir, AgentDefSource::ProjectFile) {
        defs_by_name.insert(def.name.clone(), def);
    }

    let mut defs: Vec<AgentDefinition> = defs_by_name.into_values().collect();
    defs.sort_by(|a, b| a.name.cmp(&b.name));

    Ok(defs)
}
