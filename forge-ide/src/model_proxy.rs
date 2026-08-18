// SPDX-License-Identifier: Apache-2.0
// Not yet called: the reverse SSH tunnel that hands connections to `serve_one`
// is the next piece. Kept and tested on its own because it is the part that
// decides what a credential does and does not travel with, and that is worth
// pinning before anything depends on it.
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
//! the credential and forwards to the real endpoint. The agent sends
//! unauthenticated requests to localhost and never holds anything secret — not
//! on disk, not in memory.
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
    upstream: &Upstream,
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

    let url = upstream_url(&upstream.base_url, &head.path);
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
    fn upstream_that_echoes_its_auth() -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
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
                b"POST /v1/chat/completions HTTP/1.1\r\nhost: 127.0.0.1:1\r\ncontent-length: 2\r\n\r\n{}"
                    .to_vec(),
            ),
            output: Vec::new(),
        };
        serve_one(
            &mut pipe,
            &Upstream {
                base_url,
                style: AuthStyle::Bearer,
                credential: "sk-lent".into(),
                extra_headers: Vec::new(),
            },
        )
        .expect("proxied");
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
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            let _ = sock.write_all(
                b"HTTP/1.1 429 Too Many Requests\r\ncontent-length: 5\r\n\r\nslow!",
            );
        });
        let mut pipe = Pipe {
            input: std::io::Cursor::new(b"POST /v1/x HTTP/1.1\r\ncontent-length: 0\r\n\r\n".to_vec()),
            output: Vec::new(),
        };
        serve_one(
            &mut pipe,
            &Upstream {
                base_url: format!("http://127.0.0.1:{port}"),
                style: AuthStyle::Bearer,
                credential: "k".into(),
                extra_headers: Vec::new(),
            },
        )
        .expect("a 429 is a response, not a failure");
        server.join().unwrap();
        let out = String::from_utf8_lossy(&pipe.output);
        assert!(out.starts_with("HTTP/1.1 429"), "got {out:?}");
        assert!(out.ends_with("slow!"), "the body should be relayed: {out:?}");
    }
}
