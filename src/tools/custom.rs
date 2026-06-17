// SPDX-License-Identifier: Apache-2.0
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

use crate::api::types::{FunctionDefinition, ToolDefinition};

use super::executor::ToolKind;

#[derive(Debug, Clone)]
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub script_path: PathBuf,
    pub kind: ToolKind,
    pub timeout_secs: u64,
}

#[derive(Debug, Deserialize)]
struct CustomToolFile {
    name: Option<String>,
    description: Option<String>,
    parameters: Option<serde_json::Value>,
    script: Option<String>,
    kind: Option<String>,
    timeout_secs: Option<u64>,
}

impl CustomTool {
    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            tool_type: "function".to_string(),
            function: FunctionDefinition {
                name: self.name.clone(),
                description: self.description.clone(),
                parameters: self.parameters.clone(),
            },
        }
    }
}

pub fn load_custom_tools(project_root: &Path) -> Vec<CustomTool> {
    let mut tools_by_name = std::collections::HashMap::new();

    if let Some(home) = dirs::home_dir() {
        let global_dir = home.join(".config").join("forge").join("tools");
        for tool in load_custom_tools_from_dir(&global_dir) {
            tools_by_name.insert(tool.name.clone(), tool);
        }
    }

    let project_dir = project_root.join(".agent").join("tools");
    for tool in load_custom_tools_from_dir(&project_dir) {
        tools_by_name.insert(tool.name.clone(), tool);
    }

    let mut tools: Vec<CustomTool> = tools_by_name.into_values().collect();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    tools
}

fn load_custom_tools_from_dir(dir: &Path) -> Vec<CustomTool> {
    let mut tools = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return tools;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        match load_custom_tool_file(dir, &path) {
            Ok(tool) => tools.push(tool),
            Err(e) => eprintln!(
                "Warning: failed to load custom tool {}: {}",
                path.display(),
                e
            ),
        }
    }

    tools
}

fn load_custom_tool_file(dir: &Path, path: &Path) -> Result<CustomTool> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read custom tool {}", path.display()))?;
    let raw: CustomToolFile = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse custom tool {}", path.display()))?;

    let fallback_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("custom_tool")
        .to_string();
    let name = raw.name.unwrap_or(fallback_name);
    validate_tool_name(&name)?;

    let script = raw.script.unwrap_or_else(|| format!("{}.sh", name));
    let script_path = dir.join(script);
    if !script_path.is_file() {
        anyhow::bail!("script not found: {}", script_path.display());
    }

    let description = raw
        .description
        .unwrap_or_else(|| format!("Run custom tool script {}", script_path.display()));
    let parameters = raw.parameters.unwrap_or_else(|| {
        json!({
            "type": "object",
            "properties": {},
            "required": []
        })
    });
    validate_parameters(&parameters)?;

    Ok(CustomTool {
        name,
        description,
        parameters,
        script_path,
        kind: parse_kind(raw.kind.as_deref()),
        timeout_secs: raw.timeout_secs.unwrap_or(300).clamp(1, 3600),
    })
}

fn validate_tool_name(name: &str) -> Result<()> {
    if name.is_empty() {
        anyhow::bail!("tool name cannot be empty");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!("tool name must contain only ASCII letters, digits, _ or -");
    }
    Ok(())
}

fn validate_parameters(parameters: &serde_json::Value) -> Result<()> {
    if parameters.get("type").and_then(|v| v.as_str()) != Some("object") {
        anyhow::bail!("parameters must be a JSON schema object with type=object");
    }
    Ok(())
}

fn parse_kind(kind: Option<&str>) -> ToolKind {
    match kind.unwrap_or("execute") {
        "read" => ToolKind::Read,
        "write" => ToolKind::Write,
        "execute" => ToolKind::Execute,
        _ => ToolKind::Execute,
    }
}
