//! LSP client — transport, diagnostics, hover, completions, go-to-definition.
//!
//! Threading model:
//!   • Main thread writes (notify + request) via Arc<Mutex<ChildStdin>>.
//!   • Reader thread drains stdout: auto-replies to server→client requests
//!     (workspace/configuration etc.), sends everything else (notifications
//!     and responses) to the main thread on a single mpsc channel.
//!   • Main thread's `poll()` drains that channel each frame, splitting into
//!     - notifications  → diagnostics map
//!     - responses      → pending_requests map (keyed by id)
//!   • Callers check `take_response(id)` to pick up a result.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};

use serde_json::{json, Value};

// ── Public types ────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub start_line: u32,
    pub start_col:  u32,
    pub end_line:   u32,
    pub end_col:    u32,
    pub severity:   u8, // 1 = error  2 = warning  3 = info  4 = hint
    pub message:    String,
}

#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label:         String,
    pub kind:          u8,    // LSP CompletionItemKind (1=text,2=method,6=variable…)
    pub detail:        String, // type signature / short doc
    pub insert_text:   String, // text to actually insert (may differ from label)
}

#[derive(Clone, Debug)]
pub struct Location {
    pub path:       PathBuf,
    pub start_line: u32,
    pub start_col:  u32,
}

// ── Client ──────────────────────────────────────────────────────────────────

pub struct LspClient {
    child:            Child,
    stdin:            Arc<Mutex<ChildStdin>>,
    rx:               mpsc::Receiver<Value>,
    next_id:          i64,
    versions:         HashMap<PathBuf, i64>,
    pending_requests: HashMap<i64, Value>, // id → response (once arrived)
}

impl LspClient {
    pub fn start(root: &Path) -> Option<Self> {
        let mut child = Command::new("rust-analyzer")
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;

        let stdin  = Arc::new(Mutex::new(child.stdin.take()?));
        let mut reader = BufReader::new(child.stdout.take()?);

        // ── initialize handshake (synchronous) ──────────────────────────
        let root_uri = path_to_uri(root);
        let init = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "workspaceFolders": [{ "uri": root_uri, "name": "root" }],
                "capabilities": {
                    "workspace": { "configuration": true, "workspaceFolders": true },
                    "textDocument": {
                        "synchronization": { "didSave": true },
                        "publishDiagnostics": { "relatedInformation": false },
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "completion": {
                            "completionItem": { "snippetSupport": false }
                        },
                        "definition": {},
                        "references": {},
                        "rename": {},
                        "codeAction": { "codeActionLiteralSupport": {
                            "codeActionKind": { "valueSet": ["", "quickfix", "refactor"] }
                        }},
                        "signatureHelp": {
                            "signatureInformation": {
                                "parameterInformation": { "labelOffsetSupport": true }
                            }
                        },
                        "formatting": {}
                    }
                }
            }
        });
        write_msg(&mut *stdin.lock().ok()?, &init).ok()?;
        loop {
            let msg = read_msg(&mut reader)?;
            let id_matches = msg.get("id").and_then(|v| v.as_i64()) == Some(1);
            if id_matches && (msg.get("result").is_some() || msg.get("error").is_some()) {
                break;
            }
        }
        write_msg(&mut *stdin.lock().ok()?,
            &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} })).ok()?;

        // ── reader thread ────────────────────────────────────────────────
        let (tx, rx) = mpsc::channel::<Value>();
        let stdin2   = Arc::clone(&stdin);
        std::thread::spawn(move || {
            while let Some(msg) = read_msg(&mut reader) {
                // Server→client requests: auto-reply so the server doesn't stall.
                let is_server_req = msg.get("id").is_some() && msg.get("method").is_some();
                if is_server_req {
                    let id  = msg["id"].clone();
                    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let result = if method == "workspace/configuration" {
                        let n = msg.pointer("/params/items")
                            .and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
                        Value::Array(vec![Value::Null; n])
                    } else { Value::Null };
                    if let Ok(mut w) = stdin2.lock() {
                        let _ = write_msg(&mut *w,
                            &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                    }
                    continue;
                }
                if tx.send(msg).is_err() { break; }
            }
        });

        Some(Self { child, stdin, rx, next_id: 2,
                    versions: HashMap::new(), pending_requests: HashMap::new() })
    }

    // ── Document sync ───────────────────────────────────────────────────────

    pub fn did_open(&mut self, path: &Path, text: &str, lang: &str) {
        if self.versions.contains_key(path) { return; }
        self.versions.insert(path.to_path_buf(), 1);
        self.notify("textDocument/didOpen", json!({
            "textDocument": {
                "uri": path_to_uri(path), "languageId": lang,
                "version": 1, "text": text,
            }
        }));
    }

    pub fn did_change(&mut self, path: &Path, text: &str) {
        let Some(v) = self.versions.get_mut(path) else { return };
        *v += 1;
        let ver = *v;
        self.notify("textDocument/didChange", json!({
            "textDocument": { "uri": path_to_uri(path), "version": ver },
            "contentChanges": [{ "text": text }],
        }));
    }

    // ── Requests (fire-and-forget; result arrives via poll + take_response) ──

    /// Request hover info at (line, col).  Returns the id to poll for.
    pub fn hover(&mut self, path: &Path, line: u32, col: u32) -> i64 {
        self.request("textDocument/hover", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": col }
        }))
    }

    /// Request completions at (line, col).  Returns the id to poll for.
    pub fn completions(&mut self, path: &Path, line: u32, col: u32) -> i64 {
        self.request("textDocument/completion", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": col },
            "context": { "triggerKind": 1 }
        }))
    }

    /// Request go-to-definition at (line, col).  Returns the id to poll for.
    pub fn goto_def(&mut self, path: &Path, line: u32, col: u32) -> i64 {
        self.request("textDocument/definition", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": col }
        }))
    }

    /// Request all references for the symbol at (line, col).
    pub fn references(&mut self, path: &Path, line: u32, col: u32) -> i64 {
        self.request("textDocument/references", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": col },
            "context": { "includeDeclaration": true }
        }))
    }

    /// Request a rename of the symbol at (line, col) to `new_name`.
    pub fn rename(&mut self, path: &Path, line: u32, col: u32, new_name: &str) -> i64 {
        self.request("textDocument/rename", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": col },
            "newName": new_name
        }))
    }

    /// Request code actions for the cursor range.
    pub fn code_actions(&mut self, path: &Path, line: u32, col: u32,
                        diags: &[crate::lsp::Diagnostic]) -> i64 {
        let lsp_diags: Vec<Value> = diags.iter().filter(|d| {
            line >= d.start_line && line <= d.end_line
        }).map(|d| json!({
            "range": {
                "start": { "line": d.start_line, "character": d.start_col },
                "end":   { "line": d.end_line,   "character": d.end_col   }
            },
            "severity": d.severity,
            "message":  d.message
        })).collect();
        self.request("textDocument/codeAction", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "range": {
                "start": { "line": line, "character": col },
                "end":   { "line": line, "character": col }
            },
            "context": { "diagnostics": lsp_diags, "only": Value::Null }
        }))
    }

    /// Request signature help (triggered by typing `(`).
    pub fn signature_help(&mut self, path: &Path, line: u32, col: u32) -> i64 {
        self.request("textDocument/signatureHelp", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "position": { "line": line, "character": col },
            "context": { "triggerKind": 2, "triggerCharacter": "(" }
        }))
    }

    /// Request the document symbol tree (outline view).
    pub fn document_symbols(&mut self, path: &Path) -> i64 {
        self.request("textDocument/documentSymbol", json!({
            "textDocument": { "uri": path_to_uri(path) }
        }))
    }

    /// Request full-document formatting.
    pub fn formatting(&mut self, path: &Path) -> i64 {
        self.request("textDocument/formatting", json!({
            "textDocument": { "uri": path_to_uri(path) },
            "options": { "tabSize": 4, "insertSpaces": true }
        }))
    }

    /// Drain the incoming channel.  Diagnostics are merged into `diags`;
    /// responses are stashed in `pending_requests`.  Returns true if any
    /// diagnostics changed (so caller can request a repaint).
    pub fn poll(&mut self, diags: &mut HashMap<PathBuf, Vec<Diagnostic>>) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            if let Some(method) = msg.get("method").and_then(|m| m.as_str()) {
                // Notification from server.
                if method == "textDocument/publishDiagnostics" {
                    if let Some(path) = msg.pointer("/params/uri")
                        .and_then(|u| u.as_str()).and_then(uri_to_path)
                    {
                        let list = msg.pointer("/params/diagnostics")
                            .and_then(|d| d.as_array())
                            .map(|a| a.iter().filter_map(parse_diag).collect())
                            .unwrap_or_default();
                        diags.insert(path, list);
                        changed = true;
                    }
                }
            } else if let Some(id) = msg.get("id").and_then(|i| i.as_i64()) {
                // Response to one of our requests.
                self.pending_requests.insert(id, msg);
            }
        }
        changed
    }

    /// Check if a response arrived for `id`.  Consumes it if so.
    pub fn take_response(&mut self, id: i64) -> Option<Value> {
        self.pending_requests.remove(&id)
    }

    // ── Helpers ─────────────────────────────────────────────────────────────

    fn notify(&self, method: &str, params: Value) {
        if let Ok(mut w) = self.stdin.lock() {
            let _ = write_msg(&mut *w,
                &json!({ "jsonrpc": "2.0", "method": method, "params": params }));
        }
    }

    fn request(&mut self, method: &str, params: Value) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        if let Ok(mut w) = self.stdin.lock() {
            let _ = write_msg(&mut *w,
                &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        }
        id
    }
}

impl Drop for LspClient {
    fn drop(&mut self) { let _ = self.child.kill(); }
}

// ── Parse helpers ───────────────────────────────────────────────────────────

fn parse_diag(v: &Value) -> Option<Diagnostic> {
    let s = v.pointer("/range/start")?;
    let e = v.pointer("/range/end")?;
    Some(Diagnostic {
        start_line: s["line"].as_u64()? as u32,
        start_col:  s["character"].as_u64()? as u32,
        end_line:   e["line"].as_u64()? as u32,
        end_col:    e["character"].as_u64()? as u32,
        severity:   v.get("severity").and_then(|x| x.as_u64()).unwrap_or(1) as u8,
        message:    v["message"].as_str().unwrap_or("").to_string(),
    })
}

/// One row of the flattened outline tree.
#[derive(Clone, Debug)]
pub struct DocSymbol {
    pub name:  String,
    pub kind:  u8,   // LSP SymbolKind
    pub line:  u32,
    pub col:   u32,
    pub depth: usize,
}

/// Parse a textDocument/documentSymbol response (either DocumentSymbol[]
/// with children, or flat SymbolInformation[]).
pub fn parse_document_symbols(result: &Value) -> Vec<DocSymbol> {
    let mut out = Vec::new();
    if let Some(arr) = result.get("result").and_then(|r| r.as_array()) {
        for v in arr { collect_symbol(v, 0, &mut out); }
    }
    out
}

fn collect_symbol(v: &Value, depth: usize, out: &mut Vec<DocSymbol>) {
    let start = v.pointer("/selectionRange/start")
        .or_else(|| v.pointer("/location/range/start"));
    let line = start.and_then(|s| s.get("line")).and_then(|l| l.as_u64()).unwrap_or(0) as u32;
    let col  = start.and_then(|s| s.get("character")).and_then(|c| c.as_u64()).unwrap_or(0) as u32;
    out.push(DocSymbol {
        name:  v.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string(),
        kind:  v.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u8,
        line, col, depth,
    });
    if let Some(children) = v.get("children").and_then(|c| c.as_array()) {
        for c in children { collect_symbol(c, depth + 1, out); }
    }
}

/// Extract human-readable hover text from an LSP Hover result.
pub fn parse_hover(result: &Value) -> Option<String> {
    let contents = result.get("result")?.get("contents")?;
    // MarkupContent  { kind, value }
    if let Some(s) = contents.get("value").and_then(|v| v.as_str()) {
        return Some(strip_markdown(s));
    }
    // MarkedString   { language, value }
    if let Some(s) = contents.get("value").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    // Plain string
    if let Some(s) = contents.as_str() { return Some(s.to_string()); }
    // Array of any of the above
    if let Some(arr) = contents.as_array() {
        let parts: Vec<_> = arr.iter().filter_map(|e| {
            e.get("value").and_then(|v| v.as_str())
                .or_else(|| e.as_str())
        }).collect();
        if !parts.is_empty() { return Some(parts.join("\n")); }
    }
    None
}

/// Parse a completion list response into items.
pub fn parse_completions(result: &Value) -> Vec<CompletionItem> {
    let items = result.pointer("/result/items")
        .or_else(|| result.get("result"))
        .and_then(|v| v.as_array());
    let Some(items) = items else { return vec![] };
    items.iter().filter_map(|v| {
        let label = v["label"].as_str()?.to_string();
        let insert_text = v.get("insertText").and_then(|t| t.as_str())
            .unwrap_or(&label).to_string();
        let detail = v.get("detail").and_then(|d| d.as_str())
            .unwrap_or("").to_string();
        let kind = v.get("kind").and_then(|k| k.as_u64()).unwrap_or(1) as u8;
        Some(CompletionItem { label, kind, detail, insert_text })
    }).take(20).collect() // cap at 20 for the dropdown
}

/// Parse a references response into a list of Locations.
pub fn parse_references(result: &Value) -> Vec<Location> {
    let arr = result.get("result").and_then(|r| r.as_array());
    let Some(arr) = arr else { return vec![] };
    arr.iter().filter_map(|loc| {
        let uri  = loc.pointer("/uri")?.as_str()?;
        let s    = loc.pointer("/range/start")?;
        Some(Location {
            path:       uri_to_path(uri)?,
            start_line: s["line"].as_u64()? as u32,
            start_col:  s["character"].as_u64()? as u32,
        })
    }).collect()
}

/// One text edit to apply (LSP TextEdit).
#[derive(Clone, Debug)]
pub struct TextEdit {
    pub start_line: u32,
    pub start_col:  u32,
    pub end_line:   u32,
    pub end_col:    u32,
    pub new_text:   String,
}

/// Parse a formatting / rename response into TextEdits.
/// Formatting returns `[TextEdit]`; rename returns a WorkspaceEdit
/// with `{ changes: { uri: [TextEdit] } }` or `{ documentChanges: [...] }`.
pub fn parse_text_edits(result: &Value, for_uri: Option<&str>) -> Vec<TextEdit> {
    // Direct array (formatting)
    if let Some(arr) = result.get("result").and_then(|r| r.as_array()) {
        return arr.iter().filter_map(parse_one_edit).collect();
    }
    // WorkspaceEdit → changes map (rename)
    if let Some(changes) = result.pointer("/result/changes") {
        if let Some(obj) = changes.as_object() {
            let uri = for_uri.unwrap_or("");
            if let Some(arr) = obj.get(uri).and_then(|v| v.as_array()) {
                return arr.iter().filter_map(parse_one_edit).collect();
            }
            // Return all edits if no specific URI given
            return obj.values()
                .filter_map(|v| v.as_array())
                .flat_map(|a| a.iter().filter_map(parse_one_edit))
                .collect();
        }
    }
    // WorkspaceEdit → documentChanges (newer protocol)
    if let Some(doc_changes) = result.pointer("/result/documentChanges") {
        if let Some(arr) = doc_changes.as_array() {
            return arr.iter().flat_map(|dc| {
                let uri = dc.pointer("/textDocument/uri").and_then(|u| u.as_str());
                let matches = for_uri.map_or(true, |f| uri == Some(f));
                if matches {
                    dc.get("edits").and_then(|e| e.as_array())
                        .map(|a| a.iter().filter_map(parse_one_edit).collect::<Vec<_>>())
                        .unwrap_or_default()
                } else { vec![] }
            }).collect();
        }
    }
    vec![]
}

fn parse_one_edit(v: &Value) -> Option<TextEdit> {
    let s = v.pointer("/range/start")?;
    let e = v.pointer("/range/end")?;
    Some(TextEdit {
        start_line: s["line"].as_u64()? as u32,
        start_col:  s["character"].as_u64()? as u32,
        end_line:   e["line"].as_u64()? as u32,
        end_col:    e["character"].as_u64()? as u32,
        new_text:   v["newText"].as_str().unwrap_or("").to_string(),
    })
}

/// Apply a sorted list of TextEdits to a line vec in-place.
/// Edits must be applied back-to-front (high line first) to keep offsets valid.
pub fn apply_edits(lines: &mut Vec<String>, mut edits: Vec<TextEdit>) {
    // Sort descending so later edits don't invalidate earlier positions.
    edits.sort_by(|a, b| b.start_line.cmp(&a.start_line)
        .then(b.start_col.cmp(&a.start_col)));
    for ed in &edits {
        let sl = ed.start_line as usize;
        let el = ed.end_line   as usize;
        if sl >= lines.len() { continue; }
        let sc = ed.start_col as usize;
        let ec = ed.end_col   as usize;
        if sl == el {
            // Single-line edit
            let line = &lines[sl];
            let sc = sc.min(line.len());
            let ec = ec.min(line.len());
            let new = format!("{}{}{}", &line[..sc], ed.new_text, &line[ec..]);
            lines[sl] = new;
        } else {
            // Multi-line edit: collect head and tail, splice.
            let head = {
                let l = &lines[sl];
                let sc = sc.min(l.len());
                format!("{}{}", &l[..sc], ed.new_text)
            };
            let tail = {
                let l = &lines[el.min(lines.len()-1)];
                let ec = ec.min(l.len());
                l[ec..].to_string()
            };
            let end_el = el.min(lines.len().saturating_sub(1));
            lines.drain(sl..=end_el);
            let combined = format!("{}{}", head, tail);
            for (i, part) in combined.split('\n').enumerate() {
                lines.insert(sl + i, part.to_string());
            }
        }
    }
}

/// Signature help result: active parameter highlighted within the label.
#[derive(Clone, Debug)]
pub struct SignatureHelp {
    pub label:         String, // full signature label
    pub param_label:   Option<String>, // extracted param name/type
}

pub fn parse_signature_help(result: &Value) -> Option<SignatureHelp> {
    let sigs = result.pointer("/result/signatures")?.as_array()?;
    if sigs.is_empty() { return None; }
    let active_sig = result.pointer("/result/activeSignature")
        .and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let sig = sigs.get(active_sig).or_else(|| sigs.first())?;
    let label = sig["label"].as_str()?.to_string();
    let active_param: Option<usize> = result.pointer("/result/activeParameter")
        .or_else(|| sig.get("activeParameter"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let param_label: Option<String> = active_param.and_then(|ap| {
        let params = sig.get("parameters")?.as_array()?;
        let param: &Value = params.get(ap)?;
        // paramLabel can be a string or [start, end] offsets
        if let Some(s) = param.pointer("/label").and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
        if let Some(arr) = param.pointer("/label").and_then(|v| v.as_array()) {
            let s = arr.first()?.as_u64()? as usize;
            let e = arr.get(1)?.as_u64()? as usize;
            return Some(label.get(s..e)?.to_string());
        }
        None
    });
    Some(SignatureHelp { label, param_label })
}

/// A code action (quick-fix or refactor).
#[derive(Clone, Debug)]
pub struct CodeAction {
    pub title:  String,
    /// Top-level LSP `CodeActionKind` (e.g. "refactor.extract") — groups the
    /// lightbulb dropdown into "Quick Fix"/"Refactor"/"Source Action".
    pub kind:   String,
    /// Raw value kept so we can resolve/execute it later.
    pub raw:    Value,
}

pub fn parse_code_actions(result: &Value) -> Vec<CodeAction> {
    let arr = result.get("result").and_then(|r| r.as_array());
    let Some(arr) = arr else { return vec![] };
    arr.iter().filter_map(|v| {
        // Can be a Command or a CodeAction
        let title = v.get("title")?.as_str()?.to_string();
        let kind  = v.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
        Some(CodeAction { title, kind, raw: v.clone() })
    }).collect()
}

/// Parse a definition response into a Location.
pub fn parse_goto(result: &Value) -> Option<Location> {
    // response is either a Location or [Location]; take the first.
    let loc = result.get("result").and_then(|r| {
        if r.is_array() { r.as_array()?.first() } else { Some(r) }
    })?;
    let uri  = loc.pointer("/uri")?.as_str()?;
    let s    = loc.pointer("/range/start")?;
    Some(Location {
        path:       uri_to_path(uri)?,
        start_line: s["line"].as_u64()? as u32,
        start_col:  s["character"].as_u64()? as u32,
    })
}

fn strip_markdown(s: &str) -> String {
    // Strategy: show the first code-fence block (type signature) + up to one
    // doc sentence.  rust-analyzer hovers are structured as:
    //   ```rust\n<type sig>\n```\n\n<doc text>
    // We want the type sig on line 1, then optionally one short doc line.
    let mut sig   = String::new();
    let mut doc   = String::new();
    let mut in_fence = false;
    let mut found_fence = false;

    for line in s.lines() {
        if line.starts_with("```") {
            if !in_fence && !found_fence {
                in_fence    = true;
                found_fence = true;
            } else {
                in_fence = false; // closing fence
            }
            continue;
        }
        if in_fence {
            // Type signature lines — join with space, strip leading whitespace.
            if !sig.is_empty() { sig.push(' '); }
            sig.push_str(line.trim());
        } else if found_fence && !in_fence && doc.is_empty() {
            // First non-empty doc line after the code fence.
            let clean = line.replace(['*', '_', '#'], "").trim().to_string();
            if !clean.is_empty() { doc = clean; }
        }
    }

    // Fallback: no fence found — return plain text, first 120 chars.
    if sig.is_empty() {
        let plain = s.replace(['*', '_', '#'], "");
        let trimmed = plain.trim();
        return if trimmed.len() > 120 {
            format!("{}…", &trimmed[..120])
        } else {
            trimmed.to_string()
        };
    }

    // Truncate the signature if it's very long.
    let sig_display = if sig.len() > 100 { format!("{}…", &sig[..100]) } else { sig };

    if doc.is_empty() {
        sig_display
    } else {
        // Cap doc line at 80 chars.
        let doc_display = if doc.len() > 80 { format!("{}…", &doc[..80]) } else { doc };
        format!("{}\n{}", sig_display, doc_display)
    }
}

// ── JSON-RPC framing ─────────────────────────────────────────────────────────

fn write_msg(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(v)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

fn read_msg(r: &mut impl BufRead) -> Option<Value> {
    let mut len = 0usize;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 { return None; }
        let line = line.trim_end();
        if line.is_empty() { break; }
        if let Some(rest) = line.strip_prefix("Content-Length:") {
            len = rest.trim().parse().ok()?;
        }
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

// ── URI helpers ───────────────────────────────────────────────────────────────

pub fn path_to_uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for ch in path.to_string_lossy().chars() {
        match ch {
            ' ' => s.push_str("%20"),
            '#' => s.push_str("%23"),
            '?' => s.push_str("%3F"),
            _   => s.push(ch),
        }
    }
    s
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i+1] as char).to_digit(16);
            let lo = (bytes[i+2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3; continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(String::from_utf8_lossy(&out).into_owned()))
}
