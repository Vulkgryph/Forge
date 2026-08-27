// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)]
//! Lending a model endpoint to an agent on another machine, without lending the
//! credentials.
//!
//! An agent running on a remote host needs to reach a model API. Copying a key
//! there puts it on a disk that is not the user's; handing it over per session
//! keeps it off the disk but still puts it in that machine's memory, and does
//! not work at all for ChatGPT Codex or Anthropic OAuth, whose tokens are read
//! from a file beside the config on whichever machine the agent runs on.
//!
//! So the request comes here instead. The remote agent is pointed at a port on
//! its own loopback, which SSH forwards back to this machine; this module adds
//! the credential and forwards to the real endpoint. The agent never holds a
//! provider credential — not on disk, not in memory.
//!
//! It does hold one secret, and has to: the port is on that machine's loopback,
//! which every process and every other local user on it can reach, so a tunnel
//! that served whoever connected would let anything over there spend the user's
//! tokens without a credential of its own. Each session issues a random token
//! (`session_token`), hands it to the agent as the api_key of every endpoint it
//! lends, and serves nothing that does not present it (`authorized`). The token
//! authorises this tunnel and nothing else, it is marked ephemeral so the agent
//! does not write it to that machine's config, and it dies with the session.
//!
//! This half is the rewriting: what arrives, what is sent on, and what must
//! never be passed through. It performs no I/O so it can be tested against the
//! exact bytes a client sends.

/// How an endpoint expects to be authenticated.
///
/// Mirrors `forge-agent`'s own `EndpointType` handling: OpenAI-compatible and
/// ChatGPT Codex use a bearer token, Anthropic uses `x-api-key` and requires a
/// version header. Kept as its own type rather than shared, because agreeing
/// with that crate matters more than linking against it — this client
/// deliberately does not depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStyle {
    Bearer,
    AnthropicKey,
}

impl AuthStyle {
    /// From the `endpoint_type` string as it appears in config.toml.
    pub fn from_endpoint_type(kind: &str) -> Self {
        match kind {
            "anthropic" => AuthStyle::AnthropicKey,
            _ => AuthStyle::Bearer,
        }
    }
}

/// The Anthropic API version the agent sends. Must match, or Anthropic rejects
/// the request — and the agent cannot send it itself, since it is only added
/// alongside the key.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The head of an HTTP/1.1 request: everything before the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub path: String,
    /// In arrival order, names as sent.
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    /// Case-insensitively, as HTTP header names are.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Body length from `Content-Length`, or zero.
    pub fn content_length(&self) -> usize {
        self.header("content-length")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }
}

/// Parse the head of a request.
///
/// Deliberately strict about the request line and forgiving about everything
/// else: this only ever reads what forge-agent's own HTTP client sends, and a
/// malformed request is a bug rather than an attack surface — but it is still
/// refused rather than guessed at.
pub fn parse_head(text: &str) -> Result<RequestHead, String> {
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or("empty request")?;
    let mut parts = request_line.split(' ');
    let method = parts.next().filter(|m| !m.is_empty()).ok_or("no method")?;
    let path = parts.next().filter(|p| !p.is_empty()).ok_or("no path")?;
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/") {
        return Err(format!("not an HTTP request line: {request_line:?}"));
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        // A header without a colon is malformed; skipping it would silently
        // change the request, so the whole thing is refused.
        let (name, value) = line.split_once(':').ok_or_else(|| format!("bad header: {line:?}"))?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    Ok(RequestHead {
        method: method.to_string(),
        path: path.to_string(),
        headers,
    })
}

/// A fresh secret for one session's tunnel.
///
/// 32 bytes from the system's random device, hex-encoded. Not a provider
/// credential and not derived from one: it authorises use of this tunnel and
/// nothing else, it is never written to the remote's disk (the lent endpoint is
/// marked ephemeral), and it dies when the session does.
///
/// `/dev/urandom` rather than a crate, per the project's no-dependencies rule.
/// A failure to read it is fatal on purpose — the alternative is a predictable
/// token, and an unauthenticated tunnel is what this exists to prevent.
pub fn session_token() -> Result<String, String> {
    use std::io::Read;
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("could not read /dev/urandom for a session token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Whether a request may be served.
///
/// The tunnel listens on the remote's loopback, which sounds private and is
/// not: every process and every other user on that machine can reach loopback.
/// Without this check, anything over there could spend the user's tokens
/// through this proxy while holding no credential of its own — the credential
/// is added on this side.
///
/// The token arrives wherever the agent's endpoint style puts its key, so both
/// are accepted. Compared in constant time, which costs nothing here and
/// removes the question.
pub fn authorized(head: &RequestHead, token: &str) -> bool {
    if token.is_empty() {
        // No token means the caller built Routes without one. Refusing is the
        // only safe reading: an empty expected secret must not match an empty
        // presented one.
        return false;
    }
    head.headers.iter().any(|(name, value)| {
        let presented = if name.eq_ignore_ascii_case("authorization") {
            value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer ")).unwrap_or(value)
        } else if name.eq_ignore_ascii_case("x-api-key") {
            value.as_str()
        } else {
            return false;
        };
        constant_time_eq(presented.trim().as_bytes(), token.as_bytes())
    })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Headers that must not be forwarded upstream.
///
/// `host` is rewritten for the real endpoint. Anything carrying credentials is
/// dropped and replaced: the agent has none, so whatever it sent is either
/// empty or stale, and forwarding it could only override the real one. The
/// hop-by-hop headers belong to the connection this proxy terminates, not to
/// the one it opens.
fn is_dropped(name: &str) -> bool {
    const DROP: &[&str] = &[
        "host",
        "authorization",
        "x-api-key",
        "anthropic-version",
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        // Set by whatever performs the upstream request; a stale one would
        // describe a body length that is no longer accurate.
        "content-length",
    ];
    DROP.iter().any(|d| d.eq_ignore_ascii_case(name))
}

/// The headers to send upstream: what the agent sent, minus what must not
/// travel, plus the credential it does not have.
///
/// The credential is added last and unconditionally, so a header the agent
/// happened to send under the same name cannot survive to compete with it.
pub fn upstream_headers(
    head: &RequestHead,
    style: AuthStyle,
    credential: &str,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = head
        .headers
        .iter()
        .filter(|(k, _)| !is_dropped(k))
        .cloned()
        .collect();

    match style {
        AuthStyle::Bearer => {
            out.push(("Authorization".into(), format!("Bearer {credential}")));
        }
        AuthStyle::AnthropicKey => {
            out.push(("x-api-key".into(), credential.to_string()));
            out.push(("anthropic-version".into(), ANTHROPIC_VERSION.to_string()));
        }
    }
    out
}

/// The upstream URL for a request that arrived at the proxy.
///
/// The agent is configured with a base_url pointing at its own loopback, so the
/// path it sends is the path the real endpoint expects — `/v1/chat/completions`
/// and so on. Joining is therefore a matter of not doubling the slash.
pub fn upstream_url(base_url: &str, path: &str) -> String {
    format!("{}/{}", base_url.trim_end_matches('/'), path.trim_start_matches('/'))
}

/// What the proxy needs to know to serve one endpoint.
#[derive(Debug, Clone)]
pub struct Upstream {
    /// The real endpoint, e.g. `https://api.x.ai/v1`.
    pub base_url: String,
    pub style: AuthStyle,
    /// The key or access token. Held here, on this machine, and sent to the
    /// endpoint — never to the agent.
    pub credential: String,
    /// Headers only this machine can supply. ChatGPT Codex identifies the
    /// account with `chatgpt-account-id`, which the agent reads from the same
    /// token file it does not have — so the proxy adds it, or the request is
    /// authenticated as nobody in particular.
    pub extra_headers: Vec<(String, String)>,
}

/// Every endpoint this machine is lending, keyed by the path prefix the remote
/// reaches it through.
///
/// One tunnel rather than one per endpoint. The remote is given a distinct
/// prefix per endpoint — `/e0`, `/e1` — so switching models over there is a
/// different path on the same forwarded port, and the model picker can offer
/// everything this machine has instead of the single endpoint it was started
/// with.
#[derive(Debug, Clone, Default)]
pub struct Routes {
    endpoints: Vec<(String, Upstream)>,
    /// The secret a request must present to be served at all. See
    /// `session_token` and `authorized`.
    token: String,
}

impl Routes {
    pub fn new(token: String) -> Self {
        Self { endpoints: Vec::new(), token }
    }

    /// Add an endpoint and return the prefix the remote should use for it.
    pub fn add(&mut self, upstream: Upstream) -> String {
        let prefix = format!("/e{}", self.endpoints.len());
        self.endpoints.push((prefix.clone(), upstream));
        prefix
    }

    pub fn is_empty(&self) -> bool {
        self.endpoints.is_empty()
    }

    /// The token the remote agent is handed as its `api_key`, so it presents it
    /// on every request without needing to know it is doing so.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The endpoint a request is for, and the path with its prefix removed.
    ///
    /// An unrecognised prefix is not served. Guessing — falling back to the
    /// first endpoint, say — would send a request to the wrong provider with
    /// the wrong credential, which is worse than a refusal.
    pub fn route(&self, path: &str) -> Option<(&Upstream, String)> {
        self.endpoints.iter().find_map(|(prefix, up)| {
            let rest = path.strip_prefix(prefix.as_str())?;
            // `/e1` must not match a request for `/e10`.
            if !rest.is_empty() && !rest.starts_with('/') {
                return None;
            }
            let rest = if rest.is_empty() { "/" } else { rest };
            Some((up, rest.to_string()))
        })
    }
}

/// Read one request from `client`, forward it upstream with the credential
/// attached, and stream the answer back.
///
/// Streamed rather than buffered: a chat completion is server-sent events
/// arriving over the length of the model's reply, and collecting them first
/// would turn a response the user watches appear into one that arrives all at
/// once, minutes later.
///
/// One request per call. HTTP keep-alive would save a round trip on a link
/// where the round trip is already the smaller cost, and reusing a connection
/// means tracking its framing; the agent's client opens what it needs.
pub fn serve_one<S: std::io::Read + std::io::Write>(
    client: &mut S,
    routes: &Routes,
) -> Result<(), String> {
    use std::io::Read;

    // Read to the end of the head, and no further: whatever follows is body,
    // and the head says how much of it there is.
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        let mut byte = [0u8; 1];
        match client.read(&mut byte) {
            Ok(0) => return Err("client closed before sending a request".into()),
            Ok(_) => buf.push(byte[0]),
            Err(e) => return Err(format!("reading request: {e}")),
        }
        if buf.ends_with(b"\r\n\r\n") {
            break buf.len();
        }
        // A head this large is not something forge-agent sends.
        if buf.len() > 64 * 1024 {
            return Err("request head too large".into());
        }
    };

    let head = parse_head(&String::from_utf8_lossy(&buf[..head_end]))?;
    let mut body = vec![0u8; head.content_length()];
    if !body.is_empty() {
        client.read_exact(&mut body).map_err(|e| format!("reading body: {e}"))?;
    }

    let (upstream, path) = routes
        .route(&head.path)
        .ok_or_else(|| format!("no endpoint is lent at {}", head.path))?;
    // Checked after the request is read so the client gets an answer rather
    // than a dropped connection, and before anything is forwarded so an
    // unauthorised request never reaches a provider or a credential.
    if !authorized(&head, routes.token()) {
        let body = "{\"error\":{\"message\":\"this tunnel requires the session token Forge issued\"}}";
        let out = format!(
            "HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\n\
             content-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        client.write_all(out.as_bytes()).map_err(|e| format!("writing 401: {e}"))?;
        return Err("refused a request with no valid session token".into());
    }
    let url = upstream_url(&upstream.base_url, &path);
    let mut req = ureq::request(&head.method, &url);
    for (name, value) in upstream_headers(&head, upstream.style, &upstream.credential) {
        req = req.set(&name, &value);
    }
    for (name, value) in &upstream.extra_headers {
        req = req.set(name, value);
    }

    // An error response is a response: an upstream 401 or 429 is something the
    // agent knows how to report and retry, and turning it into a proxy failure
    // would hide what actually happened.
    let response = match req.send_bytes(&body) {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("upstream request failed: {e}")),
    };

    let status = response.status();
    let status_text = response.status_text().to_string();
    let names = response.headers_names();
    let mut out = format!("HTTP/1.1 {status} {status_text}\r\n");
    for name in &names {
        // Framing is this connection's business, not the upstream's: the body
        // is relayed as it arrives and closed at the end.
        if name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("content-length")
        {
            continue;
        }
        if let Some(value) = response.header(name) {
            out.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    out.push_str("connection: close\r\n\r\n");
    client.write_all(out.as_bytes()).map_err(|e| format!("writing response head: {e}"))?;

    let mut reader = response.into_reader();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                client.write_all(&chunk[..n]).map_err(|e| format!("writing body: {e}"))?;
                // Flushed per chunk: an event held in a buffer is an event the
                // user does not see yet, which is the whole point of streaming.
                client.flush().map_err(|e| format!("flushing: {e}"))?;
            }
            Err(e) => return Err(format!("reading upstream body: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;


    const REQ: &str = "POST /v1/chat/completions HTTP/1.1\r\n\
                       host: 127.0.0.1:18791\r\n\
                       content-type: application/json\r\n\
                       content-length: 42\r\n\
                       accept: text/event-stream\r\n\
                       \r\n";

    fn head() -> RequestHead {
        parse_head(REQ).unwrap()
    }

    #[test]
    fn a_request_head_is_parsed_into_its_parts() {
        let h = head();
        assert_eq!(h.method, "POST");
        assert_eq!(h.path, "/v1/chat/completions");
        assert_eq!(h.header("Content-Type"), Some("application/json"));
        assert_eq!(h.content_length(), 42);
    }

    #[test]
    fn header_lookup_ignores_case() {
        // The agent's client sends lowercase; upstreams and tests do not.
        let h = head();
        assert_eq!(h.header("HOST"), Some("127.0.0.1:18791"));
        assert_eq!(h.header("host"), h.header("Host"));
    }

    #[test]
    fn a_bearer_credential_is_added_and_the_loopback_host_dropped() {
        // The host header names the tunnel, which upstream would reject.
        let out = upstream_headers(&head(), AuthStyle::Bearer, "sk-secret");
        assert!(out.iter().any(|(k, v)| k == "Authorization" && v == "Bearer sk-secret"));
        assert!(!out.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")));
        // Everything the agent legitimately sent survives.
        assert!(out.iter().any(|(k, v)| k == "accept" && v == "text/event-stream"));
    }

    #[test]
    fn anthropic_gets_its_own_header_and_the_version_it_requires() {
        let out = upstream_headers(&head(), AuthStyle::AnthropicKey, "sk-ant");
        assert!(out.iter().any(|(k, v)| k == "x-api-key" && v == "sk-ant"));
        assert!(out.iter().any(|(k, v)| k == "anthropic-version" && v == ANTHROPIC_VERSION));
        assert!(
            !out.iter().any(|(k, _)| k == "Authorization"),
            "Anthropic does not take a bearer token",
        );
    }

    #[test]
    fn a_credential_the_agent_sent_cannot_survive() {
        // It has none, so anything under these names is stale or an attempt to
        // override the real one. Either way it does not travel.
        let req = "POST /v1/x HTTP/1.1\r\n\
                   authorization: Bearer stale-token\r\n\
                   x-api-key: someone-elses\r\n\
                   \r\n";
        let out = upstream_headers(&parse_head(req).unwrap(), AuthStyle::Bearer, "real");
        let auths: Vec<&String> = out
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
            .map(|(_, v)| v)
            .collect();
        assert_eq!(auths, vec!["Bearer real"], "exactly one, and ours");
        assert!(!out.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-api-key")));
    }

    #[test]
    fn hop_by_hop_headers_do_not_cross_to_the_upstream_connection() {
        // They describe the connection this proxy terminates. Forwarding
        // transfer-encoding in particular would describe a framing the
        // upstream request does not use.
        let req = "POST /v1/x HTTP/1.1\r\n\
                   connection: keep-alive\r\n\
                   transfer-encoding: chunked\r\n\
                   content-length: 9\r\n\
                   \r\n";
        let out = upstream_headers(&parse_head(req).unwrap(), AuthStyle::Bearer, "k");
        for banned in ["connection", "transfer-encoding", "content-length"] {
            assert!(
                !out.iter().any(|(k, _)| k.eq_ignore_ascii_case(banned)),
                "{banned} was forwarded",
            );
        }
    }

    fn two_endpoints() -> Routes {
        let mut r = Routes::new("t".into());
        for (n, url) in [("a", "https://api.one.test/v1"), ("b", "https://api.two.test/v1")] {
            r.add(Upstream {
                base_url: url.into(),
                style: AuthStyle::Bearer,
                credential: format!("key-{n}"),
                extra_headers: Vec::new(),
            });
        }
        r
    }

    #[test]
    fn each_endpoint_is_reached_at_its_own_path() {
        // One tunnel, several endpoints — so the model picker on the remote can
        // offer everything this machine has, not just the one it started with.
        let r = two_endpoints();
        let (up, rest) = r.route("/e0/chat/completions").expect("first");
        assert_eq!(up.credential, "key-a");
        assert_eq!(rest, "/chat/completions", "the prefix is stripped before forwarding");

        let (up, _) = r.route("/e1/chat/completions").expect("second");
        assert_eq!(up.credential, "key-b", "and they do not get each other's key");
    }

    #[test]
    fn an_unknown_prefix_is_refused_rather_than_guessed() {
        // Falling back to the first endpoint would send a request to the wrong
        // provider with the wrong credential.
        assert!(two_endpoints().route("/e7/chat/completions").is_none());
        assert!(two_endpoints().route("/chat/completions").is_none());
    }

    #[test]
    fn a_prefix_is_not_a_prefix_of_a_longer_one() {
        // /e1 must not answer for /e10, which it would with a plain
        // starts_with.
        let mut r = Routes::new("t".into());
        for i in 0..11 {
            r.add(Upstream {
                base_url: format!("https://{i}.test"),
                style: AuthStyle::Bearer,
                credential: format!("k{i}"),
                extra_headers: Vec::new(),
            });
        }
        let (up, _) = r.route("/e10/x").expect("e10");
        assert_eq!(up.credential, "k10");
    }

    #[test]
    fn an_endpoint_reached_at_its_bare_prefix_still_has_a_path() {
        let (_, rest) = two_endpoints().route("/e0").expect("bare");
        assert_eq!(rest, "/");
    }

    #[test]
    fn the_upstream_url_joins_without_doubling_the_slash() {
        assert_eq!(
            upstream_url("https://api.x.ai/v1", "/chat/completions"),
            "https://api.x.ai/v1/chat/completions",
        );
        assert_eq!(
            upstream_url("https://api.x.ai/v1/", "/chat/completions"),
            "https://api.x.ai/v1/chat/completions",
        );
    }

    #[test]
    fn something_that_is_not_a_request_is_refused() {
        // Better than forwarding a guess at what was meant.
        assert!(parse_head("hello\r\n\r\n").is_err());
        assert!(parse_head("POST /x\r\n\r\n").is_err());
        assert!(parse_head("POST /x HTTP/1.1\r\nno-colon-here\r\n\r\n").is_err());
    }

    #[test]
    fn the_auth_style_follows_the_configured_endpoint_type() {
        assert_eq!(AuthStyle::from_endpoint_type("anthropic"), AuthStyle::AnthropicKey);
        assert_eq!(AuthStyle::from_endpoint_type("open_ai"), AuthStyle::Bearer);
        // ChatGPT Codex is a bearer token too — an OAuth access token rather
        // than an API key, but the header is the same.
        assert_eq!(AuthStyle::from_endpoint_type("chatgpt_codex"), AuthStyle::Bearer);
    }
}

#[cfg(test)]
mod io_tests {
    use super::*;

    /// A stand-in for the per-session secret. Long enough that the constant-time
    /// comparison is exercised over a realistic length.
    const TEST_TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    use std::io::{Read, Write};

    /// A stand-in for the forwarded connection: the agent's bytes go in, the
    /// proxy's answer comes out.
    struct Pipe {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Pipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.input.read(buf)
        }
    }
    impl Write for Pipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    /// A local HTTP server standing in for the model API, so the round trip is
    /// real — head parsed, credential attached, response streamed back — while
    /// staying offline and costing nothing.
    /// Read a whole request, head and declared body, rather than whatever one
    /// `read` happened to return.
    ///
    /// A single read is a race, not a shortcut: a proxied request can arrive as
    /// head-then-body, and a server that answers after the first segment and
    /// returns — closing the socket — makes the *client's* remaining write fail.
    /// That was an intermittent failure in these tests (about one run in four),
    /// and it looked like a proxy bug rather than a test bug.
    fn read_request(sock: &mut std::net::TcpStream) -> String {
        let mut req: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = match sock.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            req.extend_from_slice(&buf[..n]);
            let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
            let head = String::from_utf8_lossy(&req[..end]).to_ascii_lowercase();
            let len: usize = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
            if req.len() >= end + 4 + len { break; }
        }
        String::from_utf8_lossy(&req).into_owned()
    }

    fn upstream_that_echoes_its_auth() -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let req = read_request(&mut sock);
            let auth = req
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .unwrap_or("(none)")
                .trim()
                .to_string();
            let body = format!("saw[{auth}]");
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    #[test]
    fn a_request_reaches_the_endpoint_carrying_a_credential_the_client_never_sent() {
        // The whole design in one test: the agent sends an unauthenticated
        // request to what it thinks is localhost, and the endpoint receives an
        // authenticated one.
        let (base_url, server) = upstream_that_echoes_its_auth();
        let mut pipe = Pipe {
            input: std::io::Cursor::new(
                format!("POST /e0/v1/chat/completions HTTP/1.1\r\nhost: 127.0.0.1:1\r\n\
                         authorization: Bearer {TEST_TOKEN}\r\ncontent-length: 2\r\n\r\n{{}}")
                    .into_bytes(),
            ),
            output: Vec::new(),
        };
        let mut routes = Routes::new(TEST_TOKEN.into());
        let prefix = routes.add(Upstream {
            base_url,
            style: AuthStyle::Bearer,
            credential: "sk-lent".into(),
            extra_headers: Vec::new(),
        });
        assert_eq!(prefix, "/e0");
        serve_one(&mut pipe, &routes).expect("proxied");
        server.join().unwrap();

        let out = String::from_utf8_lossy(&pipe.output);
        assert!(out.starts_with("HTTP/1.1 200 OK"), "got {out:?}");
        assert!(
            out.contains("saw[Authorization: Bearer sk-lent]"),
            "the endpoint should have seen our credential: {out:?}",
        );
    }

    #[test]
    fn an_upstream_error_is_relayed_rather_than_swallowed() {
        // A 401 or a 429 is something the agent knows how to report and retry;
        // turning it into a proxy failure would hide what actually happened.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let _ = read_request(&mut sock);
            let _ = sock.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 5\r\n\r\nslow!",
            );
        });
        let mut pipe = Pipe {
            input: std::io::Cursor::new(
                format!("POST /e0/v1/x HTTP/1.1\r\nauthorization: Bearer {TEST_TOKEN}\r\n\
                         content-length: 0\r\n\r\n").into_bytes(),
            ),
            output: Vec::new(),
        };
        let mut routes = Routes::new(TEST_TOKEN.into());
        routes.add(Upstream {
            base_url: format!("http://127.0.0.1:{port}"),
            style: AuthStyle::Bearer,
            credential: "k".into(),
            extra_headers: Vec::new(),
        });
        serve_one(&mut pipe, &routes).expect("a 429 is a response, not a failure");
        server.join().unwrap();
        let out = String::from_utf8_lossy(&pipe.output);
        assert!(out.starts_with("HTTP/1.1 429"), "got {out:?}");
        assert!(out.ends_with("slow!"), "the body should be relayed: {out:?}");
    }

    /// The tunnel listens on the remote machine's loopback, which sounds
    /// private and is not: every process and every other local user on that
    /// host can reach loopback, and the credential is added on this side. So an
    /// unauthenticated request used to be enough to spend the user's tokens
    /// through their own proxy. It is now refused before it reaches a provider.
    ///
    /// No upstream is started by this test on purpose — if the refusal ever
    /// stops working, there is nothing listening to absorb the request, and the
    /// test fails rather than quietly passing against a live endpoint.
    #[test]
    fn a_request_without_the_session_token_is_refused() {
        let mut pipe = Pipe {
            input: std::io::Cursor::new(
                b"POST /e0/v1/chat/completions HTTP/1.1\r\ncontent-length: 0\r\n\r\n".to_vec(),
            ),
            output: Vec::new(),
        };
        let mut routes = Routes::new(TEST_TOKEN.into());
        routes.add(Upstream {
            base_url: "http://127.0.0.1:1".into(),
            style: AuthStyle::Bearer,
            credential: "sk-must-not-be-used".into(),
            extra_headers: Vec::new(),
        });
        let err = serve_one(&mut pipe, &routes).expect_err("an unauthenticated request is refused");
        assert!(err.contains("session token"), "unexpected error: {err}");

        let out = String::from_utf8_lossy(&pipe.output);
        assert!(out.starts_with("HTTP/1.1 401 Unauthorized"), "got {out:?}");
        assert!(!out.contains("sk-must-not-be-used"), "the credential leaked into the refusal");
    }

    /// A wrong token is not a missing one, and both are refused. The empty
    /// expected token is the case that would turn the check off wholesale.
    #[test]
    fn only_the_issued_token_authorizes() {
        let head = |name: &str, value: &str| RequestHead {
            method: "POST".into(),
            path: "/e0/v1/x".into(),
            headers: vec![(name.to_string(), value.to_string())],
        };
        assert!(authorized(&head("authorization", &format!("Bearer {TEST_TOKEN}")), TEST_TOKEN));
        assert!(authorized(&head("Authorization", &format!("bearer {TEST_TOKEN}")), TEST_TOKEN));
        assert!(authorized(&head("x-api-key", TEST_TOKEN), TEST_TOKEN));

        assert!(!authorized(&head("authorization", "Bearer wrong"), TEST_TOKEN));
        assert!(!authorized(&head("x-api-key", ""), TEST_TOKEN));
        assert!(!authorized(&head("content-type", TEST_TOKEN), TEST_TOKEN), "any header will do?");
        assert!(!authorized(&head("authorization", &format!("Bearer {TEST_TOKEN}")), ""),
            "an empty expected token must never match");
        // A prefix of the real token is not the real token.
        let short = &TEST_TOKEN[..TEST_TOKEN.len() - 1];
        assert!(!authorized(&head("x-api-key", short), TEST_TOKEN));
    }

    /// Two sessions to the same host must not share a secret, and a token has
    /// to be unguessable to be worth having.
    #[test]
    fn each_session_gets_its_own_unguessable_token() {
        let a = session_token().expect("/dev/urandom");
        let b = session_token().expect("/dev/urandom");
        assert_ne!(a, b, "two sessions were issued the same token");
        assert_eq!(a.len(), 64, "expected 32 bytes hex-encoded");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
