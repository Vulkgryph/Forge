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

/// Every client connected right now, by an id of the daemon's own making.
///
/// There used to be no need for this: the daemon served one client at a time,
/// because Forge IDE was one process. It is not any more — a window restarted on
/// its own moves into a process of its own — so several clients are now normal,
/// and each needs its own writer rather than a single cell whose contents the
/// newest connection overwrote.
type Clients = Arc<Mutex<HashMap<u64, Writer>>>;

/// Which client each PTY's output goes to.
///
/// Also new, and for the same reason. A session's push callback used to write
/// into the one shared writer, which meant "whoever connected most recently" —
/// correct only while that was also "the only client there is". Now a session
/// names its subscriber, set when it is opened and moved when a client
/// reattaches to it after its own process restarted.
type PtySubs = Arc<Mutex<HashMap<u32, Writer>>>;

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
    // Stdio mode (an SSH exec channel): exactly one client by construction, so
    // the registries hold one entry and the broadcast has nobody else to reach.
    dispatch(
        reader, writer, lsp_sessions, pty_sessions,
        Arc::new(Mutex::new(HashMap::new())),
        Arc::new(Mutex::new(HashMap::new())),
        1,
    );
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

    let clients: Clients = Arc::new(Mutex::new(HashMap::new()));
    let subs:    PtySubs = Arc::new(Mutex::new(HashMap::new()));
    let mut next_client: u64 = 1;

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let read_half = match stream.try_clone() {
            Ok(s)  => s,
            Err(_) => continue,
        };
        // One thread per connection, rather than serving them one at a time.
        // Serially was fine while Forge IDE was a single process; with a window
        // able to restart into a process of its own, a second client used to sit
        // unaccepted in the backlog until the first quit — long enough for its
        // handshake to time out, at which point it decided the daemon was wedged
        // and started another one, and the sessions recorded against this one
        // became unreachable.
        let writer: Writer = Arc::new(Mutex::new(Box::new(stream)));
        let client_id = next_client;
        next_client += 1;
        clients.lock().unwrap().insert(client_id, Arc::clone(&writer));

        let lsp2  = Arc::clone(&lsp_sessions);
        let pty2  = Arc::clone(&pty_sessions);
        let subs2 = Arc::clone(&subs);
        let cls2  = Arc::clone(&clients);
        std::thread::spawn(move || {
            dispatch(BufReader::new(read_half), writer, lsp2, pty2, subs2,
                     Arc::clone(&cls2), client_id);
            // Gone. Its sessions keep running — that is the point of the daemon
            // — and whichever client reattaches to them says so with
            // `pty/subscribe`.
            cls2.lock().unwrap().remove(&client_id);
        });
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

#[allow(clippy::too_many_arguments)]
fn dispatch(
    mut reader:   impl std::io::BufRead,
    writer:       Writer,
    lsp_sessions: Arc<Mutex<HashMap<String, lsp::LspProxy>>>,
    pty_sessions: Arc<Mutex<HashMap<u32, pty::PtySession>>>,
    subs:         PtySubs,
    clients:      Clients,
    client_id:    u64,
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
        let sub2 = Arc::clone(&subs);
        let cls2 = Arc::clone(&clients);

        // Spawn handler thread so push-callbacks (pty/data, lsp/data) don't
        // block reading new requests.  The handler sends its response via the
        // shared mutex-locked writer.
        std::thread::spawn(move || {
            let resp = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_request(id, &method, params, lsp2, pty2, pw, sub2, cls2, client_id)
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

#[allow(clippy::too_many_arguments)]
fn handle_request(
    id:     i64,
    method: &str,
    params: Value,
    lsp_sessions: Arc<Mutex<HashMap<String, lsp::LspProxy>>>,
    pty_sessions: Arc<Mutex<HashMap<u32, pty::PtySession>>>,
    push_writer:  Writer,
    subs:         PtySubs,
    clients:      Clients,
    client_id:    u64,
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
        "fs/mkdir" => {
            let Some(path) = params.get("path").and_then(|v| v.as_str()) else {
                return Rpc::err(id, -32602, "bad params");
            };
            match fs::mkdir(path) {
                Ok(())  => Rpc::ok(id, json!({})),
                Err(e)  => Rpc::err(id, -32000, &e),
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
            subs.lock().unwrap().insert(pty_id, Arc::clone(&push_writer));
            // Looked up per push rather than captured: the client this session
            // belongs to can change — its process restarts and another one
            // reattaches — and the output has to follow it, not the connection
            // that happened to open it.
            let subs2 = Arc::clone(&subs);
            match pty::PtySession::open(p.cols, p.rows, &p.cwd, move |data| {
                let target = subs2.lock().unwrap().get(&pty_id).cloned();
                if let Some(w) = target {
                    send(&w, &Rpc::notify("pty/data", json!(PtyDataPush { id: pty_id, data })));
                }
            }) {
                Ok(s)  => { pty_sessions.lock().unwrap().insert(p.id, s); Rpc::ok(id, json!({})) }
                Err(e) => Rpc::err(id, -32000, &e),
            }
        }
        // "This session's output comes to me now." Sent when a client reattaches
        // to a session that outlived its own process — the client used to just
        // register a local channel and hope, which worked only because there was
        // never a second client to push to by mistake.
        "pty/subscribe" => {
            let Some(pty_id) = params.get("id").and_then(|v| v.as_u64()) else {
                return Rpc::err(id, -32602, "bad params");
            };
            let pty_id = pty_id as u32;
            if !pty_sessions.lock().unwrap().contains_key(&pty_id) {
                return Rpc::err(id, -32000, "no such session");
            }
            subs.lock().unwrap().insert(pty_id, Arc::clone(&push_writer));
            Rpc::ok(id, json!({}))
        }
        // Tell every other client something. The only thing anyone says with it
        // is "restart": a new build is installed and the windows spread across
        // several processes should all come up on it. The daemon is the only
        // thing they all already talk to, which makes it the only place this can
        // live without inventing a second channel between them.
        "broadcast" => {
            let kind = params.get("kind").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if kind.is_empty() {
                return Rpc::err(id, -32602, "bad params");
            }
            let others: Vec<Writer> = clients.lock().unwrap().iter()
                .filter(|(cid, _)| **cid != client_id)
                .map(|(_, w)| Arc::clone(w))
                .collect();
            let reached = others.len();
            let note = Rpc::notify("broadcast", json!({ "kind": kind }));
            for w in others {
                send(&w, &note);
            }
            // The count is the useful part: the sender can say how many other
            // windows it actually reached rather than claiming it told everyone.
            Rpc::ok(id, json!({ "reached": reached }))
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

#[cfg(all(test, unix))]
mod daemon_tests {
    use super::*;
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};

    /// A socket path short enough for `SUN_LEN`, which the obvious temp path is
    /// not — a lesson from watching this fail to bind.
    fn socket_for(tag: &str) -> String {
        let p = format!("/tmp/fst-{}-{tag}.sock", std::process::id());
        let _ = std::fs::remove_file(&p);
        p
    }

    fn daemon(path: &str) {
        let owned = path.to_string();
        std::thread::spawn(move || {
            listen_forever(
                &owned,
                Arc::new(Mutex::new(HashMap::new())),
                Arc::new(Mutex::new(HashMap::new())),
            );
        });
        // Wait for the bind rather than sleeping a guessed amount.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if UnixStream::connect(path).is_ok() { return; }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("daemon never bound {path}");
    }

    struct Client {
        w: UnixStream,
        r: BufReader<UnixStream>,
        n: i64,
    }

    impl Client {
        fn connect(path: &str) -> Self {
            let w = UnixStream::connect(path).expect("connect");
            w.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let r = BufReader::new(w.try_clone().unwrap());
            Self { w, r, n: 0 }
        }

        fn send(&mut self, method: &str, params: Value) -> i64 {
            self.n += 1;
            let msg = Rpc::request(self.n, method, params);
            write_rpc(&mut self.w, &msg).expect("write");
            self.n
        }

        /// The next message, or `None` on timeout.
        fn next(&mut self) -> Option<Rpc> {
            read_rpc(&mut self.r)
        }

        /// The response to `id`, skipping notifications that arrive first.
        fn response(&mut self, id: i64) -> Rpc {
            for _ in 0..64 {
                if let Some(m) = self.next() {
                    if m.id == Some(id) { return m; }
                } else {
                    break;
                }
            }
            panic!("no response to request {id}");
        }

        fn call(&mut self, method: &str, params: Value) -> Rpc {
            let id = self.send(method, params);
            self.response(id)
        }

        /// Notifications sitting unread, drained until the socket goes quiet.
        fn drain_notifications(&mut self, wait: Duration) -> Vec<Rpc> {
            self.w.set_read_timeout(Some(wait)).unwrap();
            let mut out = Vec::new();
            while let Some(m) = self.next() {
                if m.id.is_none() { out.push(m); }
                if out.len() > 256 { break; }
            }
            self.w.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            out
        }
    }

    /// The daemon used to serve one client at a time — it accepted a connection
    /// and then blocked until that client went away. Fine while Forge IDE was one
    /// process; once a window can restart into a process of its own, the second
    /// client sat unaccepted in the backlog until its handshake timed out, at
    /// which point it decided the daemon was wedged and started another one.
    #[test]
    fn two_clients_are_served_at_once() {
        let path = socket_for("concurrent");
        daemon(&path);
        let mut a = Client::connect(&path);
        let mut b = Client::connect(&path);

        // B while A is still connected: the request that used to never be read.
        assert!(a.call("pty/list", json!({})).error.is_none(), "A was not served");
        assert!(b.call("pty/list", json!({})).error.is_none(), "B was not served");
        let _ = std::fs::remove_file(&path);
    }

    /// The message that makes "restart every window" possible across processes.
    /// It goes to the others and not back to the sender, and says how many it
    /// reached so the sender can report that rather than assuming.
    #[test]
    fn a_broadcast_reaches_the_others_and_not_the_sender() {
        let path = socket_for("broadcast");
        daemon(&path);
        let mut a = Client::connect(&path);
        let mut b = Client::connect(&path);
        let mut c = Client::connect(&path);
        // Make sure all three are registered before counting.
        for cl in [&mut a, &mut b, &mut c] { assert!(cl.call("pty/list", json!({})).error.is_none()); }

        let reply = a.call("broadcast", json!({ "kind": "restart" }));
        let reached = reply.result.as_ref()
            .and_then(|v| v.get("reached")).and_then(|v| v.as_u64());
        assert_eq!(reached, Some(2), "wrong number of clients reached: {reply:?}");

        for (name, cl) in [("B", &mut b), ("C", &mut c)] {
            let notes = cl.drain_notifications(Duration::from_millis(400));
            let heard = notes.iter().any(|m| {
                m.method.as_deref() == Some("broadcast")
                    && m.params.as_ref().and_then(|p| p.get("kind")).and_then(|k| k.as_str())
                        == Some("restart")
            });
            assert!(heard, "{name} did not hear the broadcast");
        }
        let back = a.drain_notifications(Duration::from_millis(200));
        assert!(
            !back.iter().any(|m| m.method.as_deref() == Some("broadcast")),
            "the sender heard its own broadcast",
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The routing fix itself, which is what terminals surviving a restart
    /// depends on: a session's output goes to the client that has it, and moves
    /// when another client says it has it now.
    ///
    /// Previously every push went to whichever connection was newest, so B
    /// received A's shell output the moment B connected — without asking, and
    /// while A was still using it.
    #[test]
    fn a_sessions_output_follows_the_client_that_subscribes() {
        let path = socket_for("routing");
        daemon(&path);
        let mut a = Client::connect(&path);
        let mut b = Client::connect(&path);
        const ID: u32 = 4242;

        assert!(
            a.call("pty/open", json!({ "id": ID, "cols": 80, "rows": 24, "cwd": "/tmp" }))
                .error.is_none(),
            "could not open a pty",
        );

        // A owns it: A sees output, B sees none — even though B connected after.
        let _ = a.drain_notifications(Duration::from_millis(600));
        assert!(a.call("pty/write", json!({ "id": ID, "data": b"echo one\n" })).error.is_none());
        let to_a = a.drain_notifications(Duration::from_millis(800));
        let to_b = b.drain_notifications(Duration::from_millis(200));
        assert!(to_a.iter().any(|m| m.method.as_deref() == Some("pty/data")),
                "the client that opened the session got no output");
        assert!(!to_b.iter().any(|m| m.method.as_deref() == Some("pty/data")),
                "a client that never asked for this session received its output");

        // B reattaches — a window that restarted into a new process.
        assert!(b.call("pty/subscribe", json!({ "id": ID })).error.is_none());
        assert!(b.call("pty/write", json!({ "id": ID, "data": b"echo two\n" })).error.is_none());
        let to_b = b.drain_notifications(Duration::from_millis(800));
        let to_a = a.drain_notifications(Duration::from_millis(200));
        assert!(to_b.iter().any(|m| m.method.as_deref() == Some("pty/data")),
                "output did not follow the subscribing client");
        assert!(!to_a.iter().any(|m| m.method.as_deref() == Some("pty/data")),
                "the old client still receives a session it handed over");

        let _ = b.call("pty/close", json!({ "id": ID }));
        let _ = std::fs::remove_file(&path);
    }

    /// Subscribing to a session that is not there is an error rather than a
    /// silent success — a client that believes it is attached to a shell that
    /// does not exist waits forever for output.
    #[test]
    fn subscribing_to_nothing_says_so() {
        let path = socket_for("subscribe-missing");
        daemon(&path);
        let mut a = Client::connect(&path);
        let reply = a.call("pty/subscribe", json!({ "id": 999_999 }));
        assert!(reply.error.is_some(), "claimed a subscription to a session that does not exist");
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(all(test, unix))]
mod mkdir_tests {
    use super::*;

    fn scratch(tag: &str) -> String {
        let p = format!("/tmp/fs-mkdir-{}-{tag}", std::process::id());
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    /// A folder someone asked for, including the parents it needs — "a/b/c" typed
    /// into the box is one request, not three.
    #[test]
    fn a_nested_folder_is_made_in_one_go() {
        let base = scratch("nested");
        std::fs::create_dir_all(&base).unwrap();
        let target = format!("{base}/one/two/three");
        assert!(fs::mkdir(&target).is_ok());
        assert!(std::path::Path::new(&target).is_dir());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Refused when something is already there, rather than reporting success
    /// for a folder it did not create: `create_dir_all` is happy to do nothing,
    /// which would make a typo — or overwriting someone's work — look like it
    /// worked.
    #[test]
    fn making_one_that_exists_is_an_error() {
        let base = scratch("exists");
        std::fs::create_dir_all(&base).unwrap();
        let err = fs::mkdir(&base).unwrap_err();
        assert!(err.contains("already exists"), "{err}");

        // And the same for a *file* in the way, which is the case that would
        // otherwise fail obscurely later.
        let file = format!("{base}/taken");
        std::fs::write(&file, "").unwrap();
        assert!(fs::mkdir(&file).unwrap_err().contains("already exists"));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Somewhere unwritable is an error the dialog can show, not a panic.
    #[test]
    fn an_unwritable_place_is_reported() {
        let err = fs::mkdir("/this-should-not-be-writable-forge-test").unwrap_err();
        assert!(!err.is_empty());
    }
}
