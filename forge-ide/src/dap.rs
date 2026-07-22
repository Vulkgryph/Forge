//! DAP client — Debug Adapter Protocol over stdio (same framing as LSP).
//!
//! Threading model mirrors `lsp.rs`: the main thread writes requests through
//! an `Arc<Mutex<ChildStdin>>`, a reader thread forwards every incoming
//! message (events and responses) over an mpsc channel, and the app drains
//! it once per frame via `poll()`.
//!
//! Launch configuration comes from `.forge/debug.toml`:
//!
//! ```toml
//! adapter = "lldb-dap"          # or "codelldb --port 0", "python -m debugpy.adapter"
//! program = "target/debug/app" # binary or script to debug
//! args    = ["--flag"]         # optional
//! ```

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};

use serde_json::{json, Value};

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct StackFrame {
    pub id:   i64,
    pub name: String,
    pub path: Option<PathBuf>,
    pub line: usize, // 1-based (DAP convention)
}

#[derive(Clone, Debug)]
pub struct Variable {
    pub name:  String,
    pub value: String,
}

/// High-level happenings the app reacts to each frame.
#[derive(Clone, Debug)]
pub enum DapEvent {
    /// Execution stopped (breakpoint, step, pause…). Carries the thread id.
    // thread_id duplicates the client's own `self.thread_id`, updated one
    // line before this event is built and already used correctly by
    // continue_run/step_over/etc — no separate "which thread" tracking
    // needed on the app.rs side.
    Stopped { reason: String },
    /// Debuggee produced output (stdout/stderr/console).
    Output(String),
    /// The debug session ended.
    Terminated,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct LaunchConfig {
    pub adapter: String,
    pub program: String,
    #[serde(default)]
    pub args:    Vec<String>,
}

/// Load `.forge/debug.toml`, falling back to sensible defaults per language.
pub fn launch_config(root: &Path, active_file: Option<&Path>) -> Result<LaunchConfig, String> {
    let path = root.join(".forge").join("debug.toml");
    if let Ok(text) = std::fs::read_to_string(&path) {
        return toml::from_str(&text).map_err(|e| format!("debug.toml: {e}"));
    }
    let ext = active_file.and_then(|p| p.extension()).and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "rs" => {
            let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("app");
            Ok(LaunchConfig {
                adapter: "lldb-dap".into(),
                program: format!("target/debug/{name}"),
                args:    Vec::new(),
            })
        }
        "py" => Ok(LaunchConfig {
            adapter: "python3 -m debugpy.adapter".into(),
            program: active_file.unwrap().to_string_lossy().into_owned(),
            args:    Vec::new(),
        }),
        _ => Err("No .forge/debug.toml and no default adapter for this file type".into()),
    }
}

// ── Client ──────────────────────────────────────────────────────────────────

pub struct DapClient {
    child:     Child,
    stdin:     Arc<Mutex<ChildStdin>>,
    rx:        mpsc::Receiver<Value>,
    next_seq:  i64,
    /// command → response (for requests we're waiting on)
    responses: HashMap<i64, Value>,
    /// seq of the in-flight stackTrace request (0 = none)
    pub stack_req: i64,
    /// seq of the in-flight scopes/variables request (0 = none)
    pub vars_req:  i64,
    pub thread_id: i64,
}

impl DapClient {
    /// Spawn the adapter and run the `initialize` handshake.
    pub fn start(cfg: &LaunchConfig, cwd: &Path) -> Result<Self, String> {
        let mut parts = cfg.adapter.split_whitespace();
        let bin = parts.next().ok_or("empty adapter command")?;
        let mut child = Command::new(bin)
            .args(parts)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to spawn {bin}: {e}"))?;

        let stdin = Arc::new(Mutex::new(child.stdin.take().ok_or("no stdin")?));
        let mut reader = BufReader::new(child.stdout.take().ok_or("no stdout")?);

        let init = json!({
            "seq": 1, "type": "request", "command": "initialize",
            "arguments": {
                "clientID": "forge-ide", "adapterID": "forge",
                "linesStartAt1": true, "columnsStartAt1": true,
                "pathFormat": "path",
                "supportsRunInTerminalRequest": false,
            }
        });
        write_msg(&mut *stdin.lock().map_err(|_| "poisoned")?, &init)
            .map_err(|e| e.to_string())?;
        // Wait for the initialize response (events may arrive first).
        let mut early: Vec<Value> = Vec::new();
        loop {
            let msg = read_msg(&mut reader).ok_or("adapter closed during init")?;
            if msg.get("type").and_then(|v| v.as_str()) == Some("response")
                && msg.get("command").and_then(|v| v.as_str()) == Some("initialize") {
                break;
            }
            early.push(msg);
        }

        let (tx, rx) = mpsc::channel();
        for m in early { let _ = tx.send(m); }
        std::thread::spawn(move || {
            while let Some(msg) = read_msg(&mut reader) {
                if tx.send(msg).is_err() { break; }
            }
        });

        Ok(Self {
            child, stdin, rx,
            next_seq: 2,
            responses: HashMap::new(),
            stack_req: 0,
            vars_req:  0,
            thread_id: 0,
        })
    }

    fn request(&mut self, command: &str, arguments: Value) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        let msg = json!({ "seq": seq, "type": "request",
                          "command": command, "arguments": arguments });
        if let Ok(mut s) = self.stdin.lock() {
            let _ = write_msg(&mut *s, &msg);
        }
        seq
    }

    pub fn launch(&mut self, cfg: &LaunchConfig, cwd: &Path) {
        let program = if Path::new(&cfg.program).is_absolute() {
            cfg.program.clone()
        } else {
            cwd.join(&cfg.program).to_string_lossy().into_owned()
        };
        self.request("launch", json!({
            "program": program,
            "args": cfg.args,
            "cwd": cwd.to_string_lossy(),
            "stopOnEntry": false,
        }));
    }

    pub fn set_breakpoints(&mut self, path: &Path, lines: &[usize]) {
        let bps: Vec<Value> = lines.iter().map(|l| json!({ "line": l + 1 })).collect();
        self.request("setBreakpoints", json!({
            "source": { "path": path.to_string_lossy() },
            "breakpoints": bps,
        }));
    }

    pub fn configuration_done(&mut self) {
        self.request("configurationDone", json!({}));
    }

    pub fn continue_run(&mut self)   { let t = self.thread_id; self.request("continue", json!({ "threadId": t })); }
    pub fn step_over(&mut self)      { let t = self.thread_id; self.request("next",     json!({ "threadId": t })); }
    pub fn step_in(&mut self)        { let t = self.thread_id; self.request("stepIn",   json!({ "threadId": t })); }
    pub fn step_out(&mut self)       { let t = self.thread_id; self.request("stepOut",  json!({ "threadId": t })); }
    pub fn pause(&mut self)          { let t = self.thread_id; self.request("pause",    json!({ "threadId": t })); }
    pub fn disconnect(&mut self)     { self.request("disconnect", json!({ "terminateDebuggee": true })); }

    pub fn stack_trace(&mut self) {
        let t = self.thread_id;
        self.stack_req = self.request("stackTrace", json!({ "threadId": t, "levels": 32 }));
    }

    pub fn scopes(&mut self, frame_id: i64) {
        self.vars_req = self.request("scopes", json!({ "frameId": frame_id }));
    }

    pub fn variables(&mut self, variables_reference: i64) {
        self.vars_req = self.request("variables",
            json!({ "variablesReference": variables_reference }));
    }

    /// Drain incoming messages. Returns high-level events; stashes responses
    /// for `take_response`.
    pub fn poll(&mut self) -> Vec<DapEvent> {
        let mut out = Vec::new();
        while let Ok(msg) = self.rx.try_recv() {
            match msg.get("type").and_then(|v| v.as_str()) {
                Some("event") => {
                    let ev   = msg.get("event").and_then(|v| v.as_str()).unwrap_or("");
                    let body = msg.get("body").cloned().unwrap_or(Value::Null);
                    match ev {
                        "stopped" => {
                            let tid = body.get("threadId").and_then(|v| v.as_i64()).unwrap_or(0);
                            let reason = body.get("reason").and_then(|v| v.as_str())
                                .unwrap_or("").to_string();
                            self.thread_id = tid;
                            out.push(DapEvent::Stopped { reason });
                        }
                        "output" => {
                            if let Some(t) = body.get("output").and_then(|v| v.as_str()) {
                                out.push(DapEvent::Output(t.trim_end().to_string()));
                            }
                        }
                        "terminated" | "exited" => out.push(DapEvent::Terminated),
                        _ => {}
                    }
                }
                Some("response") => {
                    if let Some(seq) = msg.get("request_seq").and_then(|v| v.as_i64()) {
                        self.responses.insert(seq, msg);
                    }
                }
                _ => {}
            }
        }
        out
    }

    pub fn take_response(&mut self, seq: i64) -> Option<Value> {
        self.responses.remove(&seq)
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        self.disconnect();
        std::thread::sleep(std::time::Duration::from_millis(100));
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Response parsers ────────────────────────────────────────────────────────

pub fn parse_stack_trace(resp: &Value) -> Vec<StackFrame> {
    let mut out = Vec::new();
    let Some(frames) = resp.pointer("/body/stackFrames").and_then(|v| v.as_array())
        else { return out };
    for f in frames {
        out.push(StackFrame {
            id:   f.get("id").and_then(|v| v.as_i64()).unwrap_or(0),
            name: f.get("name").and_then(|v| v.as_str()).unwrap_or("?").to_string(),
            path: f.pointer("/source/path").and_then(|v| v.as_str()).map(PathBuf::from),
            line: f.get("line").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        });
    }
    out
}

pub fn parse_variables(resp: &Value) -> Vec<Variable> {
    let mut out = Vec::new();
    let Some(vars) = resp.pointer("/body/variables").and_then(|v| v.as_array())
        else { return out };
    for v in vars {
        out.push(Variable {
            name:  v.get("name").and_then(|x| x.as_str()).unwrap_or("?").to_string(),
            value: v.get("value").and_then(|x| x.as_str()).unwrap_or("").to_string(),
        });
    }
    out
}

/// First scope's variablesReference from a `scopes` response.
pub fn parse_first_scope_ref(resp: &Value) -> Option<i64> {
    resp.pointer("/body/scopes")?.as_array()?
        .first()?.get("variablesReference")?.as_i64()
}

// ── Wire framing (Content-Length headers, same as LSP) ─────────────────────

fn write_msg(w: &mut impl Write, msg: &Value) -> std::io::Result<()> {
    let body = msg.to_string();
    write!(w, "Content-Length: {}\r\n\r\n{}", body.len(), body)?;
    w.flush()
}

fn read_msg(r: &mut impl BufRead) -> Option<Value> {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 { return None; }
        let line = line.trim_end();
        if line.is_empty() { break; }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse().ok()?;
        }
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}
