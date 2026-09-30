// SPDX-License-Identifier: Apache-2.0
//! Reading a text file that is not UTF-8, and writing it back the way it came.
//!
//! `read_to_string` refuses anything that is not valid UTF-8, and the editor
//! turned that into "`{path}` is not a text file" — a thoughtful message for
//! the wrong thing. A latin-1 source file from 2003, a UTF-16 file off a
//! Windows box, or one stray byte in an otherwise fine file all hit it, and
//! none of them is a binary.
//!
//! ## Why this also has to encode
//!
//! Decoding alone would have been worse than refusing. `Buffer::save` writes
//! `text_for_disk()` through `fs::write`, which is UTF-8 — so opening a
//! UTF-16 file, changing one character and saving would have silently
//! rewritten the whole thing as UTF-8 and left the BOM as mojibake at the top.
//! A file the editor cannot write back unchanged is a file it should not
//! open. So the encoding is remembered on load and applied on save, and
//! `encode(decode(bytes)) == bytes` is the property the tests check.
//!
//! Line endings are here for the same reason. `str::lines()` strips `\r`, and
//! saving wrote `\n`, so every CRLF file the editor touched became LF —
//! quietly, and across every line rather than the one that was edited. That is
//! the same defect as the encoding one and it was already shipping.
//!
//! ## What is deliberately not supported
//!
//! No encoding detection beyond a BOM and a UTF-8 validity check. Guessing
//! between latin-1, Windows-1252, KOI8-R and Shift-JIS from byte frequencies
//! is what `encoding_rs` and `chardet` exist for, and getting it wrong means
//! showing someone plausible nonsense. Latin-1 is the fallback because it is
//! total and exactly reversible: every byte is a codepoint, every codepoint
//! under U+0100 is a byte, so a file opened that way is written back
//! byte-identical even if the *characters* shown were not the ones intended.
//! A wrong-but-reversible reading loses nothing; a wrong-and-lossy one does.

/// How a file's bytes map to text, remembered so saving can reverse it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    /// Valid UTF-8, no byte-order mark. The overwhelming common case.
    Utf8,
    /// Valid UTF-8 behind an EF BB BF mark. Kept because tools on Windows
    /// write it and some of them need it back.
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    /// Not valid UTF-8 and no mark. Read byte-for-codepoint; see the module
    /// doc on why this is the fallback rather than a guess.
    Latin1,
}

/// Which newline the file used, so saving does not convert it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl Encoding {
    /// For the status bar. `Utf8` returns `None` — the common case needs no
    /// label, and a label on every file would make the unusual ones invisible.
    pub fn label(self) -> Option<&'static str> {
        match self {
            Encoding::Utf8 => None,
            Encoding::Utf8Bom => Some("UTF-8 BOM"),
            Encoding::Utf16Le => Some("UTF-16 LE"),
            Encoding::Utf16Be => Some("UTF-16 BE"),
            Encoding::Latin1 => Some("Latin-1"),
        }
    }
}

impl LineEnding {
    pub fn label(self) -> Option<&'static str> {
        match self {
            LineEnding::Lf => None,
            LineEnding::Crlf => Some("CRLF"),
        }
    }
}

/// What `decode` worked out about a file, alongside its text.
#[derive(Debug)]
pub struct Decoded {
    pub text: String,
    pub encoding: Encoding,
    pub line_ending: LineEnding,
}

/// Turn a file's bytes into text, or say why they are not text at all.
///
/// The binary check is a NUL byte outside a UTF-16 file, which is the same
/// heuristic `grep` and `git` use. Without it, latin-1 would happily "decode"
/// an ELF binary into 40,000 lines of control characters, and the editor's
/// refusal for genuinely binary files — which is correct and worth keeping —
/// would never fire again.
pub fn decode(bytes: &[u8]) -> Result<Decoded, String> {
    // A mark settles it. UTF-16 is checked before UTF-8 because a UTF-16
    // file's NUL bytes would otherwise trip the binary check below.
    // A failed UTF-16 decode falls through rather than erroring: plenty of
    // binaries begin FF FE or FE FF by coincidence, and reporting "UTF-16 with
    // a trailing odd byte" for a `.bin` names the wrong thing entirely. If the
    // bytes really are UTF-16 this succeeds; if they are not, the checks below
    // reach the right answer.
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        if let Ok(text) = decode_utf16(rest, false) {
            return Ok(finish(text, Encoding::Utf16Le));
        }
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        if let Ok(text) = decode_utf16(rest, true) {
            return Ok(finish(text, Encoding::Utf16Be));
        }
    }
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        let text = String::from_utf8(rest.to_vec())
            .map_err(|_| "has a UTF-8 byte-order mark but is not valid UTF-8".to_string())?;
        return Ok(finish(text, Encoding::Utf8Bom));
    }

    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(finish(text.to_string(), Encoding::Utf8)),
        Err(_) => {
            if bytes.contains(&0) {
                return Err("is a binary file".to_string());
            }
            // Byte for codepoint. Exactly reversible, which is the point.
            let text: String = bytes.iter().map(|&b| b as char).collect();
            Ok(finish(text, Encoding::Latin1))
        }
    }
}

/// Text back to bytes, in the encoding it came from.
pub fn encode(text: &str, encoding: Encoding) -> Vec<u8> {
    match encoding {
        Encoding::Utf8 => text.as_bytes().to_vec(),
        Encoding::Utf8Bom => {
            let mut out = vec![0xEF, 0xBB, 0xBF];
            out.extend_from_slice(text.as_bytes());
            out
        }
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let big = encoding == Encoding::Utf16Be;
            let mut out = Vec::with_capacity(text.len() * 2 + 2);
            out.extend_from_slice(if big { &[0xFE, 0xFF] } else { &[0xFF, 0xFE] });
            let mut buf = [0u16; 2];
            for ch in text.chars() {
                for unit in ch.encode_utf16(&mut buf) {
                    out.extend_from_slice(&if big {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    });
                }
            }
            out
        }
        Encoding::Latin1 => text
            .chars()
            // Anything the user typed that latin-1 cannot hold becomes `?`.
            // Lossy, and only for characters that were not in the file when it
            // was opened — see `Buffer::save`, which refuses rather than
            // silently doing this.
            .map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' })
            .collect(),
    }
}

/// True when `text` contains a character the encoding cannot represent, so a
/// save would lose it.
pub fn is_lossy(text: &str, encoding: Encoding) -> bool {
    match encoding {
        // Unicode, all of it.
        Encoding::Utf8 | Encoding::Utf8Bom | Encoding::Utf16Le | Encoding::Utf16Be => false,
        Encoding::Latin1 => text.chars().any(|c| (c as u32) >= 0x100),
    }
}

fn finish(text: String, encoding: Encoding) -> Decoded {
    // CRLF if any line ends that way. A file with mixed endings is normalised
    // to the one it uses first, which is what every editor does and is the
    // only option that does not require storing an ending per line.
    let line_ending = if text.contains("\r\n") { LineEnding::Crlf } else { LineEnding::Lf };
    Decoded { text, encoding, line_ending }
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Result<String, String> {
    if bytes.len() % 2 != 0 {
        return Err("is UTF-16 with a trailing odd byte".to_string());
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|p| if big_endian { u16::from_be_bytes([p[0], p[1]]) } else { u16::from_le_bytes([p[0], p[1]]) })
        .collect();
    String::from_utf16(&units).map_err(|_| "is UTF-16 with an unpaired surrogate".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The property that makes opening these files safe: whatever was read can
    /// be written back byte-identical.
    ///
    /// Without it, decoding would have been worse than the refusal it
    /// replaced — `save` writes UTF-8, so a UTF-16 file would have come back
    /// re-encoded with its mark left as mojibake.
    #[test]
    fn decoding_then_encoding_returns_the_original_bytes() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("plain ascii", b"hello\nworld\n".to_vec()),
            ("utf-8 with content above ascii", "héllo — wörld\n".as_bytes().to_vec()),
            ("utf-8 with a BOM", {
                let mut v = vec![0xEF, 0xBB, 0xBF];
                v.extend_from_slice("hello\n".as_bytes());
                v
            }),
            ("utf-16 le", {
                let mut v = vec![0xFF, 0xFE];
                for u in "hi\n".encode_utf16() { v.extend_from_slice(&u.to_le_bytes()); }
                v
            }),
            ("utf-16 be", {
                let mut v = vec![0xFE, 0xFF];
                for u in "hi\n".encode_utf16() { v.extend_from_slice(&u.to_be_bytes()); }
                v
            }),
            // 0xE9 is `é` in latin-1 and invalid on its own in UTF-8.
            ("latin-1", vec![b'c', b'a', b'f', 0xE9, b'\n']),
            ("latin-1, every byte value", (1u8..=255).collect()),
        ];

        for (name, bytes) in cases {
            let d = decode(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            let back = encode(&d.text, d.encoding);
            assert_eq!(back, bytes, "{name} did not survive a round trip ({:?})", d.encoding);
        }
    }

    #[test]
    fn the_encoding_is_recognised() {
        let utf16 = {
            let mut v = vec![0xFF, 0xFE];
            for u in "x".encode_utf16() { v.extend_from_slice(&u.to_le_bytes()); }
            v
        };
        assert_eq!(decode(b"plain").unwrap().encoding, Encoding::Utf8);
        assert_eq!(decode(&utf16).unwrap().encoding, Encoding::Utf16Le);
        assert_eq!(decode(&[0xEF, 0xBB, 0xBF, b'x']).unwrap().encoding, Encoding::Utf8Bom);
        assert_eq!(decode(&[b'c', 0xE9]).unwrap().encoding, Encoding::Latin1);
    }

    /// A latin-1 file is text; an executable is not. The NUL check is what
    /// keeps the refusal working for the files it was right about.
    #[test]
    fn binaries_are_still_refused() {
        // An ELF header: invalid UTF-8 and full of NULs.
        let elf = [0x7F, b'E', b'L', b'F', 2, 1, 1, 0, 0, 0, 0, 0, 0xFF, 0xFE, 0x00];
        let err = decode(&elf).expect_err("an ELF binary should not open as text");
        assert!(err.contains("binary"), "{err}");

        // And a latin-1 text file with no NULs still opens.
        assert!(decode(&[b'c', b'a', b'f', 0xE9]).is_ok());
    }

    #[test]
    fn line_endings_are_detected() {
        assert_eq!(decode(b"a\nb\n").unwrap().line_ending, LineEnding::Lf);
        assert_eq!(decode(b"a\r\nb\r\n").unwrap().line_ending, LineEnding::Crlf);
        // Mixed: the ending it uses at all wins, so a save does not strip the
        // CRLFs that are there.
        assert_eq!(decode(b"a\nb\r\n").unwrap().line_ending, LineEnding::Crlf);
    }

    /// Typing a character latin-1 cannot hold must be detectable, because
    /// writing it would silently replace it.
    #[test]
    fn a_character_latin_1_cannot_hold_is_reported_as_lossy() {
        assert!(!is_lossy("café", Encoding::Latin1), "é fits in latin-1");
        assert!(is_lossy("日本語", Encoding::Latin1));
        assert!(is_lossy("emoji 🎉", Encoding::Latin1));
        // And never for the Unicode encodings.
        for enc in [Encoding::Utf8, Encoding::Utf8Bom, Encoding::Utf16Le, Encoding::Utf16Be] {
            assert!(!is_lossy("日本語 🎉", enc), "{enc:?}");
        }
    }

    /// Bytes that start like UTF-16 but are not fall through to the other
    /// checks instead of erroring as broken UTF-16.
    ///
    /// Binaries begin FF FE by coincidence often enough that reporting
    /// "UTF-16 with a trailing odd byte" for one was naming the wrong thing —
    /// an existing test caught exactly that, with a `.bin` fixture whose first
    /// two bytes happened to be a byte-order mark.
    #[test]
    fn something_that_only_looks_like_utf16_is_not_reported_as_broken_utf16() {
        // Odd length after the mark, no NULs: readable as latin-1.
        let d = decode(&[0xFF, 0xFE, 0x41]).expect("falls through to latin-1");
        assert_eq!(d.encoding, Encoding::Latin1);

        // An unpaired surrogate, with a NUL: a binary, and reported as one.
        let err = match decode(&[0xFF, 0xFE, 0x00, 0xD8]) {
            Err(e) => e,
            Ok(_) => panic!("bytes with a NUL should not decode as text"),
        };
        assert!(err.contains("binary"), "{err}");
    }
}
