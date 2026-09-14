// SPDX-License-Identifier: Apache-2.0
use crate::agent::agent_def::AgentDefinition;
use crate::api::types::{FunctionDefinition, ToolDefinition};
use serde_json::json;

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "read_file".to_string(),
                description: "Read the contents of a file. Returns the file content with line numbers.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read, relative to the project root"
                        },
                        "start_line": {
                            "type": "integer",
                            "description": "Optional starting line number (1-indexed). Omit to read from the beginning."
                        },
                        "end_line": {
                            "type": "integer",
                            "description": "Optional ending line number (1-indexed). Omit to read to the end."
                        }
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "list_directory".to_string(),
                description: "List files and directories at a given path. Shows file sizes and directory item counts.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the directory, relative to the project root. Use '.' for root."
                        },
                        "max_depth": {
                            "type": "integer",
                            "description": "Maximum depth to recurse. Default is 1 (immediate children only)."
                        }
                    },
                    "required": ["path"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "search_code".to_string(),
                description: "Search for a pattern in the codebase using regex or literal string matching. Returns matching lines with file paths and line numbers.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search pattern (regex by default)"
                        },
                        "path": {
                            "type": "string",
                            "description": "Optional directory or file to search in, relative to project root. Defaults to entire project."
                        },
                        "file_pattern": {
                            "type": "string",
                            "description": "Optional glob pattern to filter files, e.g. '*.rs' or '*.py'"
                        },
                        "fixed_string": {
                            "type": "boolean",
                            "description": "If true, treat query as a literal string instead of regex. Default false."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of results to return. Default 30."
                        }
                    },
                    "required": ["query"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "apply_patch".to_string(),
                description: "Apply a valid git-style unified diff to modify files. Use this for larger or multi-file changes only when you can produce a complete diff with ---/+++ file headers and @@ hunks. For small targeted edits, prefer edit_file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "unified_diff": {
                            "type": "string",
                            "description": "A complete unified diff in the format produced by 'git diff'. Must include --- and +++ file headers plus @@ hunk headers with valid line counts. Do not pass partial snippets or informal patches."
                        }
                    },
                    "required": ["unified_diff"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "write_file".to_string(),
                description: "Create a new file or overwrite an existing file with the given content.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write, relative to the project root"
                        },
                        "content": {
                            "type": "string",
                            "description": "The full content to write to the file"
                        }
                    },
                    "required": ["path", "content"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "edit_file".to_string(),
                description: "Replace a unique string in a file with new content. Prefer this for small, targeted edits because it avoids malformed unified diffs. The old_string must appear exactly once in the file.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to edit, relative to the project root"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "The exact string to find and replace (must be unique in the file)"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "The replacement string"
                        }
                    },
                    "required": ["path", "old_string", "new_string"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "glob_files".to_string(),
                description: "Find files matching a glob pattern. Fast file discovery by name/extension.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "Glob pattern like \"**/*.rs\", \"src/**/*.ts\", \"*.json\""
                        },
                        "path": {
                            "type": "string",
                            "description": "Optional directory to search in, relative to project root. Defaults to project root."
                        }
                    },
                    "required": ["pattern"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "todo_write".to_string(),
                description: "Manage your task list. Use to track progress on multi-step work. \
                    Mark an item done as soon as it is done, and call clear_done once a batch of \
                    work is finished — a list that is mostly completed items no longer shows what \
                    is left, which is the only thing it is for. A task is identified by its own \
                    text, so pass the task text to update it; there are no numbers to track.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "enum": ["add", "update", "list", "clear_done"],
                            "description": "Action to perform: add a task, update a task's status, list all tasks, or clear_done to remove the completed ones"
                        },
                        "text": {
                            "type": "string",
                            "description": "The task itself — required for both 'add' and 'update'. A task is identified by its text, so for 'update' pass the task's text as it appears in the list. Enough of it to be unambiguous is sufficient."
                        },
                        "status": {
                            "type": "string",
                            "enum": ["pending", "in_progress", "done"],
                            "description": "New status (required for 'update')"
                        }
                    },
                    "required": ["action"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "web_search".to_string(),
                description: "Search pages Forge has crawled and indexed itself. Returns titles, URLs and \
                    snippets. This is a real index, not a scrape of someone else's results page, so it is \
                    reliable — but it only knows what it has crawled. A query the index cannot answer \
                    triggers a crawl first, which takes up to 25 seconds; every later query is \
                    milliseconds. If the results are off-topic, the index simply has not read the right \
                    pages: pass `sites` with the URLs to crawl rather than rephrasing the query. For \
                    biomedical or life-sciences literature use search_papers instead, which reaches \
                    journal articles this cannot.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The search query. Supports quoted phrases and -excluded terms."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of results to return (default 5, max 20)"
                        },
                        "sites": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "URLs to crawl when the index cannot answer the query. Use this \
                                to point the crawler at the right documentation instead of hoping the \
                                default seeds cover it. Crawling stays on the hosts given here."
                        },
                        "max_pages": {
                            "type": "integer",
                            "description": "Pages to crawl if a crawl is needed (default 40, max 500). \
                                Higher covers more and takes longer; the 25-second budget may stop it early, \
                                in which case asking again continues from where it stopped."
                        }
                    },
                    "required": ["query"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "search_papers".to_string(),
                description: "Search the biomedical and life-sciences literature through Europe PMC, and \
                    search the full text of the articles it returns. Use this for a measured value, a \
                    method, or a result that would be stated in a paper — recording temperatures, \
                    concentrations, cell types, assay conditions — none of which are on the pages \
                    web_search can crawl, because PubMed Central forbids crawling. \
                    Only open-access articles have their text indexed; anything else comes back as a \
                    citation and a link, and the result says so. Every result reports the licence its \
                    text is held under: quote it with that attribution, and note that cc by-nc excludes \
                    commercial use. \
                    The query goes to Europe PMC as written, so its syntax works: quoted phrases, AND/OR, \
                    and field prefixes such as AUTH:, JOURNAL:, METHODS:. First call on a topic fetches \
                    articles at one per second; later ones are instant.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "The query, in Europe PMC syntax. Prefer a few specific terms \
                                over a sentence — e.g. '\"pyramidal neuron\" AND \"patch clamp\" AND temperature'."
                        },
                        "max_results": {
                            "type": "integer",
                            "description": "Maximum number of passages to return (default 5, max 20)"
                        },
                        "max_articles": {
                            "type": "integer",
                            "description": "How many articles to fetch if the local index cannot answer \
                                (default 6, max 25). Each one is a request at one per second, so raising \
                                this makes the first call slower and the coverage wider."
                        }
                    },
                    "required": ["query"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "web_fetch".to_string(),
                description: "Fetch a web page and extract specific information. Requires a prompt describing what to look for. Returns a focused answer, not raw page text. Use after web_search to read full content, or to fetch any URL directly.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": "The URL to fetch"
                        },
                        "prompt": {
                            "type": "string",
                            "description": "What information to extract from the page"
                        },
                        "max_length": {
                            "type": "integer",
                            "description": "Max chars of raw content to send to summarizer (default 20000)"
                        }
                    },
                    "required": ["url", "prompt"]
                }),
            },
        },
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: "shell_exec".to_string(),
                description: "Execute a shell command in the project directory. Commands still running after 120s are automatically backgrounded unless wait=true. Regardless of wait/timeout_secs, any command still running after agent.forced_shell_background_secs (default 300s / 5 minutes) is force-moved to the background as bg-N so the turn cannot stay stuck — poll with background_id or stop with background_action=kill; completion is delivered automatically. Forge emits progress heartbeats every 5 minutes while a foreground command is still running. Set wait=true for commands that must complete before you continue (builds, tests, compilers); set a high timeout_secs for legitimate long audits (still subject to the forced-background ceiling). Set run_in_background=true for long-running services, watchers, daemons. Do not run interactive/full-screen terminal apps inline (forge, vim, less, top, ssh, REPLs, etc.); run them manually or use non-interactive alternatives. Do not run `forge` or `./forge` from inside Forge; use `forge --version`, wrapper inspection commands, or `forge-agent --headless` for protocol tests.".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute"
                        },
                        "working_dir": {
                            "type": "string",
                            "description": "Optional working directory relative to project root. Defaults to project root."
                        },
                        "wait": {
                            "type": "boolean",
                            "description": "If true, wait for the command to complete instead of the normal 120s auto-background. Still subject to agent.forced_shell_background_secs (default 300s), which force-backgrounds rather than killing. Use for builds, compilers, test runners, installs."
                        },
                        "run_in_background": {
                            "type": "boolean",
                            "description": "If true, start the command and immediately background it without waiting for output. Use for long-running services, dev servers, watchers, and daemons. Returns a background ID and PID."
                        },
                        "timeout_secs": {
                            "type": "integer",
                            "description": "Timeout in seconds. With wait=true: kills the command if exceeded before the forced-background ceiling (default 300s). Without wait: overrides the auto-background threshold (default 120s). Cannot prevent force-background at agent.forced_shell_background_secs."
                        },
                        "background_id": {
                            "type": "string",
                            "description": "Check on a background command by its ID (e.g. 'bg-1'). Ignored when 'command' is present. Empty strings are treated as absent."
                        },
                        "background_action": {
                            "type": "string",
                            "enum": ["status", "kill"],
                            "description": "Action for background command: 'status' (default) to check output, 'kill' to stop it. Ignored when 'command' is present."
                        }
                    }
                }),
            },
        },
    ]
}

/// Build the delegate_task tool definition, dynamically listing available agents.
pub fn delegate_task_definition(available_agents: &[AgentDefinition]) -> ToolDefinition {
    let agent_list: Vec<String> = available_agents
        .iter()
        .map(|a| {
            format!(
                "{}: {} (tools: {})",
                a.name,
                a.description,
                a.tools.join(", ")
            )
        })
        .collect();
    let agent_desc = format!(
        "Pre-defined agent to use, or 'custom' for a fully custom agent. Available agents:\n{}\n\n\
         IMPORTANT: Choose the right agent for the task based on its tools. For example, 'bash' only has \
         shell_exec and read_file — it CANNOT write or edit files. Use 'general' for tasks that need file \
         writing (apply_patch, shell_exec, read_file, etc.), or use tools_override to give any agent the \
         specific tools it needs.",
        agent_list.join("\n")
    );

    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: "delegate_task".to_string(),
            description: "Delegate a task to a specialized subagent that runs autonomously with its own context window. \
                For complex multi-step work with independent subtasks, you can call this multiple times in one response \
                to run subagents in parallel. The subagent runs to completion and returns its findings/results. \
                Parent waits are bounded by agent.subagents.max_delegate_secs (default 1800s); subagent shell_exec \
                calls themselves hard-timeout (default 300s) so a stuck command cannot freeze the chat.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "agent_type": {
                        "type": "string",
                        "description": agent_desc
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The task for the subagent to perform"
                    },
                    "tools_override": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional. Override the tool allowlist. If omitted, uses the agent's default tools."
                    },
                    "system_prompt_override": {
                        "type": "string",
                        "description": "Optional. Override the system prompt. If omitted, uses the agent's default."
                    },
                    "model_override": {
                        "type": "string",
                        "description": "Optional. Endpoint name from config to use instead of the agent's default model. Omit this field to inherit the current/default subagent model; do not use raw model ids or values like default."
                    },
                    "max_turns_override": {
                        "type": "integer",
                        "description": "Optional. Set a turn limit for this invocation."
                    }
                },
                "required": ["agent_type", "prompt"]
            }),
        },
    }
}

pub fn ask_question_definition() -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: "ask_question".to_string(),
            description: "Ask the user a question and wait for their response. Use this when you need clarification, \
                want to confirm an approach before proceeding, or need the user to make a decision. \
                Supports plain text (\"question\") or structured multi-question format (\"questions\" array) with \
                selectable options. An \"Other\" free-text option is automatically appended to each question. \
                Execution pauses until the user responds.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": {
                        "type": "string",
                        "description": "A plain-text question to ask the user (simple mode). Mutually exclusive with 'questions'."
                    },
                    "questions": {
                        "type": "array",
                        "description": "Structured questions with selectable options (1-4 questions). Mutually exclusive with 'question'.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "The question text"
                                },
                                "header": {
                                    "type": "string",
                                    "description": "Short label displayed as a tag (max 12 chars), e.g. 'Auth method'"
                                },
                                "options": {
                                    "type": "array",
                                    "description": "2-4 selectable options",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "Display text for this option (1-5 words)"
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "Explanation of what this option means"
                                            }
                                        },
                                        "required": ["label", "description"]
                                    }
                                },
                                "multiSelect": {
                                    "type": "boolean",
                                    "description": "If true, allow multiple options to be selected. Default false."
                                }
                            },
                            "required": ["question", "header", "options"]
                        }
                    }
                }
            }),
        },
    }
}

pub fn enter_plan_mode_definition() -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: "enter_plan_mode".to_string(),
            description: "Enter planning mode. In plan mode, you can only read/explore the codebase and write a plan. \
                Use this when you need to think through a complex task before making changes. \
                Once in plan mode, use write_plan to draft your plan and exit_plan_mode to submit it for user approval.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    }
}

pub fn write_plan_definition() -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: "write_plan".to_string(),
            description: "Write or update your implementation plan. This is the only write operation allowed in plan mode. \
                Structure your plan with: Goal, Files to modify, Implementation steps, Verification.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The full plan content in markdown format"
                    }
                },
                "required": ["content"]
            }),
        },
    }
}

pub fn exit_plan_mode_definition() -> ToolDefinition {
    ToolDefinition {
        tool_type: "function".to_string(),
        function: FunctionDefinition {
            name: "exit_plan_mode".to_string(),
            description: "Submit your plan for user approval. Call this when your plan is complete and ready for review. \
                The user will be able to approve the plan (letting you proceed with implementation) or request revisions.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        },
    }
}

/// Returns the tool definitions available during plan mode:
/// read tools + write_plan + exit_plan_mode (NOT enter_plan_mode since we're already in it).
pub fn get_plan_mode_tools() -> Vec<ToolDefinition> {
    let all = get_tool_definitions();
    let mut plan_tools: Vec<ToolDefinition> = all
        .into_iter()
        .filter(|td| {
            matches!(
                td.function.name.as_str(),
                "read_file" | "list_directory" | "search_code" | "glob_files"
            )
        })
        .collect();
    plan_tools.push(write_plan_definition());
    plan_tools.push(exit_plan_mode_definition());
    plan_tools
}

/// Return only tool definitions whose names are in the allowlist.
/// All tool names that can be toggled via disabled_tools config.
/// Excludes internal tools (enter_plan_mode, write_plan, exit_plan_mode, ask_question).
pub fn get_toggleable_tool_names() -> Vec<&'static str> {
    vec![
        "read_file",
        "list_directory",
        "search_code",
        "apply_patch",
        "write_file",
        "edit_file",
        "glob_files",
        "todo_write",
        "web_search",
        "web_fetch",
        "search_papers",
        "shell_exec",
        "delegate_task",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tool that can be toggled but has no definition is wired only halfway:
    /// turning it on does nothing, and the two halves live in different files.
    ///
    /// `delegate_task` is the one exception, and a real one rather than an
    /// oversight — its definition is built from whichever subagents are
    /// available, so it cannot be a constant. It is named here so the
    /// exception is a decision rather than a hole in the check.
    #[test]
    fn every_toggleable_tool_is_actually_defined() {
        const BUILT_PER_SESSION: &[&str] = &["delegate_task"];
        let defined: Vec<String> = get_tool_definitions()
            .into_iter()
            .map(|t| t.function.name)
            .collect();
        for name in get_toggleable_tool_names() {
            if BUILT_PER_SESSION.contains(&name) {
                continue;
            }
            assert!(
                defined.iter().any(|d| d == name),
                "{name} can be toggled but has no definition, so turning it on does nothing",
            );
        }
        // And the exception really is built, rather than missing everywhere.
        assert_eq!(delegate_task_definition(&[]).function.name, "delegate_task");
    }

    #[test]
    fn the_literature_tool_is_offered() {
        let defs = get_tool_definitions();
        let papers = defs
            .iter()
            .find(|t| t.function.name == "search_papers")
            .expect("search_papers is not in the tool list");
        // The licence obligation has to reach the model, since it is what
        // decides whether a passage may be quoted or used commercially.
        let described = &papers.function.description;
        assert!(described.contains("licence"), "the description does not mention licensing: {described}");
        assert!(
            described.contains("open-access") || described.contains("open access"),
            "the description does not say what is and is not kept: {described}",
        );
        let props = &papers.function.parameters["properties"];
        assert!(props.get("query").is_some());
        assert!(props.get("max_articles").is_some());
        assert!(get_toggleable_tool_names().contains(&"search_papers"));
    }

    /// The old description told the model the tool was unreliable and not to
    /// retry, which was true of a DuckDuckGo scrape and is not true of an
    /// index Forge crawls itself. Left in place it steers the model away from
    /// a tool that works.
    #[test]
    fn web_search_no_longer_describes_itself_as_a_scrape() {
        let defs = get_tool_definitions();
        let web = defs.iter().find(|t| t.function.name == "web_search").expect("web_search");
        let described = &web.function.description;
        assert!(!described.contains("DuckDuckGo"), "{described}");
        assert!(!described.contains("UNRELIABLE"), "{described}");
        // And it documents the arguments that decide what gets crawled.
        let props = &web.function.parameters["properties"];
        assert!(props.get("sites").is_some(), "`sites` is undocumented, so the model cannot aim the crawl");
        assert!(props.get("max_pages").is_some());
    }
}
