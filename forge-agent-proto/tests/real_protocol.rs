// SPDX-License-Identifier: Apache-2.0
//! Validation against what the agent actually emits.
//!
//! The unit tests in `lib.rs` check these types against themselves, which proves
//! only internal consistency — a mirror can be perfectly self-consistent and
//! still wrong about the thing it mirrors. These tests compare against real
//! bytes.
//!
//! Two levels:
//!
//!  * [`every_line_of_real_startup_output_is_recognised`] runs always, against a
//!    fixture captured from a real `forge-agent --headless` startup. Values are
//!    replaced with placeholders (no real paths, endpoint URLs, model names or
//!    session ids); the *shape* is byte-for-byte what the agent produced.
//!  * [`live_agent_output_is_recognised`] is `#[ignore]`d and spawns the actual
//!    binary, so drift can be caught against a build rather than a snapshot.
//!    Run it with:
//!
//!    ```text
//!    cargo test -p forge-agent-proto --test real_protocol -- --ignored --nocapture
//!    ```

use forge_agent_proto::{AgentLine, AgentMessage, ClientMessage};

const STARTUP_FIXTURE: &str = include_str!("fixtures/agent_startup.jsonl");

/// Every line the real agent sent at startup must land on a known variant.
///
/// An `Unknown` here means the types are missing something the agent sends,
/// which is the failure the zod schemas used to hit at runtime instead.
#[test]
fn every_line_of_real_startup_output_is_recognised() {
    let mut tags = Vec::new();

    for (i, line) in STARTUP_FIXTURE.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let parsed = AgentLine::parse(line)
            .unwrap_or_else(|e| panic!("line {i} is not valid JSON: {e}\n{line}"));

        match parsed {
            AgentLine::Known(msg) => tags.push(msg.tag().to_string()),
            AgentLine::Unknown(v) => panic!(
                "line {i} was not recognised — the protocol types are missing a variant \
                 or a field.\n  tag: {:?}\n  raw: {v}",
                v.get("type"),
            ),
        }
    }

    // The agent's opening sequence. Pinned so a change to what it sends at
    // startup surfaces here rather than as a confused client.
    assert_eq!(
        tags,
        vec!["init", "usage_update", "usage"],
        "the real startup sequence changed",
    );
}

/// The fields that drive the opening screen must survive the round trip, not
/// merely parse. A silently-defaulted `max_context_tokens` would show a wrong
/// context bar rather than fail.
#[test]
fn init_carries_the_fields_the_client_renders() {
    let first = STARTUP_FIXTURE.lines().next().expect("fixture has an init line");
    let init = match AgentLine::parse(first).unwrap() {
        AgentLine::Known(AgentMessage::Init(init)) => init,
        other => panic!("first line should be init, got {other:?}"),
    };

    assert!(!init.project_root.is_empty(), "project_root drives the header");
    assert!(!init.model_name.is_empty(), "model_name drives the header");
    assert!(!init.model_id.is_empty());
    assert!(init.max_context_tokens > 0, "context bar needs a real window");
    assert!(!init.log_path.is_empty());
    assert!(!init.context_strategy.is_empty());

    // Real installs have these populated; empty would mean the lists silently
    // failed to deserialize.
    assert!(!init.endpoints.is_empty(), "endpoints list came through");
    assert!(!init.agent_definitions.is_empty(), "agent definitions came through");
    assert!(!init.available_tools.is_empty(), "tool list came through");

    // Nested types resolved rather than falling back to defaults for a
    // deserialization failure we would otherwise not notice.
    let ep = &init.endpoints[0];
    assert!(!ep.name.is_empty());
    assert!(!ep.endpoint_type.is_empty());
    assert!(ep.max_output_tokens > 0);
}

/// The usage snapshot feeds the context bar, so its numbers must arrive intact.
#[test]
fn usage_snapshot_deserializes_with_real_numbers() {
    let line = STARTUP_FIXTURE
        .lines()
        .find(|l| l.contains(r#""type":"usage""#))
        .expect("fixture has a usage line");

    match AgentLine::parse(line).unwrap() {
        AgentLine::Known(AgentMessage::Usage { snapshot }) => {
            assert!(snapshot.max_context_tokens > 0, "window size came through");
            // Fraction must be finite for layout arithmetic downstream.
            let f = snapshot.context_fraction();
            assert!(f.is_finite() && (0.0..=1.0).contains(&f), "fraction was {f}");
        }
        other => panic!("expected usage, got {other:?}"),
    }
}

/// Spawn the real binary and check its output against these types.
///
/// Ignored by default: it needs a built `forge-agent` and touches the developer's
/// own configuration. This is the test that catches drift after the agent
/// changes, which a committed fixture cannot.
#[test]
#[ignore = "spawns the real forge-agent binary; run with --ignored"]
fn live_agent_output_is_recognised() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    let binary = std::env::var("FORGE_AGENT_BIN").unwrap_or_else(|_| {
        // target/debug/ relative to this crate, i.e. the workspace build.
        concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/forge-agent").to_string()
    });
    assert!(
        std::path::Path::new(&binary).exists(),
        "{binary} not found — `cargo build -p forge-agent` first, \
         or set FORGE_AGENT_BIN",
    );

    // A scratch cwd, so the agent does not write session logs into the repo.
    let cwd = std::env::temp_dir().join("forge-agent-proto-live-test");
    std::fs::create_dir_all(&cwd).expect("scratch dir");

    let mut child = Command::new(&binary)
        .arg("--headless")
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn forge-agent");

    let stdout = child.stdout.take().expect("piped stdout");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // Ask for something that always answers, then quit.
    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        let _ = stdin.write_all(ClientMessage::RequestUsage.to_line().as_bytes());
        let _ = stdin.flush();
    }

    let mut lines = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(line) => lines.push(line),
            Err(mpsc::RecvTimeoutError::Timeout) if !lines.is_empty() => break,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    {
        let stdin = child.stdin.as_mut().expect("piped stdin");
        let _ = stdin.write_all(ClientMessage::Quit.to_line().as_bytes());
        let _ = stdin.flush();
    }
    let _ = child.wait();

    assert!(!lines.is_empty(), "the agent produced no output");

    let mut unknown = Vec::new();
    let mut seen = Vec::new();
    for line in &lines {
        match AgentLine::parse(line) {
            Ok(AgentLine::Known(msg)) => seen.push(msg.tag().to_string()),
            Ok(AgentLine::Unknown(v)) => {
                unknown.push(v.get("type").and_then(|t| t.as_str()).unwrap_or("?").to_string());
            }
            Err(e) => panic!("agent emitted a line that is not JSON: {e}\n{line}"),
        }
    }

    println!("recognised: {seen:?}");
    assert!(
        unknown.is_empty(),
        "the live agent sent variants these types do not cover: {unknown:?}",
    );
    assert!(seen.contains(&"init".to_string()), "init should always arrive");
}
