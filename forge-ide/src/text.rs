// SPDX-License-Identifier: Apache-2.0
//! Text handling that has to survive whatever a person or a model produces.

/// Truncate to at most `max` **characters**, cutting on a character boundary.
///
/// `&s[..n]` slices by byte, and panics when byte `n` lands inside a multi-byte
/// character. That is not an edge case for this program: everything truncated
/// here is text someone typed or a model wrote — an emoji in a prompt, a CJK
/// identifier, an accented word, or the box-drawing characters models reach for
/// when they draw a diagram. One of those straddling the cut is enough to take
/// the process down, which is how a `▎` in a first message crashed a session.
pub fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((byte_index, _)) => &s[..byte_index],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_chars;

    /// Plain ASCII behaves exactly as a byte slice would.
    #[test]
    fn ascii_cuts_where_asked() {
        assert_eq!(truncate_chars("hello world", 5), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hi", 5), "hi");
        assert_eq!(truncate_chars("", 5), "");
    }

    /// The case that crashed: a multi-byte character straddling the cut.
    /// `▎` is three bytes, so a byte slice at 1 or 2 lands inside it.
    #[test]
    fn a_multibyte_character_across_the_cut_does_not_panic() {
        let s = "a\u{258E}b";                 // a ▎ b
        assert_eq!(truncate_chars(s, 1), "a");
        assert_eq!(truncate_chars(s, 2), "a\u{258E}");
        assert_eq!(truncate_chars(s, 3), s);
        assert_eq!(truncate_chars(s, 99), s);
    }

    /// Counted in characters, not bytes, so a limit means what a reader expects.
    #[test]
    fn the_limit_counts_characters() {
        let cjk = "\u{4F60}\u{597D}\u{4E16}\u{754C}"; // four 3-byte characters
        assert_eq!(truncate_chars(cjk, 2).chars().count(), 2);
        assert_eq!(truncate_chars(cjk, 4), cjk);

        // An emoji outside the BMP is four bytes and still one cut point.
        let emoji = "\u{1F600}\u{1F600}";
        assert_eq!(truncate_chars(emoji, 1), "\u{1F600}");
    }

    /// Every cut point of a mixed string is a valid boundary — the property
    /// that matters, since callers pick the limit, not this function.
    #[test]
    fn every_cut_point_is_a_boundary() {
        let s = "ab\u{258E}\u{1F600}c\u{00E9}d\u{4F60}";
        for n in 0..=s.chars().count() + 3 {
            let cut = truncate_chars(s, n);
            assert!(s.starts_with(cut), "cut at {n} is not a prefix");
        }
    }
}
