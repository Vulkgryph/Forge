//! Putting text on the system clipboard.
//!
//! A terminal program cannot see the user's copy key — Cmd-C and Ctrl-Shift-C
//! are handled by the terminal itself and never reach us, and what they copy
//! is whatever is highlighted. So copying a message is our own command, and
//! this is how the text gets out.
//!
//! Two routes, tried in that order:
//!
//! 1. The platform's clipboard helper (`pbcopy` and friends). Works no matter
//!    which terminal is in front, which matters because Apple's Terminal — the
//!    one most people have — does not implement route 2.
//! 2. OSC 52, the escape sequence that asks the terminal to do it. This is the
//!    only route that reaches the right machine's clipboard when forge is
//!    running over SSH, since the helper there would set the clipboard of a
//!    computer nobody is sitting at.

use std::io::Write;
use std::process::{Command, Stdio};

/// Clipboard helpers to try, in order. Each takes the text on stdin.
#[cfg(target_os = "macos")]
const HELPERS: &[(&str, &[&str])] = &[("pbcopy", &[])];

#[cfg(all(unix, not(target_os = "macos")))]
const HELPERS: &[(&str, &[&str])] = &[
    // Wayland first: on a Wayland session xclip may still be installed and
    // will appear to work while setting a clipboard nothing is reading.
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
];

#[cfg(windows)]
const HELPERS: &[(&str, &[&str])] = &[("clip", &[])];

/// Copy `text`, returning how it got there — worth saying out loud, because
/// "copied" via OSC 52 means "asked the terminal to", and some terminals
/// quietly decline.
pub fn copy(text: &str) -> Result<String, String> {
    let mut why = Vec::new();
    for (prog, args) in HELPERS {
        match pipe_to(prog, args, text) {
            Ok(()) => return Ok((*prog).to_string()),
            Err(e) => why.push(format!("{prog}: {e}")),
        }
    }
    match write_to_terminal(&osc52(text)) {
        Ok(()) => Ok("the terminal".to_string()),
        Err(e) => {
            why.push(format!("OSC 52: {e}"));
            Err(why.join("; "))
        }
    }
}

/// Feed `text` to a helper's stdin and wait for it to accept it.
fn pipe_to(prog: &str, args: &[&str], text: &str) -> Result<(), String> {
    let mut child = Command::new(prog)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    // Taken, not borrowed: stdin has to be dropped before the wait, or a
    // helper that reads to EOF never gets one and we deadlock.
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let wrote = stdin.write_all(text.as_bytes()).map_err(|e| e.to_string());
    drop(stdin);
    let status = child.wait().map_err(|e| e.to_string())?;
    wrote?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("exited with {status}"))
    }
}

/// The escape sequence asking the terminal to set the system clipboard.
///
/// `c` is the clipboard selection; the payload is base64 because the sequence
/// is terminated by a control character and the text may contain anything.
pub fn osc52(text: &str) -> String {
    let seq = format!("\x1b]52;c;{}\x07", base64(text.as_bytes()));
    // tmux does not forward escape sequences it does not understand, so
    // inside tmux this has to be wrapped in its passthrough — which also
    // needs every ESC in the payload doubled. There are none here: base64
    // has no ESC in its alphabet, and the wrapper's own are added after.
    if std::env::var_os("TMUX").is_some() {
        format!("\x1bPtmux;{}\x1b\\", seq.replace('\x1b', "\x1b\x1b"))
    } else {
        seq
    }
}

/// Write straight to the terminal rather than through the renderer: this is
/// not part of a frame, and the renderer counts the rows it draws.  An OSC
/// sequence moves no cursor, so it is safe to slip between frames.
fn write_to_terminal(seq: &str) -> Result<(), String> {
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes()).map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

/// Standard base64 with padding (RFC 4648 §4).
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            // A 2-byte chunk encodes to 3 characters plus one pad, a 1-byte
            // chunk to 2 plus two.
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - i * 6) & 0x3f) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ignored by default: it touches the real clipboard, which belongs to
    /// whoever is running the tests. Run it deliberately —
    /// `cargo test -p forge-tui-rs -- --ignored copy_reaches` — after touching
    /// anything here; it restores what was on the clipboard when it started.
    #[test]
    #[ignore = "uses the real system clipboard"]
    #[cfg(target_os = "macos")]
    fn copy_reaches_the_real_clipboard() {
        let read = || -> String {
            String::from_utf8_lossy(
                &Command::new("pbpaste").output().expect("pbpaste").stdout,
            )
            .into_owned()
        };
        let before = read();
        let sample = "forge clipboard check\nsecond line\ttabbed";
        let via = copy(sample).expect("copy failed");
        assert_eq!(via, "pbcopy", "fell back instead of using the helper");
        assert_eq!(read(), sample);
        pipe_to("pbcopy", &[], &before).expect("restore the clipboard");
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        // RFC 4648 §10, which exists precisely so an encoder like this one can
        // be checked against something other than its author's expectations.
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want, "encoding {input:?}");
        }
    }

    #[test]
    fn base64_handles_bytes_that_are_not_text() {
        // The high bits are where a hand-rolled encoder goes wrong: a sign
        // extension or a missing mask shows up here and nowhere else.
        assert_eq!(base64(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(base64(&[0xfb, 0xff, 0xbf]), "+/+/");
    }

    #[test]
    fn osc52_carries_the_text_base64_encoded() {
        // Not under tmux in CI, but assert on the inner sequence either way so
        // this test says the same thing wherever it runs.
        let seq = osc52("hi");
        assert!(seq.contains("52;c;aGk="), "got {seq:?}");
    }

    #[test]
    fn a_newline_survives_encoding() {
        // The whole point is copying a multi-line message; a sequence broken
        // by a raw newline would be interpreted as far as the newline and the
        // rest printed to the screen.
        let seq = osc52("a\nb");
        assert!(!seq.contains('\n'), "raw newline in the escape sequence: {seq:?}");
        assert!(seq.contains("YQpi"), "got {seq:?}");
    }
}
