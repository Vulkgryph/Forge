// SPDX-License-Identifier: Apache-2.0
//! Black-box failure tests: the agent against a loopback-only fake provider.
//!
//! These began as a standalone Python harness, which proved the behaviour but
//! had to be remembered and run by hand — and for faults nobody reproduces on
//! purpose, that is much the same as not being run. As a `cargo test` the
//! matrix runs wherever the suite runs, Windows CI included.
//!
//! Nothing here contacts a real model or reads the user's configuration. Each
//! case gets its own HOME, workspace and provider on an ephemeral loopback
//! port, and refuses to send anything until the agent has confirmed it loaded
//! the fixture endpoint.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Long enough for a 2-second request timeout to fire and be reported, short
/// enough that a hung agent fails the test rather than the CI job.
const WAIT: Duration = Duration::from_secs(25);

// ── The fake provider ───────────────────────────────────────────────────────

struct Provider {
    /// The fault to inject. Only the *first* request is faulted; a retry gets a
    /// clean response, which is how `http503` proves it was retried at all.
    mode: Mutex<String>,
    requests: Mutex<Vec<Value>>,
    protocol: &'static str,
}

impl Provider {
    fn take_mode(&self) -> String {
        let requests = self.requests.lock().unwrap();
        let first = requests.len() == 1;
        drop(requests);
        if first {
            self.mode.lock().unwrap().clone()
        } else {
            "ok".to_string()
        }
    }
}

fn start_provider(protocol: &'static str, mode: &str) -> (Arc<Provider>, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let state = Arc::new(Provider {
        mode: Mutex::new(mode.to_string()),
        requests: Mutex::new(Vec::new()),
        protocol,
    });
    let served = Arc::clone(&state);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let state = Arc::clone(&served);
            thread::spawn(move || {
                let _ = handle(stream, &state);
            });
        }
    });
    (state, port)
}

fn handle(mut stream: TcpStream, state: &Provider) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    if reader.read_line(&mut request_line)? == 0 {
        return Ok(());
    }
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }

    // The model-list probe the agent makes before its first turn.
    if request_line.starts_with("GET") {
        let body = json!({"data": [{"id": "failure-fixture", "context_length": 131072}]});
        let body = body.to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )?;
        return stream.flush();
    }

    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body)?;
    let request: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    state.requests.lock().unwrap().push(request);

    let mode = state.take_mode();

    if matches!(mode.as_str(), "http503" | "http429" | "context_limit") {
        let (code, reason, message) = match mode.as_str() {
            "http503" => (503, "Service Unavailable", "injected provider outage"),
            "http429" => (429, "Too Many Requests", "injected provider outage"),
            _ => (400, "Bad Request", "context window exceeded"),
        };
        let body = json!({"error": {"message": message}}).to_string();
        write!(
            stream,
            "HTTP/1.1 {code} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )?;
        stream.flush()?;
        // Half-close: FIN, not RST. Shutting down both directions here raced
        // the client's read and truncated the body it was about to parse,
        // which surfaced as one case in twenty-two hanging to its timeout.
        return stream.shutdown(Shutdown::Write);
    }

    let chunked = mode == "reset";
    let mut head = String::from("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n");
    head.push_str("Connection: close\r\n");
    if chunked {
        head.push_str("Transfer-Encoding: chunked\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()?;

    let stalled = matches!(mode.as_str(), "stall" | "cancel" | "restart");
    let partial = if mode == "ok" { "RECOVERED" } else { "PARTIAL-FIXTURE" };

    if state.protocol == "anthropic" {
        event(&mut stream, chunked, Some("message_start"), &json!({"message": {"usage": {"input_tokens": 10}}}))?;
        if matches!(mode.as_str(), "tool_failure" | "truncated_tool") {
            event(&mut stream, chunked, Some("content_block_start"), &json!({
                "index": 0,
                "content_block": {"type": "tool_use", "id": "fixture-tool", "name": "read_file", "input": {}}
            }))?;
            let partial_json = if mode == "tool_failure" {
                r#"{"path":"missing-fixture-file.txt"}"#
            } else {
                r#"{"path":"#
            };
            event(&mut stream, chunked, Some("content_block_delta"), &json!({
                "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": partial_json}
            }))?;
            event(&mut stream, chunked, Some("content_block_stop"), &json!({"index": 0}))?;
        } else {
            event(&mut stream, chunked, Some("content_block_delta"), &json!({
                "index": 0, "delta": {"type": "text_delta", "text": partial}
            }))?;
            if matches!(mode.as_str(), "eof" | "reset") {
                return stream.shutdown(Shutdown::Write);
            }
            if stalled {
                thread::sleep(Duration::from_secs(5));
            }
        }
        let stop_reason = match mode.as_str() {
            "output_limit" | "truncated_tool" => "max_tokens",
            "tool_failure" => "tool_use",
            _ => "end_turn",
        };
        event(&mut stream, chunked, Some("message_delta"), &json!({
            "delta": {"stop_reason": stop_reason}, "usage": {"output_tokens": 10}
        }))?;
        event(&mut stream, chunked, Some("message_stop"), &json!({}))?;
        return stream.flush();
    }

    if matches!(mode.as_str(), "tool_failure" | "truncated_tool") {
        let arguments = if mode == "tool_failure" {
            r#"{"path":"missing-fixture-file.txt"}"#
        } else {
            r#"{"path":"#
        };
        delta(&mut stream, chunked, &json!({
            "tool_calls": [{
                "index": 0, "id": "fixture-tool", "type": "function",
                "function": {"name": "read_file", "arguments": arguments}
            }]
        }), None)?;
        let finish = if mode == "tool_failure" { "tool_calls" } else { "length" };
        delta(&mut stream, chunked, &json!({}), Some(finish))?;
    } else {
        delta(&mut stream, chunked, &json!({"content": partial}), None)?;
        if matches!(mode.as_str(), "eof" | "reset") {
            return stream.shutdown(Shutdown::Write);
        }
        if stalled {
            thread::sleep(Duration::from_secs(5));
        }
        let finish = if mode == "output_limit" { "length" } else { "stop" };
        delta(&mut stream, chunked, &json!({}), Some(finish))?;
    }
    write_frame(&mut stream, chunked, b"data: [DONE]\n\n")?;
    stream.flush()
}

fn event(w: &mut TcpStream, chunked: bool, kind: Option<&str>, value: &Value) -> std::io::Result<()> {
    let mut payload = String::new();
    if let Some(kind) = kind {
        payload.push_str("event: ");
        payload.push_str(kind);
        payload.push('\n');
    }
    payload.push_str("data: ");
    payload.push_str(&value.to_string());
    payload.push_str("\n\n");
    write_frame(w, chunked, payload.as_bytes())
}

fn delta(w: &mut TcpStream, chunked: bool, body: &Value, finish: Option<&str>) -> std::io::Result<()> {
    let value = json!({"choices": [{"index": 0, "delta": body, "finish_reason": finish}]});
    event(w, chunked, None, &value)
}

/// A chunked-transfer frame, or the bare bytes. `reset` sends valid chunks and
/// then stops without the terminating zero-length one, which is a broken
/// transport rather than a clean end of stream.
fn write_frame(w: &mut TcpStream, chunked: bool, payload: &[u8]) -> std::io::Result<()> {
    if chunked {
        write!(w, "{:x}\r\n", payload.len())?;
        w.write_all(payload)?;
        w.write_all(b"\r\n")?;
    } else {
        w.write_all(payload)?;
    }
    w.flush()
}

// ── The agent under test ────────────────────────────────────────────────────

struct Agent {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    events: Arc<Mutex<Vec<Value>>>,
}

impl Agent {
    fn start(home: &Path, resume: Option<&str>) -> Agent {
        let mut command = Command::new(env!("CARGO_BIN_EXE_forge-agent"));
        command.arg("--headless");
        if let Some(session) = resume {
            command.arg("--resume-session").arg(session);
        }
        let appdata = home.join("AppData").join("Roaming");
        let local = home.join("AppData").join("Local");
        command
            .current_dir(home.join("workspace"))
            .env("HOME", home)
            .env("USERPROFILE", home)
            .env("APPDATA", &appdata)
            .env("LOCALAPPDATA", &local)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_DATA_HOME", home.join(".local").join("share"))
            .env("XDG_CACHE_HOME", home.join(".cache"))
            .env("FORGE_CONFIG_FILE", home.join(".config/forge/config.toml"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        let mut child = command.spawn().expect("spawn forge-agent");
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();

        let (tx, rx) = channel();
        let events = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&events);
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let message: Value = serde_json::from_str(&line)
                    .unwrap_or_else(|_| json!({"type": "invalid_json", "line": line}));
                collected.lock().unwrap().push(message.clone());
                if tx.send(message).is_err() {
                    break;
                }
            }
        });

        Agent { child, stdin, rx, events }
    }

    fn send(&mut self, message: Value) {
        let _ = writeln!(self.stdin, "{message}");
        let _ = self.stdin.flush();
    }

    /// Wait for one of `kinds`. An agent that exits instead is a failure with a
    /// different name than a timeout, because they mean different things.
    fn until(&mut self, kinds: &[&str]) -> Result<Value, String> {
        let deadline = Instant::now() + WAIT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("timed out waiting for {kinds:?}"));
            }
            match self.rx.recv_timeout(left.min(Duration::from_millis(200))) {
                Ok(message) => {
                    if let Some(kind) = message.get("type").and_then(Value::as_str) {
                        if kinds.contains(&kind) {
                            return Ok(message);
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if let Ok(Some(status)) = self.child.try_wait() {
                        return Err(format!("agent exited unexpectedly: {status}"));
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("agent closed its output".to_string())
                }
            }
        }
    }

    fn saw(&self, predicate: impl Fn(&Value) -> bool) -> bool {
        self.events.lock().unwrap().iter().any(predicate)
    }

    fn snapshot(&self) -> Vec<Value> {
        self.events.lock().unwrap().clone()
    }

    fn close(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.send(json!({"type": "quit"}));
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

// ── One case ────────────────────────────────────────────────────────────────

fn config(port: u16, protocol: &str) -> String {
    format!(
        r#"
[models]
default = "fixture"
[[models.endpoints]]
name = "fixture"
base_url = "http://127.0.0.1:{port}/v1"
model_id = "failure-fixture"
max_context_tokens = 131072
max_output_tokens = 64
request_timeout_secs = 2
endpoint_type = "{protocol}"
api_key = "loopback-fixture-not-a-real-key"
[agent]
thinking_mode = false
auto_approve_reads = true
auto_approve_writes = false
max_history_messages = 200
compaction_threshold = 150
"#
    )
}

fn run(mode: &str, protocol: &'static str) {
    // Outside the repository, deliberately. CARGO_TARGET_TMPDIR lives under
    // target/, which is inside the worktree — the agent walks up to find a
    // project root, found this checkout, and took its rewind lock. Twenty-two
    // cases then contended on it, and worse, a test fixture was snapshotting
    // the working tree it was being run from.
    let home: PathBuf = std::env::temp_dir()
        .join("forge-resilience")
        .join(format!("{protocol}-{mode}"));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join(".config/forge")).unwrap();
    std::fs::create_dir_all(home.join("workspace")).unwrap();

    let (provider, port) = start_provider(protocol, mode);
    std::fs::write(home.join(".config/forge/config.toml"), config(port, protocol)).unwrap();

    let mut agent = Agent::start(&home, None);
    let mut carried: Vec<Value> = Vec::new();
    let outcome = case(mode, &mut agent, &provider, &home, &mut carried);

    agent.close();
    carried.extend(agent.snapshot());
    let _ = std::fs::write(
        home.join("events.json"),
        serde_json::to_string_pretty(&carried).unwrap_or_default(),
    );
    let _ = std::fs::write(
        home.join("requests.json"),
        serde_json::to_string_pretty(&*provider.requests.lock().unwrap()).unwrap_or_default(),
    );

    if let Err(why) = outcome {
        panic!("{protocol}/{mode}: {why}\nevidence: {}", home.display());
    }
}

fn case(
    mode: &str,
    agent: &mut Agent,
    provider: &Provider,
    home: &Path,
    carried: &mut Vec<Value>,
) -> Result<(), String> {
    let init = agent.until(&["init"])?;

    // The guard that keeps this test loopback-only: if the agent did not load
    // the fixture endpoint, something else is configured and nothing is sent.
    let model = init.get("model_id").and_then(Value::as_str).unwrap_or_default();
    if model != "failure-fixture" {
        return Err(format!(
            "refusing to send: agent loaded {model:?}, not the isolated fixture"
        ));
    }

    agent.send(json!({
        "type": "send_message",
        "content": "Exercise the failure fixture. Do not use external services."
    }));

    match mode {
        "cancel" => {
            agent.until(&["assistant_token"])?;
            agent.send(json!({"type": "cancel_run"}));
            let end = agent.until(&["cancelled", "error", "done"])?;
            expect(&end, "cancelled")?;
        }
        "restart" => {
            agent.until(&["assistant_token"])?;
            let session = init
                .get("session_id")
                .and_then(Value::as_str)
                .ok_or("init carried no session_id to resume")?
                .to_string();
            let _ = agent.child.kill();
            let _ = agent.child.wait();
            carried.extend(agent.snapshot());
            agent.close();

            *agent = Agent::start(home, Some(&session));
            agent.until(&["init"])?;
            let resumed = agent.until(&["session_loaded"])?;
            let count = resumed.get("message_count").and_then(Value::as_u64).unwrap_or(0);
            if count < 1 {
                return Err(format!("resumed session remembered nothing: {resumed}"));
            }
        }
        _ => {
            let end = agent.until(&["done", "error", "cancelled"])?;
            let faults_are_errors = matches!(
                mode,
                "eof" | "reset" | "output_limit" | "truncated_tool" | "stall" | "http429" | "context_limit"
            );
            expect(&end, if faults_are_errors { "error" } else { "done" })?;

            if mode == "truncated_tool" && agent.saw(|e| e["type"] == "tool_request") {
                return Err("a tool call with truncated arguments was dispatched".into());
            }
            if mode == "tool_failure"
                && !agent.saw(|e| e["type"] == "tool_result" && e["success"] == json!(false))
            {
                return Err("the failed tool was not reported as a failure".into());
            }
            if mode == "http503" && provider.requests.lock().unwrap().len() < 2 {
                return Err("a transient server failure was not retried".into());
            }
        }
    }

    // Whatever broke, the session must still take another turn.
    *provider.mode.lock().unwrap() = "ok".to_string();
    agent.send(json!({"type": "send_message", "content": "Recover now."}));
    let end = agent.until(&["done", "error", "cancelled"])?;
    expect(&end, "done")?;
    if !agent.saw(|e| e.get("content").and_then(Value::as_str) == Some("RECOVERED")) {
        return Err("the recovery turn produced no answer".into());
    }

    // A partial answer is worth keeping, but only once: replaying it on every
    // subsequent request would quietly duplicate it into the transcript.
    if matches!(mode, "eof" | "reset" | "output_limit" | "stall") {
        let requests = provider.requests.lock().unwrap();
        let last = requests.last().ok_or("no request to inspect")?;
        let messages = last
            .get("messages")
            .and_then(Value::as_array)
            .ok_or("the last request carried no messages")?;
        let partials = messages
            .iter()
            .filter(|m| {
                m.get("role").and_then(Value::as_str) == Some("assistant")
                    && m.get("content").map(|c| c.to_string()).unwrap_or_default().contains("PARTIAL-FIXTURE")
            })
            .count();
        if partials != 1 {
            return Err(format!("the partial answer survived {partials} times, not once"));
        }
    }
    Ok(())
}

fn expect(message: &Value, kind: &str) -> Result<(), String> {
    let got = message.get("type").and_then(Value::as_str).unwrap_or("?");
    if got == kind {
        Ok(())
    } else {
        Err(format!("expected {kind}, got {got}: {message}"))
    }
}

// ── The matrix ──────────────────────────────────────────────────────────────

macro_rules! cases {
    ($($name:ident: $mode:literal,)*) => {
        $(
            mod $name {
                #[test]
                fn open_ai() { super::run($mode, "open_ai"); }
                #[test]
                fn anthropic() { super::run($mode, "anthropic"); }
            }
        )*
    };
}

cases! {
    incomplete_eof: "eof",
    broken_chunked_transport: "reset",
    output_limit: "output_limit",
    truncated_tool_arguments: "truncated_tool",
    failing_file_tool: "tool_failure",
    transient_503_is_retried: "http503",
    rate_limited_429: "http429",
    context_window_rejection: "context_limit",
    stalled_stream: "stall",
    cancellation: "cancel",
    kill_and_resume: "restart",
}
