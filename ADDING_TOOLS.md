# Adding a New Tool

Every tool must be wired into multiple places. Missing any of these will cause bugs — the tool won't appear in the API, won't be callable, or won't display correctly.

## Checklist

### Required for every tool

| # | File | Location | What to do |
|---|------|----------|------------|
| 1 | `src/tools/definitions.rs` | `get_tool_definitions()` | Add a `ToolDefinition` with name, description, and JSON schema for parameters |
| 2 | `src/tools/executor.rs` | `classify_tool()` | Add tool name to the correct `ToolKind` match arm (`Read`, `Write`, or `Execute`) |
| 3 | `src/tools/executor.rs` | `execute()` | Add match arm dispatching to your handler function |
| 4 | `src/tools/executor.rs` | (new method) | Implement `async fn my_tool(&self, args: &serde_json::Value) -> Result<String>` |
| 5 | `src/agent/core.rs` | `build_system_prompt()` | Add tool to the capabilities list with a description. Place it in the right category (File reading / File writing / Other) |
| 6 | `ui/src/components/App.tsx` | `toolLabel()` | Add a friendly label so the settings UI does not show a raw tool name |
| 7 | `src/tools/definitions.rs` | `get_toggleable_tool_names()` | Add the tool if users should be able to enable/disable it from settings |

### Conditional — depends on the tool's nature

| # | File | Location | When | What to do |
|---|------|----------|------|------------|
| 8 | `src/tools/definitions.rs` | `get_plan_mode_tools()` | Tool is read-only | Add tool name to the filter: `"read_file" \| "list_directory" \| ... \| "my_tool"` |
| 9 | `src/agent/core.rs` | `PLAN_MODE_SYSTEM_ADDENDUM` | Tool is read-only | Add to the "You can ONLY use read tools (...)" list |
| 10 | `src/agent/core.rs` | Plan mode BLOCKED message in `handle_tool_call()` | Tool is read-only | Add to the "You can only use read tools (...)" error text |
| 11 | `src/agent/agent_def.rs` | `builtin_agents()` | Tool belongs to specific agents | Add tool name string to the `tools` vec of relevant agents (explore, bash, plan, general) |
| 12 | `src/agent/core.rs` | Unknown agent fallback in `prepare_subagent()` | Tool is a general-purpose read tool | Add to the fallback `tools: vec![...]` |
| 13 | `src/agent/subagent.rs` | Unknown agent fallback in `run_nested_subagent()` | Tool is a general-purpose read tool | Add to the fallback `tools: vec![...]` (same as #12 but for nested subagents) |
| 14 | `ui/src/hooks/useAgent.ts` | Plan approval auto-approval | Tool is a write tool that should auto-approve after plan approval | Update the plan approval handler to include the new tool |
| 15 | `src/agent/core.rs` | `is_dangerous_command()` / shell safeguards | Tool can do dangerous things | Add detection logic or equivalent guardrails |

## ToolKind classification guide

Note: `enter_plan_mode` is classified as `Execute`, while `write_plan`, `exit_plan_mode`, and `ask_question` are currently treated as `Read` for approval purposes.

- **Read**: Tool only reads/queries. No side effects. Auto-approved by default. Available in plan mode.
- **Write**: Tool modifies files. Requires approval unless auto-approved or in auto mode.
- **Execute**: Tool runs external processes or has side effects beyond file writes. Always requires approval unless in auto mode.

## How the tool list flows to the model

```
definitions.rs::get_tool_definitions()
        │
        ├──► core.rs::process_turn()     (normal mode — all tools sent to API)
        ├──► definitions.rs::get_plan_mode_tools()  (plan mode — filtered subset)
        └──► definitions.rs::get_tool_definitions_filtered()  (subagents — allowlist filter)
                                                         │
                                                         └── agent_def.rs::builtin_agents()
                                                             defines what each agent's allowlist contains
```

**Critical**: If a tool is not in `get_tool_definitions()`, the model literally cannot call it, no matter what the system prompt says. The tool definition is what makes it appear in the API's tool list.

## Common mistakes

1. **Adding to system prompt but not to `get_tool_definitions()`** — The model sees the description but the tool isn't in its callable tool list. It will try to use other tools instead.
2. **Adding to `get_tool_definitions()` but not to `classify_tool()`** — Tool returns `ToolKind::Unknown`, which always requires approval and won't work in plan mode.
3. **Adding to system prompt but not to agent allowlists** — Subagents won't have access even though the main agent does.
4. **Omitting the UI label** — The settings menu will show the raw `tool_name` instead of a friendly label.
5. **Omitting plan mode filter** — Read-only tool is unavailable during planning, forcing the agent out of plan mode to use it.
6. **Omitting toggleable-tools registration** — The tool exists, but users cannot enable/disable it from settings.

## Example: adding a hypothetical `count_lines` read tool

```rust
// 1. definitions.rs — get_tool_definitions()
ToolDefinition {
    tool_type: "function".to_string(),
    function: FunctionDefinition {
        name: "count_lines".to_string(),
        description: "Count lines in a file or set of files.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "File or directory path"
                }
            },
            "required": ["path"]
        }),
    },
},

// 2. executor.rs — classify_tool()
"read_file" | "list_directory" | "search_code" | "glob_files"
| "count_lines"  // <-- add here
| "todo_write"
| ... => ToolKind::Read,

// 3. executor.rs — execute()
"count_lines" => self.count_lines(args).await,

// 4. executor.rs — implementation
async fn count_lines(&self, args: &serde_json::Value) -> Result<String> {
    let path = args["path"].as_str().context("Missing 'path'")?;
    // ... implementation ...
    Ok(format!("{}: {} lines", path, count))
}

// 5. core.rs — build_system_prompt(), under "File reading:"
// - count_lines: Count lines in files

// 6. ui/src/components/App.tsx — toolLabel()
count_lines: "Count lines",

// 7. definitions.rs — get_toggleable_tool_names()
// add "count_lines" if you want it configurable from the UI

// 8. definitions.rs — get_plan_mode_tools() filter (it's read-only)
"read_file" | "list_directory" | "search_code" | "glob_files" | "count_lines"

// 9. core.rs — PLAN_MODE_SYSTEM_ADDENDUM (it's read-only)
// add to "You can ONLY use read tools (..., count_lines)"

// 10. core.rs — BLOCKED message (it's read-only)
// add to "You can only use read tools (..., count_lines)"

// 11. agent_def.rs — add to explore, plan, general agent tools vecs
```
