// SPDX-License-Identifier: Apache-2.0
//! How wide text is, in terminal cells.
//!
//! This is the only place that answers that question. Layout wraps with it and
//! the renderer advances the cursor with it, so the two cannot disagree about
//! where a line ends.
//!
//! That matters because disagreement is exactly what broke the previous TUI. Ink
//! measured with JavaScript's `string-width` while forge-ide's grid advanced one
//! cell per character; when a line straddled the wrap boundary the two computed
//! different row counts, ink's cursor-up landed in the wrong place, and messages
//! drew over each other. Two implementations of one rule will always drift. One
//! implementation cannot.
//!
//! The unit is the *grapheme cluster*, not the character. A cluster is what a
//! reader calls one symbol and what a terminal advances the cursor for once,
//! even when it is many codepoints: `e` + combining acute is one cell, and a
//! ZWJ family emoji is two cells rather than the six a naive per-codepoint sum
//! would give.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Zero-width joiner. Its presence marks an emoji sequence that renders as a
/// single glyph, however many codepoints it is built from.
const ZWJ: char = '\u{200D}';
/// Variation selector 16, the "render the preceding character as emoji" request.
/// It turns an otherwise 1-cell symbol into a 2-cell one: `⚠` is narrow, `⚠️`
/// is not.
const VS16: char = '\u{FE0F}';

/// Width of a single grapheme cluster, in cells.
///
/// Returns 0 for clusters that occupy no space (control characters, lone
/// combining marks), 2 for wide and emoji-presented clusters, 1 otherwise.
pub fn cluster_width(cluster: &str) -> usize {
    let mut chars = cluster.chars();
    let Some(first) = chars.next() else { return 0 };

    // An emoji sequence renders as one glyph regardless of how many codepoints
    // it carries, so summing them would badly overcount. Terminals that support
    // these draw them double-width; ones that don't fall back to drawing the
    // first emoji, which is also double-width. Either way the advance is 2.
    if cluster.contains(ZWJ) || cluster.contains(VS16) {
        return 2;
    }

    // Control characters are emitted by nobody sane and advance nothing.
    if first.is_control() {
        return 0;
    }

    // Otherwise the sum over codepoints is already right: combining marks score
    // 0, East Asian wide characters score 2, and a regional-indicator pair (a
    // flag) scores 1 + 1 = 2, which is what terminals draw.
    UnicodeWidthStr::width(cluster)
}

/// Width of a string, in cells.
pub fn str_width(s: &str) -> usize {
    s.graphemes(true).map(cluster_width).sum()
}

/// Split `s` into lines that each fit within `cols` cells.
///
/// Breaks at word boundaries where it can and mid-word when a single word is
/// wider than the line. Measured with [`cluster_width`], so the result is
/// consistent with what the renderer will do when it draws these lines.
pub fn wrap(s: &str, cols: usize) -> Vec<String> {
    if cols == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut line_w = 0usize;

    for word in split_keeping_spaces(s) {
        let word_w = str_width(&word);

        // Trailing spaces at a break point would push content off the edge for
        // no visible gain.
        if line_w + word_w > cols && word.trim().is_empty() {
            lines.push(std::mem::take(&mut line));
            line_w = 0;
            continue;
        }

        if line_w + word_w > cols && line_w > 0 {
            lines.push(std::mem::take(&mut line));
            line_w = 0;
        }

        if word_w > cols {
            // A single word too wide for any line: fill the current line and
            // carry the rest, splitting on cluster boundaries so no glyph is
            // torn in half.
            for cluster in word.graphemes(true) {
                let w = cluster_width(cluster);
                // A glyph wider than the entire line can never be placed — a
                // two-cell character in a one-column terminal has nowhere to go.
                // Skipping keeps the contract that every returned line fits;
                // emitting it would hand the renderer a line it must clip, and
                // clipping mid-glyph is how cells and columns fall out of step.
                if w > cols {
                    continue;
                }
                if line_w + w > cols {
                    lines.push(std::mem::take(&mut line));
                    line_w = 0;
                }
                line.push_str(cluster);
                line_w += w;
            }
            continue;
        }

        line.push_str(&word);
        line_w += word_w;
    }

    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

/// Split into words, keeping the whitespace attached so wrapping can decide
/// what to do with it rather than silently collapsing runs of spaces.
fn split_keeping_spaces(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_space = false;

    for cluster in s.graphemes(true) {
        let is_space = cluster.chars().all(|c| c == ' ' || c == '\t');
        if current.is_empty() {
            in_space = is_space;
        } else if is_space != in_space {
            out.push(std::mem::take(&mut current));
            in_space = is_space;
        }
        current.push_str(cluster);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// True when this cluster occupies two cells, so the renderer knows to reserve
/// the neighbouring one.
pub fn is_wide(cluster: &str) -> bool {
    cluster_width(cluster) == 2
}

/// Width of a single `char`, for the rare caller that has no cluster.
pub fn char_width(c: char) -> usize {
    UnicodeWidthChar::width(c).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// These numbers are not invented: they are what JavaScript's `string-width`
    /// reports for the same strings, measured directly against the copy in
    /// forge-tui's node_modules. Matching it matters because it is what ink used,
    /// what most terminals agree with, and what a naive one-cell-per-character
    /// grid got wrong — the third column is what forge-ide's emulator computes
    /// today, and every row where it differs is a row that could overlap.
    ///
    /// | text              | string-width | naive per-char |
    /// |-------------------|--------------|----------------|
    /// | "hello world"     | 11           | 11             |
    /// | "日本語テキスト"  | 14           | 7              |
    /// | "check ✅ done"   | 13           | 12             |
    /// | "warn ⚠️ here"    | 12           | 12             |
    /// | "team 👨‍👩‍👧 here" | 12           | 15             |
    /// | "├── src"         | 7            | 7              |
    /// | "café" (combining)| 4            | 5              |
    #[test]
    fn agrees_with_string_width() {
        let cases: &[(&str, usize)] = &[
            ("hello world", 11),
            ("日本語テキスト", 14),
            ("check ✅ done", 13),
            ("warn \u{26A0}\u{FE0F} here", 12),
            ("team \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} here", 12),
            ("├── src", 7),
            ("cafe\u{301}", 4),
        ];
        for (text, expected) in cases {
            assert_eq!(
                str_width(text), *expected,
                "width of {text:?} must match string-width",
            );
        }
    }

    /// The specific cases a per-codepoint sum gets wrong. Kept separate because
    /// these are the ones that actually caused overlapping messages.
    #[test]
    fn emoji_sequences_are_two_cells_not_the_sum_of_their_parts() {
        // Six codepoints, one glyph, two cells.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        assert_eq!(cluster_width(family), 2, "ZWJ sequence is one glyph");

        // Without VS16 this is a narrow dingbat; with it, an emoji.
        assert_eq!(cluster_width("\u{26A0}"), 1);
        assert_eq!(cluster_width("\u{26A0}\u{FE0F}"), 2);

        // A flag is a regional-indicator pair.
        assert_eq!(cluster_width("\u{1F1FA}\u{1F1F8}"), 2);
    }

    #[test]
    fn combining_marks_add_nothing() {
        assert_eq!(cluster_width("e\u{301}"), 1);
        assert_eq!(str_width("cafe\u{301}"), 4);
    }

    #[test]
    fn control_characters_occupy_nothing() {
        assert_eq!(cluster_width("\u{7}"), 0);
        assert_eq!(cluster_width(""), 0);
    }

    #[test]
    fn wide_characters_are_two_cells() {
        assert_eq!(cluster_width("日"), 2);
        assert!(is_wide("日"));
        assert!(!is_wide("a"));
    }

    /// The property that actually prevents the bug: nothing wrap produces may
    /// exceed the width the renderer will measure it at.
    #[test]
    fn wrapped_lines_never_exceed_the_column_budget() {
        let text = "a ✅ 日本語のテキストと team \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} \
                    mixed with ordinary words and a_very_long_unbreakable_token_here";
        for cols in [1usize, 2, 3, 5, 8, 13, 20, 40, 79, 80] {
            for line in wrap(text, cols) {
                assert!(
                    str_width(&line) <= cols,
                    "line {line:?} is {} cells, over budget {cols}",
                    str_width(&line),
                );
            }
        }
    }

    /// A glyph that cannot fit any line at all is dropped rather than emitted
    /// over budget. Only reachable in a terminal narrower than one glyph, where
    /// there is no correct answer — but the invariant above must still hold.
    #[test]
    fn a_glyph_wider_than_the_whole_line_is_dropped() {
        assert_eq!(wrap("✅", 1), vec![String::new()]);
        for line in wrap("日本語", 1) {
            assert!(str_width(&line) <= 1);
        }
    }

    /// Wrapping must not lose or invent text.
    ///
    /// Column budgets here are all at least as wide as the widest glyph in the
    /// sample; narrower than that, dropping is the documented behaviour.
    #[test]
    fn wrapping_preserves_every_cluster() {
        let text = "the quick brown 狐 jumps over the lazy 犬 with ✅ and \u{26A0}\u{FE0F}";
        for cols in [4usize, 7, 11, 20, 40] {
            let joined: String = wrap(text, cols).join("");
            let strip = |s: &str| s.replace([' ', '\t'], "");
            assert_eq!(
                strip(&joined), strip(text),
                "wrapping at {cols} changed the text",
            );
        }
    }

    #[test]
    fn a_word_wider_than_the_line_is_split_not_dropped() {
        let lines = wrap("日本語日本語日本語", 4);
        assert!(lines.len() > 1, "must split");
        for line in &lines {
            assert!(str_width(line) <= 4);
        }
        let joined: String = lines.join("");
        assert_eq!(joined, "日本語日本語日本語");
    }

    #[test]
    fn zero_columns_is_not_an_infinite_loop() {
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn empty_input_yields_one_empty_line() {
        assert_eq!(wrap("", 10), vec![String::new()]);
    }
}
