//! forge-server — speaks JSON-RPC (forge-proto). Two modes:
//!  - default (stdio): spawned over an SSH exec channel for remote workspaces.
//!  - `--listen <socket>`: a local daemon Forge IDE spawns once and reconnects
//!    to across its own restarts, so local terminal PTYs (and whatever's
//!    running inside them) survive a "Reload Window" instead of being killed
//!    with the old process.

mod fs;
mod lsp;
mod pty;

use std::io::{BufReader, Write, stdin, stdout};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use forge_proto::*;
use serde_json::{json, Value};

// Shared, mutex-protected stdout — used directly by handlers and push callbacks.
type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("forge-server {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    let lsp_sessions: Arc<Mutex<HashMap<String, lsp::LspProxy>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pty_sessions: Arc<Mutex<HashMap<u32, pty::PtySession>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // `--listen <socket>`: local pty-host daemon mode. Unlike the SSH-exec
    // stdio mode below, sessions (`pty_sessions`) and the push-notification
    // writer are created *once* and persist across however many client
    // connections come and go — the whole point being that Forge IDE's own
    // process can restart (e.g. "Reload Window") and reconnect to the same
    // running shells instead of killing them. See `listen_forever`.
    if let Some(pos) = args.iter().position(|a| a == "--listen") {
        let Some(socket_path) = args.get(pos + 1) else {
            eprintln!("--listen requires a socket path"); return;
        };
        listen_forever(socket_path, lsp_sessions, pty_sessions);
        return;
    }

    let writer: Writer = Arc::new(Mutex::new(Box::new(stdout())));
    let reader = BufReader::new(stdin());
    dispatch(reader, writer, lsp_sessions, pty_sessions);
}

/// Local daemon mode: accept one client connection at a time on a Unix
/// socket, forever. `pty_sessions` outlives any single connection; when a
/// client disconnects (e.g. its process restarted) and a new one connects,
/// existing PTY sessions' push callbacks keep working unmodified because
/// they hold the *same* `Arc<Mutex<..>>` writer cell — swapping the boxed
/// value inside it on reconnect transparently redirects every session's
/// future output to the new connection, with no per-session rewiring.
#[cfg(unix)]
fn listen_forever(
    socket_path:  &str,
    lsp_sessions: Arc<Mutex<HashMap<String, lsp::LspProxy>>>,
    pty_sessions: Arc<Mutex<HashMap<u32, pty::PtySession>>>,
) {
    use std::os::unix::net::UnixListener;

    let _ = std::fs::remove_file(socket_path); // stale socket from a prior run
    let listener = match UnixListener::bind(socket_path) {
        Ok(l)  => l,
        Err(e) => { eprintln!("bind {socket_path}: {e}"); return; }
    };

    // Placeholder writer until the first client connects; PTY sessions
    // opened before any client exists would be unusual but harmless — the
    // push simply has nowhere to go until swapped below.
    let writer: Writer = Arc::new(Mutex::new(Box::new(std::io::sink())));

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let read_half = match stream.try_clone() {
            Ok(s)  => s,
            Err(_) => continue,
        };
        *writer.lock().unwrap() = Box::new(stream);
        let reader = BufReader::new(read_half);
        // Blocks until this client disconnects, then loops to accept the next.
        dispatch(reader, Arc::clone(&writer), Arc::clone(&lsp_sessions), Arc::clone(&pty_sessions));
    }
}

#[cfg(not(unix))]
fn listen_forever(
    _socket_path:  &str,
    _lsp_sessions: Arc<Mutex<HashMap<String, lsp::LspProxy>>>,
    _pty_sessions: Arc<Mutex<HashMap<u32, pty::PtySession>>>,
) {
    eprintln!("--listen is only supported on Unix");
}

fn send(writer: &Writer, msg: &Rpc) {
    if let Ok(mut w) = writer.lock() {
        let mut buf = Vec::new();
        if write_rpc(&mut buf, msg).is_ok() {
            let _ = w.write_all(&buf);
            let _ = w.flush(); // flush immediately — pipe stdout is fully buffered
        }
    }
}

fn dispatch(
    mut reader:   impl std::io::BufRead,
    writer:       Writer,
    lsp_sessions: Arc<Mutex<HashMap<String, lsp::LspProxy>>>,
    pty_sessions: Arc<Mutex<HashMap<u32, pty::PtySession>>>,
) {
    loop {
        let Some(msg) = read_rpc(&mut reader) else { break };
        if !msg.is_request() { continue; }

        let id     = msg.id.unwrap();
        let method = msg.method.clone().unwrap_or_default();
        let params = msg.params.clone().unwrap_or(Value::Null);

        let w2   = Arc::clone(&writer);
        let lsp2 = Arc::clone(&lsp_sessions);
        let pty2 = Arc::clone(&pty_sessions);
        let pw   = Arc::clone(&writer);

        // Spawn handler thread so push-callbacks (pty/data, lsp/data) don't
        // block reading new requests.  The handler sends its response via the
        // shared mutex-locked writer.
        std::thread::spawn(move || {
            let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_request(id, &method, params, lsp2, pty2, pw)
            })).unwrap_or_else(|e| {
                let msg = e.downcast_ref::<String>().map(|s| s.as_str())
                    .or_else(|| e.downcast_ref::<&str>().copied())
                    .unwrap_or("panic in handler");
                Rpc::err(id, -32000, &format!("server panic: {msg}"))
            });
            send(&w2, &resp);
        });
    }
}

fn handle_request(
    id:     i64,
    method: &str,
    params: Value,
    lsp_sessions: Arc<Mutex<HashMap<String, lsp::LspProxy>>>,
    pty_sessions: Arc<Mutex<HashMap<u32, pty::PtySession>>>,
    push_writer:  Writer,
) -> Rpc {
    match method {
        "fs/list" => {
            let Ok(p) = serde_json::from_value::<FsListParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            match fs::list(&p.path) {
                Ok(e)  => Rpc::ok(id, json!({ "entries": e })),
                Err(e) => Rpc::err(id, -32000, &e),
            }
        }
        "fs/read" => {
            let Ok(p) = serde_json::from_value::<FsReadParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            match fs::read(&p.path) {
                Ok(t)  => Rpc::ok(id, json!({ "text": t })),
                Err(e) => Rpc::err(id, -32000, &e),
            }
        }
        "fs/write" => {
            let Ok(p) = serde_json::from_value::<FsWriteParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            match fs::write(&p.path, &p.text) {
                Ok(())  => Rpc::ok(id, json!({})),
                Err(e)  => Rpc::err(id, -32000, &e),
            }
        }
        "lsp/start" => {
            let Ok(p) = serde_json::from_value::<LspStartParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            let lang = p.lang.clone();
            let pw   = Arc::clone(&push_writer);
            match lsp::LspProxy::start(&p.lang, &p.root, move |data| {
                send(&pw, &Rpc::notify("lsp/data", json!(LspDataPush { lang: lang.clone(), data })));
            }) {
                Ok(proxy) => { lsp_sessions.lock().unwrap().insert(p.lang, proxy); Rpc::ok(id, json!({})) }
                Err(e)    => Rpc::err(id, -32000, &e),
            }
        }
        "lsp/send" => {
            let Ok(p) = serde_json::from_value::<LspSendParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            match lsp_sessions.lock().unwrap().get_mut(&p.lang) {
                Some(proxy) => match proxy.send(&p.data) {
                    Ok(())  => Rpc::ok(id, json!({})),
                    Err(e)  => Rpc::err(id, -32000, &e),
                },
                None => Rpc::err(id, -32000, "no lsp session"),
            }
        }
        "pty/open" => {
            let Ok(p) = serde_json::from_value::<PtyOpenParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            let pty_id = p.id;
            let pw     = Arc::clone(&push_writer);
            match pty::PtySession::open(p.cols, p.rows, &p.cwd, move |data| {
                send(&pw, &Rpc::notify("pty/data", json!(PtyDataPush { id: pty_id, data })));
            }) {
                Ok(s)  => { pty_sessions.lock().unwrap().insert(p.id, s); Rpc::ok(id, json!({})) }
                Err(e) => Rpc::err(id, -32000, &e),
            }
        }
        "pty/write" => {
            let Ok(p) = serde_json::from_value::<PtyWriteParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            match pty_sessions.lock().unwrap().get_mut(&p.id) {
                Some(s) => match s.write(&p.data) {
                    Ok(())  => Rpc::ok(id, json!({})),
                    Err(e)  => Rpc::err(id, -32000, &e),
                },
                None => Rpc::err(id, -32000, "no pty session"),
            }
        }
        "pty/resize" => {
            let Ok(p) = serde_json::from_value::<PtyResizeParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            match pty_sessions.lock().unwrap().get_mut(&p.id) {
                Some(s) => match s.resize(p.cols, p.rows) {
                    Ok(())  => Rpc::ok(id, json!({})),
                    Err(e)  => Rpc::err(id, -32000, &e),
                },
                None => Rpc::err(id, -32000, "no pty session"),
            }
        }
        "pty/close" => {
            let Ok(p) = serde_json::from_value::<PtyCloseParams>(params)
                else { return Rpc::err(id, -32602, "bad params") };
            pty_sessions.lock().unwrap().remove(&p.id);
            Rpc::ok(id, json!({}))
        }
        "pty/list" => {
            let infos: Vec<PtyInfo> = pty_sessions.lock().unwrap().iter()
                .map(|(id, s)| PtyInfo { id: *id, cwd: s.cwd.clone(), cols: s.cols, rows: s.rows })
                .collect();
            Rpc::ok(id, json!({ "sessions": infos }))
        }
        _ => Rpc::err(id, -32601, &format!("unknown method: {method}")),
    }
}
