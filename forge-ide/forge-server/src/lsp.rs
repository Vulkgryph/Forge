//! Spawn a language server on the remote machine and proxy its JSON-RPC to
//! the client.  The server's stdout is forwarded as `lsp/data` push
//! notifications; the client's `lsp/send` requests are written to stdin.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

pub struct LspProxy {
    child: Child,
    stdin: ChildStdin,
}

impl LspProxy {
    /// Spawn a language server for `lang` rooted at `root`.
    /// `push` is called on a background thread with each raw LSP message body.
    pub fn start(
        lang:  &str,
        root:  &str,
        push:  impl Fn(String) + Send + 'static,
    ) -> Result<Self, String> {
        let cmd = lang_to_cmd(lang)
            .ok_or_else(|| format!("no language server for '{lang}'"))?;

        let mut child = Command::new(&cmd[0])
            .args(&cmd[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("{}: {e}", cmd[0]))?;

        let stdin  = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());

        // Reader thread: forward server stdout → push callback.
        std::thread::spawn(move || {
            let mut r = stdout;
            loop {
                // Read Content-Length header
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 { return; }
                    let t = line.trim_end();
                    if t.is_empty() { break; }
                    if let Some(rest) = t.strip_prefix("Content-Length:") {
                        len = rest.trim().parse().unwrap_or(0);
                    }
                }
                if len == 0 { continue; }
                let mut buf = vec![0u8; len];
                if r.read_exact(&mut buf).is_err() { return; }
                if let Ok(s) = String::from_utf8(buf) { push(s); }
            }
        });

        Ok(Self { child, stdin })
    }

    /// Forward a raw LSP message body (no Content-Length) to the language server.
    pub fn send(&mut self, data: &str) -> Result<(), String> {
        write!(self.stdin, "Content-Length: {}\r\n\r\n{}", data.len(), data)
            .map_err(|e| e.to_string())?;
        self.stdin.flush().map_err(|e| e.to_string())
    }
}

impl Drop for LspProxy {
    fn drop(&mut self) { let _ = self.child.kill(); }
}

fn lang_to_cmd(lang: &str) -> Option<Vec<String>> {
    match lang {
        "rust"             => Some(vec!["rust-analyzer".into()]),
        "typescript" | "javascript"
                           => Some(vec!["typescript-language-server".into(), "--stdio".into()]),
        "python"           => Some(vec!["pylsp".into()]),
        "c" | "cpp"        => Some(vec!["clangd".into()]),
        "go"               => Some(vec!["gopls".into()]),
        _                  => None,
    }
}
