//! Forge remote protocol — JSON-RPC messages shared by forge-ide (client)
//! and forge-server (server).  Framed identically to LSP: each message is
//! preceded by `Content-Length: N\r\n\r\n`.
//!
//! Design: one bidirectional channel (SSH exec stdio).  Client sends
//! requests/notifications; server sends responses and push notifications
//! (PTY output, LSP relay).

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Wire envelope ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rpc {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id:      Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method:  Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params:  Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result:  Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error:   Option<RpcError>,
}

impl Rpc {
    pub fn request(id: i64, method: &str, params: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id: Some(id),
               method: Some(method.into()), params: Some(params),
               result: None, error: None }
    }
    pub fn notify(method: &str, params: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id: None,
               method: Some(method.into()), params: Some(params),
               result: None, error: None }
    }
    pub fn ok(id: i64, result: Value) -> Self {
        Self { jsonrpc: "2.0".into(), id: Some(id),
               method: None, params: None, result: Some(result), error: None }
    }
    pub fn err(id: i64, code: i32, msg: &str) -> Self {
        Self { jsonrpc: "2.0".into(), id: Some(id),
               method: None, params: None, result: None,
               error: Some(RpcError { code, message: msg.into() }) }
    }
    pub fn is_request(&self)      -> bool { self.id.is_some() && self.method.is_some() }
    pub fn is_notification(&self) -> bool { self.id.is_none() && self.method.is_some() }
    pub fn is_response(&self)     -> bool { self.id.is_some() && self.method.is_none() }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RpcError { pub code: i32, pub message: String }

// ── fs/ ───────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsListParams  { pub path: String }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsEntry {
    pub name:   String,
    pub path:   String,
    pub is_dir: bool,
    pub size:   u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsReadParams  { pub path: String }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FsWriteParams { pub path: String, pub text: String }

// ── lsp/ ──────────────────────────────────────────────────────────────────────

/// Client → server: start a language server for a language/root.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspStartParams {
    pub lang: String,   // "rust", "typescript", …
    pub root: String,   // remote workspace root
}

/// Client → server: relay an LSP message to the running server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspSendParams {
    pub lang: String,
    pub data: String, // raw JSON-RPC body (already Content-Length stripped)
}

/// Server → client push: LSP message from the language server.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LspDataPush {
    pub lang: String,
    pub data: String,
}

// ── pty/ ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyOpenParams {
    pub id:   u32,
    pub cols: u16,
    pub rows: u16,
    pub cwd:  String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyWriteParams  { pub id: u32, pub data: Vec<u8> }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyResizeParams { pub id: u32, pub cols: u16, pub rows: u16 }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyCloseParams  { pub id: u32 }

/// Server → client push: bytes from a PTY session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyDataPush { pub id: u32, pub data: Vec<u8> }

/// Existing PTY session, as reported by `pty/list` — lets a client that
/// just (re)connected (e.g. after Forge IDE's own process restarted, while
/// a local pty-host daemon kept running underneath it) discover sessions
/// that are already alive instead of assuming there are none.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PtyInfo { pub id: u32, pub cwd: String, pub cols: u16, pub rows: u16 }

// ── Framing helpers (shared by client and server) ─────────────────────────────

use std::io::{BufRead, Write};

pub fn write_rpc(w: &mut impl Write, msg: &Rpc) -> std::io::Result<()> {
    let body = serde_json::to_vec(msg)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

pub fn read_rpc(r: &mut impl BufRead) -> Option<Rpc> {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 { return None; }
        let t = line.trim_end();
        if t.is_empty() { break; }
        if let Some(rest) = t.strip_prefix("Content-Length:") {
            len = rest.trim().parse().ok()?;
        }
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}
