//! External formatter integration — shells out to the standard formatter for
//! each language, feeding the buffer through stdin and reading the result.

use std::io::Write;
use std::process::{Command, Stdio};

/// The formatter command for a file extension, if one is known.
/// `file_name` is used by formatters that infer the parser from the name.
fn command_for(ext: &str, file_name: &str) -> Option<Command> {
    let mut cmd = match ext {
        "rs" => Command::new("rustfmt"),
        "py" => { let mut c = Command::new("black"); c.args(["-q", "-"]); c }
        "go" => Command::new("gofmt"),
        "c" | "h" | "cpp" | "hpp" | "cc" | "cxx" | "m" | "mm" => {
            let mut c = Command::new("clang-format");
            c.arg(format!("--assume-filename={file_name}"));
            c
        }
        "js" | "jsx" | "ts" | "tsx" | "json" | "css" | "scss" | "html"
        | "md" | "yaml" | "yml" => {
            let mut c = Command::new("prettier");
            c.arg(format!("--stdin-filepath={file_name}"));
            c
        }
        _ => return None,
    };
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    Some(cmd)
}

/// Format `text` for the given extension. Returns the formatted text, or an
/// error describing what went wrong (missing binary, syntax error, …).
pub fn format(ext: &str, file_name: &str, text: &str) -> Result<String, String> {
    let mut cmd = command_for(ext, file_name)
        .ok_or_else(|| format!("no formatter configured for .{ext}"))?;
    let program = cmd.get_program().to_string_lossy().to_string();
    let mut child = cmd.spawn()
        .map_err(|e| format!("{program}: {e} (is it installed?)"))?;
    child.stdin.take()
        .ok_or("no stdin")?
        .write_all(text.as_bytes())
        .map_err(|e| format!("{program}: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("{program}: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{program}: {}", err.lines().next().unwrap_or("failed")));
    }
    let formatted = String::from_utf8_lossy(&out.stdout).to_string();
    if formatted.is_empty() && !text.is_empty() {
        return Err(format!("{program} produced no output"));
    }
    Ok(formatted)
}
