// SPDX-License-Identifier: Apache-2.0
//! Turning terminal bytes into key events.
//!
//! A terminal delivers input as bytes, and everything that is not a plain
//! character arrives as an escape sequence: `ESC [ A` for up, `ESC [ 5 ~` for
//! page up, and a bracketed paste as a whole block between `ESC [ 200 ~` and
//! `ESC [ 201 ~`.
//!
//! The decoder is a state machine over a byte buffer rather than a function over
//! a complete sequence, because a `read` can return in the middle of one — the
//! two halves of `ESC [ A` can and do arrive separately. An incomplete sequence
//! stays buffered until the rest turns up.
//!
//! One ambiguity is unavoidable: a lone `ESC` is both the Escape key and the
//! start of every other sequence, and nothing in the byte stream distinguishes
//! them. Resolved on timing, the way terminals have always resolved it — see
//! [`Decoder::flush_pending_escape`].

/// A decoded key press.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    PageUp,
    PageDown,
    Delete,
    /// A control chord, carrying the letter — `Ctrl('c')` for Ctrl-C.
    Ctrl(char),
    /// A bracketed paste, already assembled.
    Paste(String),
}

/// How long a lone `ESC` waits for a sequence to continue before it is treated
/// as the Escape key.
///
/// Terminals send `ESC [ A` as one write, so in practice the continuation is
/// already in the buffer and this only applies to a genuine Escape press. The
/// value is a compromise every terminal application makes: too short and a slow
/// connection turns arrow keys into Escape presses, too long and Escape feels
/// unresponsive.
pub const ESCAPE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(25);

/// Incremental decoder over a byte stream.
#[derive(Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// True while holding a lone `ESC`, waiting to see if more follows.
    pending_escape: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a partial sequence is held, so the caller knows to wait rather
    /// than treating quiet as end of input.
    pub fn has_pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Feed bytes and take whatever keys are now complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.buf.extend_from_slice(bytes);
        self.pending_escape = false;
        let mut keys = Vec::new();
        loop {
            match self.next_key() {
                Step::Key(key) => keys.push(key),
                // Bytes were consumed but mean nothing to us — an unbound
                // sequence, an Alt chord, a bad byte. Keep going: stopping here
                // would strand every key that followed in the same read.
                Step::Skipped => continue,
                Step::Incomplete => break,
            }
        }
        keys
    }

    /// Resolve a lone `ESC` as the Escape key.
    ///
    /// Called when the caller has waited [`ESCAPE_TIMEOUT`] with nothing further
    /// arriving, which is the only evidence available that no sequence is coming.
    pub fn flush_pending_escape(&mut self) -> Option<Key> {
        if self.buf == [0x1b] {
            self.buf.clear();
            self.pending_escape = false;
            return Some(Key::Escape);
        }
        None
    }

    fn next_key(&mut self) -> Step {
        let Some(&first) = self.buf.first() else {
            return Step::Incomplete;
        };

        match first {
            0x1b => self.escape_sequence(),
            b'\r' | b'\n' => {
                self.buf.drain(..1);
                Step::Key(Key::Enter)
            }
            0x7f | 0x08 => {
                self.buf.drain(..1);
                Step::Key(Key::Backspace)
            }
            b'\t' => {
                self.buf.drain(..1);
                Step::Key(Key::Tab)
            }
            // C0 control codes are Ctrl chords: 0x01 is Ctrl-A. Enter, Tab and
            // Backspace are handled above because they have their own meaning.
            0x01..=0x1a => {
                self.buf.drain(..1);
                let letter = (b'a' + (first - 1)) as char;
                Step::Key(Key::Ctrl(letter))
            }
            _ => self.character(),
        }
    }

    /// Decode one UTF-8 character, waiting if it is incomplete.
    fn character(&mut self) -> Step {
        let Some(&first) = self.buf.first() else {
            return Step::Incomplete;
        };
        let len = utf8_len(first);

        if self.buf.len() < len {
            return Step::Incomplete; // split across reads
        }
        match std::str::from_utf8(&self.buf[..len]) {
            Ok(text) => match text.chars().next() {
                Some(c) => {
                    self.buf.drain(..len);
                    Step::Key(Key::Char(c))
                }
                None => Step::Incomplete,
            },
            Err(_) => {
                // Not valid UTF-8. Drop one byte and resynchronise rather than
                // stalling forever on a byte that will never complete.
                self.buf.drain(..1);
                Step::Skipped
            }
        }
    }

    fn escape_sequence(&mut self) -> Step {
        // Just the ESC so far: it could be Escape, or the start of anything.
        // Hold it and let the caller decide on timing.
        if self.buf.len() == 1 {
            self.pending_escape = true;
            return Step::Incomplete;
        }

        match self.buf[1] {
            b'[' => self.csi(),
            // SS3, used by some terminals for arrows and function keys in
            // application mode: `ESC O A`.
            b'O' => {
                if self.buf.len() < 3 {
                    return Step::Incomplete;
                }
                let key = match self.buf[2] {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    _ => {
                        self.buf.drain(..3);
                        return Step::Skipped;
                    }
                };
                self.buf.drain(..3);
                Step::Key(key)
            }
            // ESC followed by anything else: Alt-<key> on most terminals. Not
            // bound, so consume both and move on rather than leaving the ESC to
            // be misread as the start of a sequence.
            _ => {
                self.buf.drain(..2);
                Step::Skipped
            }
        }
    }

    /// A control sequence: `ESC [ <params> <final>`.
    fn csi(&mut self) -> Step {
        // Find the final byte, which is the first in the range @ to ~.
        let mut end = None;
        for (i, byte) in self.buf.iter().enumerate().skip(2) {
            if (0x40..=0x7e).contains(byte) {
                end = Some(i);
                break;
            }
        }
        let Some(end) = end else {
            return Step::Incomplete; // still arriving
        };

        let params = String::from_utf8_lossy(&self.buf[2..end]).to_string();
        let final_byte = self.buf[end];

        // A paste is a block, not a key: everything up to the closing marker is
        // literal text, including newlines and anything that looks like a
        // sequence.
        if final_byte == b'~' && params == "200" {
            return self.bracketed_paste(end);
        }

        self.buf.drain(..=end);

        // Kitty keyboard protocol: `CSI <codepoint> [;<mods>] u`. We ask for
        // legacy encoding and never want this — but a terminal left in enhanced
        // mode by a program that died before popping will send it anyway, and
        // dropping it makes the keyboard appear dead. Decoding it costs little
        // and turns a broken session into a working one.
        if final_byte == b'u' {
            return self.kitty_key(&params);
        }

        Step::Key(match (final_byte, params.as_str()) {
            (b'A', _) => Key::Up,
            (b'B', _) => Key::Down,
            (b'C', _) => Key::Right,
            (b'D', _) => Key::Left,
            (b'H', _) => Key::Home,
            (b'F', _) => Key::End,
            (b'~', "1" | "7") => Key::Home,
            (b'~', "4" | "8") => Key::End,
            (b'~', "3") => Key::Delete,
            (b'~', "5") => Key::PageUp,
            (b'~', "6") => Key::PageDown,
            // Unrecognised but well-formed: consumed above, nothing to report.
            _ => return Step::Skipped,
        })
    }

    /// Decode a Kitty-protocol key event.
    ///
    /// The shape is `codepoint[:alternate][;modifiers[:event]]`. Only the parts
    /// that change what key was pressed are read: the codepoint, whether Ctrl was
    /// held, and whether this is a release — a release reported as a press would
    /// double every keystroke.
    fn kitty_key(&mut self, params: &str) -> Step {
        let mut fields = params.split(';');
        let key_field = fields.next().unwrap_or("");
        let modifier_field = fields.next().unwrap_or("");

        // `1` press, `2` repeat, `3` release. Anything but a release is input.
        let event_type = modifier_field.split(':').nth(1).unwrap_or("1");
        if event_type == "3" {
            return Step::Skipped;
        }

        let Ok(code) = key_field.split(':').next().unwrap_or("").parse::<u32>() else {
            return Step::Skipped;
        };

        // Modifiers are a bitmask, biased by one: 1 means none.
        let mods = modifier_field
            .split(':')
            .next()
            .unwrap_or("1")
            .parse::<u32>()
            .unwrap_or(1)
            .saturating_sub(1);
        let ctrl = mods & 0b100 != 0;

        // The special keys the protocol gives their own codepoints.
        let key = match code {
            13 => Key::Enter,
            9 => Key::Tab,
            127 | 8 => Key::Backspace,
            27 => Key::Escape,
            _ => match char::from_u32(code) {
                Some(c) if ctrl && c.is_ascii_alphabetic() => {
                    Key::Ctrl(c.to_ascii_lowercase())
                }
                Some(c) if !c.is_control() => Key::Char(c),
                _ => return Step::Skipped,
            },
        };
        Step::Key(key)
    }

    /// Assemble a bracketed paste, from after `ESC[200~` to before `ESC[201~`.
    fn bracketed_paste(&mut self, start_end: usize) -> Step {
        const CLOSE: &[u8] = b"\x1b[201~";
        let body_start = start_end + 1;

        let Some(offset) = find(&self.buf[body_start..], CLOSE) else {
            // The close marker has not arrived; hold the whole block.
            return Step::Incomplete;
        };
        let close_at = offset + body_start;
        let text = String::from_utf8_lossy(&self.buf[body_start..close_at]).to_string();
        self.buf.drain(..close_at + CLOSE.len());
        Step::Key(Key::Paste(text))
    }
}

/// One step of decoding.
///
/// `Skipped` is the case a plain `Option` cannot express: bytes were consumed but
/// carried no key. Conflating it with "nothing available" made the decoder stop
/// early and strand every key that followed in the same read.
enum Step {
    Key(Key),
    Skipped,
    Incomplete,
}

/// Length in bytes of the UTF-8 character starting with `first`.
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        // A continuation byte or an invalid lead; treated as one byte so the
        // decoder discards it and resynchronises.
        _ => 1,
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(bytes: &[u8]) -> Vec<Key> {
        Decoder::new().feed(bytes)
    }

    // ── Plain characters ──────────────────────────────────────────────────

    #[test]
    fn ascii_characters_decode() {
        assert_eq!(
            decode(b"abc"),
            vec![Key::Char('a'), Key::Char('b'), Key::Char('c')],
        );
    }

    #[test]
    fn enter_backspace_and_tab_are_recognised() {
        assert_eq!(decode(b"\r"), vec![Key::Enter]);
        assert_eq!(decode(b"\n"), vec![Key::Enter]);
        assert_eq!(decode(b"\x7f"), vec![Key::Backspace], "DEL is backspace");
        assert_eq!(decode(b"\x08"), vec![Key::Backspace], "BS is too");
        assert_eq!(decode(b"\t"), vec![Key::Tab]);
    }

    #[test]
    fn control_chords_carry_their_letter() {
        assert_eq!(decode(b"\x03"), vec![Key::Ctrl('c')]);
        assert_eq!(decode(b"\x04"), vec![Key::Ctrl('d')]);
        assert_eq!(decode(b"\x18"), vec![Key::Ctrl('x')]);
        assert_eq!(decode(b"\x01"), vec![Key::Ctrl('a')]);
    }

    /// Multi-byte characters must survive; the terminal sends UTF-8 and typing
    /// an emoji is four bytes.
    #[test]
    fn multibyte_characters_decode() {
        assert_eq!(decode("é".as_bytes()), vec![Key::Char('é')]);
        assert_eq!(decode("日".as_bytes()), vec![Key::Char('日')]);
        assert_eq!(decode("👨".as_bytes()), vec![Key::Char('\u{1F468}')]);
    }

    /// A read can split a character. Holding the partial bytes is the whole
    /// reason this is a state machine.
    #[test]
    fn a_character_split_across_reads_is_reassembled() {
        let bytes = "日".as_bytes();
        let mut d = Decoder::new();
        assert_eq!(d.feed(&bytes[..1]), vec![], "incomplete, nothing yet");
        assert_eq!(d.feed(&bytes[1..2]), vec![], "still incomplete");
        assert_eq!(d.feed(&bytes[2..]), vec![Key::Char('日')], "now complete");
    }

    #[test]
    fn invalid_utf8_does_not_stall_the_decoder() {
        let mut d = Decoder::new();
        // A lone continuation byte can never complete a character.
        let keys = d.feed(&[0x80, b'a']);
        assert_eq!(keys, vec![Key::Char('a')], "resynchronised onto the next byte");
        assert!(!d.has_pending());
    }

    // ── Arrows and navigation ─────────────────────────────────────────────

    #[test]
    fn csi_arrows_decode() {
        assert_eq!(decode(b"\x1b[A"), vec![Key::Up]);
        assert_eq!(decode(b"\x1b[B"), vec![Key::Down]);
        assert_eq!(decode(b"\x1b[C"), vec![Key::Right]);
        assert_eq!(decode(b"\x1b[D"), vec![Key::Left]);
    }

    /// Some terminals use SS3 for arrows in application mode.
    #[test]
    fn ss3_arrows_decode_too() {
        assert_eq!(decode(b"\x1bOA"), vec![Key::Up]);
        assert_eq!(decode(b"\x1bOB"), vec![Key::Down]);
        assert_eq!(decode(b"\x1bOH"), vec![Key::Home]);
        assert_eq!(decode(b"\x1bOF"), vec![Key::End]);
    }

    #[test]
    fn navigation_keys_decode() {
        assert_eq!(decode(b"\x1b[5~"), vec![Key::PageUp]);
        assert_eq!(decode(b"\x1b[6~"), vec![Key::PageDown]);
        assert_eq!(decode(b"\x1b[3~"), vec![Key::Delete]);
        assert_eq!(decode(b"\x1b[H"), vec![Key::Home]);
        assert_eq!(decode(b"\x1b[F"), vec![Key::End]);
        // The numeric forms both terminals use.
        assert_eq!(decode(b"\x1b[1~"), vec![Key::Home]);
        assert_eq!(decode(b"\x1b[4~"), vec![Key::End]);
    }

    /// Modified keys arrive with parameters; the base key still has to work
    /// rather than the whole sequence being dropped.
    #[test]
    fn arrows_with_modifier_parameters_still_decode() {
        assert_eq!(decode(b"\x1b[1;5A"), vec![Key::Up], "Ctrl-Up is still Up");
        assert_eq!(decode(b"\x1b[1;2B"), vec![Key::Down], "Shift-Down is still Down");
    }

    /// A sequence split mid-way is the common case on a slow connection.
    #[test]
    fn a_sequence_split_across_reads_is_reassembled() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b"), vec![], "could be Escape or a sequence");
        assert_eq!(d.feed(b"["), vec![], "still ambiguous");
        assert_eq!(d.feed(b"A"), vec![Key::Up]);
        assert!(!d.has_pending());
    }

    #[test]
    fn an_unrecognised_sequence_is_consumed_not_left_behind() {
        let mut d = Decoder::new();
        // A well-formed but unbound sequence, then a real key.
        let keys = d.feed(b"\x1b[99Za");
        assert_eq!(keys, vec![Key::Char('a')], "the letter still arrives");
        assert!(!d.has_pending(), "nothing left to confuse the next read");
    }

    // ── The lone Escape ───────────────────────────────────────────────────

    /// The ambiguity: nothing in the bytes distinguishes Escape from the start
    /// of a sequence, so it is held rather than guessed at.
    #[test]
    fn a_lone_escape_is_held_until_the_timeout() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b"), vec![], "not resolved yet");
        assert!(d.has_pending());
        assert_eq!(d.flush_pending_escape(), Some(Key::Escape));
        assert!(!d.has_pending());
    }

    #[test]
    fn flushing_does_nothing_when_a_sequence_is_in_progress() {
        let mut d = Decoder::new();
        d.feed(b"\x1b[");
        assert_eq!(
            d.flush_pending_escape(), None,
            "a partial CSI must not be reported as Escape",
        );
    }

    #[test]
    fn flushing_with_an_empty_buffer_does_nothing() {
        assert_eq!(Decoder::new().flush_pending_escape(), None);
    }

    /// Escape followed by a letter is Alt-<letter> on most terminals. Unbound,
    /// but it must not leave the ESC behind to corrupt the next sequence.
    #[test]
    fn alt_chords_are_consumed_cleanly() {
        let mut d = Decoder::new();
        let keys = d.feed(b"\x1bxa");
        assert_eq!(keys, vec![Key::Char('a')]);
        assert!(!d.has_pending());
    }

    // ── Kitty keyboard protocol ───────────────────────────────────────────

    /// We never ask for this, but a terminal left in enhanced mode by a program
    /// that died before popping will send it. Dropping it makes the keyboard
    /// appear dead, which is far worse than decoding a mode we did not request.
    #[test]
    fn kitty_protocol_key_events_are_decoded() {
        assert_eq!(decode(b"\x1b[97u"), vec![Key::Char('a')]);
        assert_eq!(decode(b"\x1b[13u"), vec![Key::Enter]);
        assert_eq!(decode(b"\x1b[9u"), vec![Key::Tab]);
        assert_eq!(decode(b"\x1b[27u"), vec![Key::Escape]);
        assert_eq!(decode(b"\x1b[127u"), vec![Key::Backspace]);
    }

    /// A release reported as a press would double every keystroke.
    #[test]
    fn kitty_release_events_are_ignored() {
        assert_eq!(decode(b"\x1b[97;1:3u"), vec![], "release produces nothing");
        assert_eq!(decode(b"\x1b[97;1:1u"), vec![Key::Char('a')], "press does");
        assert_eq!(decode(b"\x1b[97;1:2u"), vec![Key::Char('a')], "and so does a repeat");
    }

    #[test]
    fn kitty_ctrl_chords_are_decoded() {
        // Modifier 5 = ctrl (bitmask 4, biased by one).
        assert_eq!(decode(b"\x1b[99;5u"), vec![Key::Ctrl('c')]);
        assert_eq!(decode(b"\x1b[116;5u"), vec![Key::Ctrl('t')]);
    }

    #[test]
    fn kitty_alternate_key_codes_are_tolerated() {
        // `codepoint:shifted` — the base codepoint is what matters.
        assert_eq!(decode(b"\x1b[97:65u"), vec![Key::Char('a')]);
    }

    /// A `u` sequence with no usable codepoint must be skipped, and decoding
    /// must carry on afterwards.
    #[test]
    fn a_malformed_kitty_sequence_is_skipped_not_fatal() {
        let mut d = Decoder::new();
        let keys = d.feed(b"\x1b[;;u!");
        assert_eq!(keys, vec![Key::Char('!')], "recovered: {keys:?}");
        assert!(!d.has_pending());
    }

    /// Worth pinning: any letter terminates a CSI sequence, whether or not we
    /// bind it. `ESC [ a` is a complete (unbound) sequence, so it is consumed
    /// whole and the keys after it still arrive — it does not swallow them while
    /// waiting for a terminator that already came.
    #[test]
    fn an_unbound_letter_still_terminates_the_sequence() {
        let mut d = Decoder::new();
        let keys = d.feed(b"\x1b[a" as &[u8]);
        assert!(keys.is_empty(), "unbound, so nothing is reported: {keys:?}");
        assert!(!d.has_pending(), "but it was consumed, not left buffered");
        assert_eq!(d.feed(b"z"), vec![Key::Char('z')], "the next key arrives");
    }

    #[test]
    fn multibyte_kitty_codepoints_decode() {
        // U+65E5 (日)
        assert_eq!(decode("\x1b[26085u".as_bytes()), vec![Key::Char('日')]);
    }

    // ── Bracketed paste ───────────────────────────────────────────────────

    #[test]
    fn a_bracketed_paste_arrives_as_one_key() {
        assert_eq!(
            decode(b"\x1b[200~hello world\x1b[201~"),
            vec![Key::Paste("hello world".into())],
        );
    }

    /// The point of bracketed paste: pasted newlines are text, not Enter. Without
    /// this a multi-line paste would submit partway through.
    #[test]
    fn newlines_inside_a_paste_are_text_not_enter() {
        let keys = decode(b"\x1b[200~one\ntwo\nthree\x1b[201~");
        assert_eq!(keys, vec![Key::Paste("one\ntwo\nthree".into())]);
    }

    /// Pasted content can contain anything, including things that look like
    /// escape sequences. They must not be interpreted.
    #[test]
    fn escape_sequences_inside_a_paste_are_literal() {
        let keys = decode(b"\x1b[200~text \x1b[A more\x1b[201~");
        assert_eq!(keys.len(), 1, "one paste, no arrow key");
        match &keys[0] {
            Key::Paste(text) => assert!(text.contains("more"), "kept whole: {text:?}"),
            other => panic!("expected a paste, got {other:?}"),
        }
    }

    #[test]
    fn an_unterminated_paste_waits_for_its_close() {
        let mut d = Decoder::new();
        assert_eq!(d.feed(b"\x1b[200~partial"), vec![], "not complete");
        assert!(d.has_pending());
        assert_eq!(
            d.feed(b" rest\x1b[201~"),
            vec![Key::Paste("partial rest".into())],
        );
    }

    #[test]
    fn an_empty_paste_is_still_a_paste() {
        assert_eq!(decode(b"\x1b[200~\x1b[201~"), vec![Key::Paste(String::new())]);
    }

    #[test]
    fn keys_after_a_paste_still_decode() {
        assert_eq!(
            decode(b"\x1b[200~x\x1b[201~\r"),
            vec![Key::Paste("x".into()), Key::Enter],
        );
    }

    // ── Mixed streams ─────────────────────────────────────────────────────

    /// A realistic burst: typing, an arrow, a control chord.
    #[test]
    fn a_mixed_burst_decodes_in_order() {
        assert_eq!(
            decode(b"hi\x1b[A\x03"),
            vec![Key::Char('h'), Key::Char('i'), Key::Up, Key::Ctrl('c')],
        );
    }

    #[test]
    fn an_empty_feed_produces_nothing() {
        assert_eq!(decode(b""), vec![]);
    }

    /// Every byte value must be survivable — a terminal can deliver anything and
    /// a panic in the input path would take the session down.
    #[test]
    fn no_byte_sequence_panics() {
        let mut d = Decoder::new();
        for byte in 0u8..=255 {
            d.feed(&[byte]);
        }
        // And a sweep of pairs through the escape paths.
        for a in [0x1b, b'[', b'O', b'~', 0x80, 0xff] {
            for b in 0u8..=255 {
                Decoder::new().feed(&[a, b]);
            }
        }
    }
}
