// SPDX-License-Identifier: Apache-2.0
//! How many terminal columns a character occupies.
//!
//! The grid stored one `char` per cell and advanced the cursor by one, so a
//! double-width glyph — every CJK ideograph, every fullwidth form, most
//! emoji — took one column where the program writing to the terminal had
//! assumed two. Everything after it on the row was off by one, and a
//! `git log` of a repository with Chinese commit messages came apart.
//!
//! Applications decide their own padding using these widths, so the grid has
//! to agree with them or the alignment the application computed is wrong
//! before it is drawn. This is the same table `wcwidth` implements, and the
//! source of truth is Unicode's East Asian Width property (UAX #11): `W` and
//! `F` are two columns, and combining marks are zero.
//!
//! Hand-written rather than pulled from `unicode-width`, per the house rule.
//! It costs about sixty lines of ranges, and the ranges are the stable part of
//! Unicode — the blocks below have not moved since they were assigned. What a
//! new Unicode version changes is which codepoints *inside* the unassigned
//! gaps become wide, so this errs the way a terminal should: an unknown
//! codepoint is one column, which misaligns one glyph rather than the rest of
//! the row.

/// Columns occupied by `c`: 0, 1, or 2.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;

    // C0/C1 controls never reach the grid as text — the parser consumes them —
    // but a stray one should not be given a column either.
    if cp < 0x20 || (0x7F..0xA0).contains(&cp) {
        return 0;
    }

    // Zero-width: combining marks, joiners, and the variation selectors.
    // A terminal cell holds one `char`, so these cannot be attached to the
    // glyph they modify; giving them zero columns at least keeps the rest of
    // the row aligned, which is the property the alternative destroys. See the
    // module note in `terminal.rs` about what this does not do.
    if matches!(cp,
        0x0300..=0x036F   // combining diacritical marks
      | 0x0483..=0x0489
      | 0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7
      | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670
      | 0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED
      | 0x0711 | 0x0730..=0x074A | 0x07A6..=0x07B0 | 0x07EB..=0x07F3
      | 0x0816..=0x0819 | 0x081B..=0x0823 | 0x0825..=0x0827 | 0x0829..=0x082D
      | 0x0900..=0x0903 | 0x093A..=0x093C | 0x0941..=0x0948 | 0x094D
      | 0x0951..=0x0957 | 0x0962..=0x0963
      | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E          // Thai marks
      | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF                    // more combining
      | 0x200B..=0x200F                                      // ZW space/joiners, marks
      | 0x2060..=0x2064
      | 0x20D0..=0x20F0                                      // combining for symbols
      | 0xFE00..=0xFE0F                                       // variation selectors
      | 0xFE20..=0xFE2F                                       // combining half marks
      | 0xFEFF                                                // BOM as a character
      | 0xE0100..=0xE01EF                                     // variation supplement
    ) {
        return 0;
    }

    // Two columns: East Asian Wide (W) and Fullwidth (F).
    if matches!(cp,
        0x1100..=0x115F     // Hangul Jamo initial consonants
      | 0x2E80..=0x303E     // CJK radicals, Kangxi, CJK symbols/punctuation
      | 0x3041..=0x33FF     // Hiragana, Katakana, Bopomofo, Hangul compat, CJK compat
      | 0x3400..=0x4DBF     // CJK unified extension A
      | 0x4E00..=0x9FFF     // CJK unified ideographs
      | 0xA000..=0xA4CF     // Yi
      | 0xAC00..=0xD7A3     // Hangul syllables
      | 0xF900..=0xFAFF     // CJK compatibility ideographs
      | 0xFE10..=0xFE19     // vertical forms
      | 0xFE30..=0xFE6F     // CJK compatibility forms, small form variants
      | 0xFF00..=0xFF60     // fullwidth ASCII forms
      | 0xFFE0..=0xFFE6     // fullwidth signs
      | 0x1F300..=0x1F64F   // emoji: symbols/pictographs, emoticons
      | 0x1F900..=0x1F9FF   // supplemental symbols and pictographs
      | 0x20000..=0x2FFFD   // CJK extensions B-F
      | 0x30000..=0x3FFFD   // CJK extension G
    ) {
        return 2;
    }

    // A handful of two-column characters outside those blocks, common enough
    // in terminal output to be worth naming: the ones shells and TUIs print.
    if matches!(cp,
        0x231A..=0x231B     // watch, hourglass
      | 0x23E9..=0x23EC | 0x23F0 | 0x23F3
      | 0x25FD..=0x25FE
      | 0x2614..=0x2615
      | 0x2648..=0x2653     // zodiac
      | 0x267F | 0x2693 | 0x26A1 | 0x26AA..=0x26AB
      | 0x26BD..=0x26BE | 0x26C4..=0x26C5 | 0x26CE | 0x26D4 | 0x26EA
      | 0x26F2..=0x26F3 | 0x26F5 | 0x26FA | 0x26FD
      | 0x2705 | 0x270A..=0x270B | 0x2728 | 0x274C | 0x274E
      | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF
      | 0x2B1B..=0x2B1C | 0x2B50 | 0x2B55
      | 0x1F004 | 0x1F0CF | 0x1F18E | 0x1F191..=0x1F19A
      | 0x1F200..=0x1F2FF
      | 0x1F680..=0x1F6FF   // transport and map
      | 0x1FA70..=0x1FAFF   // more pictographs
    ) {
        return 2;
    }

    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_is_one_column() {
        for c in "abcXYZ0129 !@#~".chars() {
            assert_eq!(char_width(c), 1, "{c:?}");
        }
    }

    /// The case that misaligned every row after it.
    #[test]
    fn cjk_is_two_columns() {
        for c in "日本語漢字中文한국어".chars() {
            assert_eq!(char_width(c), 2, "{c:?} U+{:04X}", c as u32);
        }
        // Fullwidth ASCII forms, which shells print in Japanese locales.
        assert_eq!(char_width('Ａ'), 2);
        assert_eq!(char_width('１'), 2);
    }

    #[test]
    fn hiragana_and_katakana_are_two_columns() {
        for c in "あいうえおアイウエオ".chars() {
            assert_eq!(char_width(c), 2, "{c:?}");
        }
    }

    /// Emoji a shell prompt or a test runner actually prints.
    #[test]
    fn common_emoji_are_two_columns() {
        for c in "✅❌🎉🚀⚡🔥📦🐛".chars() {
            assert_eq!(char_width(c), 2, "{c:?} U+{:04X}", c as u32);
        }
    }

    #[test]
    fn combining_marks_and_selectors_are_zero() {
        assert_eq!(char_width('\u{0301}'), 0, "combining acute");
        assert_eq!(char_width('\u{FE0F}'), 0, "variation selector-16");
        assert_eq!(char_width('\u{200D}'), 0, "zero-width joiner");
    }

    #[test]
    fn controls_take_no_column() {
        assert_eq!(char_width('\n'), 0);
        assert_eq!(char_width('\r'), 0);
        assert_eq!(char_width('\u{7F}'), 0);
    }

    /// Box-drawing and the symbols TUIs use for borders must stay single
    /// width, or every framed layout breaks the other way.
    #[test]
    fn box_drawing_stays_one_column() {
        for c in "─│┌┐└┘├┤┬┴┼━┃╭╮╰╯█▀▄░▒▓".chars() {
            assert_eq!(char_width(c), 1, "{c:?} U+{:04X}", c as u32);
        }
        // Powerline separators, which prompts use heavily.
        for c in "\u{E0B0}\u{E0B1}\u{E0B2}\u{E0B3}".chars() {
            assert_eq!(char_width(c), 1, "private-use powerline glyph");
        }
        // And the arrows and check marks Forge's own TUI prints.
        for c in "→←↑↓✔✗●◆".chars() {
            assert_eq!(char_width(c), 1, "{c:?} U+{:04X}", c as u32);
        }
    }

    /// Widths sum the way an application's own padding arithmetic assumes.
    #[test]
    fn widths_sum_across_a_string() {
        let w = |s: &str| s.chars().map(char_width).sum::<usize>();
        assert_eq!(w("abc"), 3);
        assert_eq!(w("日本"), 4);
        assert_eq!(w("a日b"), 4);
        assert_eq!(w("e\u{0301}"), 1, "a combining mark adds nothing");
    }
}
