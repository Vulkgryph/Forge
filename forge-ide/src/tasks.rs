//! Task runner — named shell tasks defined in `.forge/tasks.toml`.
//!
//! ```toml
//! [tasks]
//! build = "cargo build"
//! test  = { cmd = "cargo test", cwd = "forge-server" }
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use crate::OutputLevel;

#[derive(Clone, Debug)]
pub struct Task {
    pub name: String,
    pub cmd:  String,
    pub cwd:  Option<String>,
}

#[derive(serde::Deserialize)]
struct TasksFile {
    #[serde(default)]
    tasks: toml::value::Table,
}

/// Load tasks from `<root>/.forge/tasks.toml` (empty vec if absent/invalid).
pub fn load(root: &Path) -> Vec<Task> {
    let path = root.join(".forge").join("tasks.toml");
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    let Ok(file) = toml::from_str::<TasksFile>(&text) else { return Vec::new() };
    let mut out = Vec::new();
    for (name, val) in file.tasks {
        match val {
            toml::Value::String(cmd) => out.push(Task { name, cmd, cwd: None }),
            toml::Value::Table(t) => {
                let Some(cmd) = t.get("cmd").and_then(|v| v.as_str()) else { continue };
                let cwd = t.get("cwd").and_then(|v| v.as_str()).map(String::from);
                out.push(Task { name, cmd: cmd.to_string(), cwd });
            }
            _ => {}
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Run a task in a background thread, streaming stdout/stderr lines.
pub fn run(task: &Task, root: &Path) -> mpsc::Receiver<(String, OutputLevel)> {
    let (tx, rx) = mpsc::channel();
    let cmd  = task.cmd.clone();
    let name = task.name.clone();
    let cwd: PathBuf = match &task.cwd {
        Some(d) if Path::new(d).is_absolute() => PathBuf::from(d),
        Some(d) => root.join(d),
        None    => root.to_path_buf(),
    };
    std::thread::spawn(move || {
        let _ = tx.send((format!("▶ {name}: {cmd}"), OutputLevel::Info));
        let shell = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
        let child = Command::new(shell.0).arg(shell.1).arg(&cmd)
            .current_dir(&cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => { let _ = tx.send((format!("task failed to start: {e}"), OutputLevel::Error)); return; }
        };
        use std::io::{BufRead, BufReader};
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let tx_err = tx.clone();
        let err_thread = stderr.map(|se| std::thread::spawn(move || {
            for line in BufReader::new(se).lines().flatten() {
                let _ = tx_err.send((line, OutputLevel::Warn));
            }
        }));
        if let Some(so) = stdout {
            for line in BufReader::new(so).lines().flatten() {
                let _ = tx.send((line, OutputLevel::Info));
            }
        }
        if let Some(h) = err_thread { let _ = h.join(); }
        match child.wait() {
            Ok(st) if st.success() => { let _ = tx.send((format!("✓ {name} finished"), OutputLevel::Success)); }
            Ok(st)  => { let _ = tx.send((format!("✗ {name} exited with {st}"), OutputLevel::Error)); }
            Err(e)  => { let _ = tx.send((format!("✗ {name}: {e}"), OutputLevel::Error)); }
        }
    });
    rx
}
