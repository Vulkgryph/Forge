// SPDX-License-Identifier: Apache-2.0
//! Enough markdown for agent output, rendered to styled lines.
//!
//! Deliberately not a full implementation. Agents emit a narrow subset —
//! headings, bullets, fenced code, inline code and bold — and a parser scoped to
//! that is small enough to read and to be sure of.
//!
//! Wrapping happens here rather than in the renderer, and it is span-aware: a
//! line's styling can change partway through, so wrapping has to track width
//! across style boundaries. It measures with [`crate::width`], the same module
//! the renderer advances the cursor with, which is what keeps a wrapped line
//! from turning out wider than the screen when it is drawn.

use crate::screen::Style;
use crate::width::str_width;

/// Colours, kept in one place so the palette can be seen at a glance.
mod palette {
    pub const HEADING: u8 = 39;
    pub const BULLET:  u8 = 245;
    pub const CODE:    u8 = 215;
    pub const QUOTE:   u8 = 245;
}

/// A run of text sharing one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text:  String,
    pub style: Style,
}

impl Span {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style }
    }
}

/// One rendered line, ready to draw.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Line {
    pub spans: Vec<Span>,
}

impl Line {
    /// Total width, for tests and for layout that needs to know.
    pub fn width(&self) -> usize {
        self.spans.iter().map(|s| str_width(&s.text)).sum()
    }

    pub fn plain(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

/// Render markdown to lines wrapped at `cols`.
pub fn render(md: &str, cols: usize) -> Vec<Line> {
    if cols == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut in_code = false;
    let mut lang = String::new();
    let mut hl = crate::highlight::State::default();
    let dim = Style::fg(palette::BULLET).dim();

    for raw in md.lines() {
        let trimmed = raw.trim_start();

        // Fences toggle a verbatim region. Inside it nothing is interpreted —
        // code containing `*` or `#` must survive untouched.
        if trimmed.starts_with("```") {
            if in_code {
                out.push(Line { spans: vec![Span::new("  ╰─", dim)] });
                in_code = false;
                lang.clear();
            } else {
                // The fence's language tag, drawn into the top border and used
                // for highlighting.
                lang = trimmed.trim_start_matches('`').trim().to_string();
                let header = if lang.is_empty() {
                    "  ╭─".to_string()
                } else {
                    format!("  ╭─ {lang} ")
                };
                out.push(Line { spans: vec![Span::new(clip(&header, cols), dim)] });
                in_code = true;
                hl = crate::highlight::State::default();
            }
            continue;
        }
        if in_code {
            // Code is not wrapped on words; it is truncated, because rewrapping
            // code changes its meaning.
            let body = clip(raw, cols.saturating_sub(4));
            let mut spans = vec![Span::new("  │ ", dim)];
            for (text, token) in crate::highlight::line(&body, &lang, &mut hl) {
                spans.push(Span::new(text, token.style()));
            }
            out.push(Line { spans });
            continue;
        }

        if trimmed.is_empty() {
            out.push(Line::default());
            continue;
        }

        let (prefix, body, style, hanging) = classify(trimmed);
        let spans = parse_inline(body, style);
        out.extend(wrap_spans(&spans, cols, prefix, hanging));
    }

    out
}

/// Work out what kind of line this is: its literal prefix, the text to parse,
/// the base style, and the indent continuation lines get.
fn classify(line: &str) -> (String, &str, Style, usize) {
    if let Some(rest) = line.strip_prefix("### ") {
        return (String::new(), rest, Style::fg(palette::HEADING).bold(), 0);
    }
    if let Some(rest) = line.strip_prefix("## ") {
        return (String::new(), rest, Style::fg(palette::HEADING).bold(), 0);
    }
    if let Some(rest) = line.strip_prefix("# ") {
        return (String::new(), rest, Style::fg(palette::HEADING).bold(), 0);
    }
    if let Some(rest) = line.strip_prefix("> ") {
        return ("│ ".to_string(), rest, Style::fg(palette::QUOTE).dim(), 2);
    }
    for marker in ["- ", "* "] {
        if let Some(rest) = line.strip_prefix(marker) {
            // Continuation lines indent to sit under the text, not the bullet.
            return ("• ".to_string(), rest, Style::default(), 2);
        }
    }
    (String::new(), line, Style::default(), 0)
}

/// Split inline markup into styled spans.
///
/// Handles `` `code` `` and `**bold**`. Unmatched delimiters are left as
/// literal text rather than swallowing the rest of the line — an agent printing
/// a lone asterisk should see a lone asterisk.
fn parse_inline(text: &str, base: Style) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    let mut plain = String::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    let flush = |plain: &mut String, spans: &mut Vec<Span>| {
        if !plain.is_empty() {
            spans.push(Span::new(std::mem::take(plain), base));
        }
    };

    while i < bytes.len() {
        // Inline code first: its contents are verbatim, so `**` inside it is not
        // emphasis.
        if bytes[i] == b'`' {
            if let Some(end) = text[i + 1..].find('`') {
                flush(&mut plain, &mut spans);
                let inner = &text[i + 1..i + 1 + end];
                spans.push(Span::new(inner, Style::fg(palette::CODE)));
                i += end + 2;
                continue;
            }
        }
        if bytes[i..].starts_with(b"**") {
            if let Some(end) = text[i + 2..].find("**") {
                flush(&mut plain, &mut spans);
                let inner = &text[i + 2..i + 2 + end];
                let mut style = base;
                style.bold = true;
                spans.push(Span::new(inner, style));
                i += end + 4;
                continue;
            }
        }
        // Advance one whole character, not one byte, or multi-byte text breaks.
        let ch = text[i..].chars().next().unwrap();
        plain.push(ch);
        i += ch.len_utf8();
    }
    flush(&mut plain, &mut spans);

    if spans.is_empty() {
        spans.push(Span::new(String::new(), base));
    }
    spans
}

/// Greedily wrap styled spans to `cols`, keeping styles attached to their text.
fn wrap_spans(spans: &[Span], cols: usize, prefix: String, hanging: usize) -> Vec<Line> {
    let prefix_w = str_width(&prefix);
    let mut lines: Vec<Line> = Vec::new();
    let mut current: Vec<Span> = Vec::new();
    let mut width = 0usize;
    let mut first = true;

    // Budget differs between the first line (which carries the bullet) and the
    // rest (which carry the hanging indent).
    let budget = |first: bool| {
        let used = if first { prefix_w } else { hanging };
        cols.saturating_sub(used).max(1)
    };

    /// Emit the line under construction. A free function rather than a closure
    /// because a closure capturing `lines` mutably would lock it for the whole
    /// body, and the tail below needs to read it.
    fn push_line(
        lines:   &mut Vec<Line>,
        current: &mut Vec<Span>,
        first:   &mut bool,
        prefix:  &str,
        hanging: usize,
    ) {
        let mut spans = Vec::new();
        let lead = if *first { prefix.to_string() } else { " ".repeat(hanging) };
        if !lead.is_empty() {
            spans.push(Span::new(lead, Style::fg(palette::BULLET)));
        }
        spans.append(current);
        lines.push(Line { spans });
        *first = false;
    }

    for span in spans {
        for word in tokens(&span.text) {
            let w = str_width(&word);
            let is_space = word.trim().is_empty();

            if width + w > budget(first) {
                if is_space {
                    // Don't carry a space to the start of the next line.
                    push_line(&mut lines, &mut current, &mut first, &prefix, hanging);
                    width = 0;
                    continue;
                }
                if width > 0 {
                    push_line(&mut lines, &mut current, &mut first, &prefix, hanging);
                    width = 0;
                }
            }

            if w > budget(first) {
                // Longer than any line: break it across cluster boundaries.
                for piece in crate::width::wrap(&word, budget(first)) {
                    let pw = str_width(&piece);
                    if width + pw > budget(first) && width > 0 {
                        push_line(&mut lines, &mut current, &mut first, &prefix, hanging);
                        width = 0;
                    }
                    current.push(Span::new(piece, span.style));
                    width += pw;
                }
                continue;
            }

            current.push(Span::new(word, span.style));
            width += w;
        }
    }

    if !current.is_empty() || lines.is_empty() {
        push_line(&mut lines, &mut current, &mut first, &prefix, hanging);
    }
    lines
}

/// Words with their trailing whitespace kept separate.
fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_space = false;
    for ch in s.chars() {
        let is_space = ch == ' ' || ch == '\t';
        if cur.is_empty() {
            in_space = is_space;
        } else if is_space != in_space {
            out.push(std::mem::take(&mut cur));
            in_space = is_space;
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Truncate to `cols` cells without splitting a glyph.
fn clip(s: &str, cols: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    let mut out = String::new();
    let mut w = 0;
    for cluster in s.graphemes(true) {
        let cw = crate::width::cluster_width(cluster);
        if w + cw > cols {
            break;
        }
        out.push_str(cluster);
        w += cw;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::plain).collect()
    }

    /// The invariant that matters most: nothing markdown produces can be wider
    /// than the screen it was wrapped for. A violation here is exactly what made
    /// the old TUI overlap messages.
    #[test]
    fn no_rendered_line_exceeds_the_column_budget() {
        let md = "\
# A heading that is quite long and will certainly need to wrap somewhere
Some **bold text** and `inline_code` mixed with 日本語のテキスト and \
a_very_long_unbreakable_identifier_that_cannot_fit and \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}.
- a bullet item that also runs on for a while and must wrap under itself
> a quoted line that is long enough to need wrapping as well
```
some code that is quite wide and should be clipped rather than wrapped
```
";
        for cols in [8usize, 12, 20, 40, 80] {
            for line in render(md, cols) {
                assert!(
                    line.width() <= cols,
                    "line {:?} is {} cells, budget {cols}",
                    line.plain(), line.width(),
                );
            }
        }
    }

    #[test]
    fn headings_are_bold() {
        let lines = render("# Title", 40);
        assert_eq!(plain(&lines), vec!["Title"]);
        assert!(lines[0].spans.iter().any(|s| s.style.bold));
    }

    #[test]
    fn bullets_get_a_marker_and_hang_under_it() {
        let lines = render("- first item wrapping onto another line here", 20);
        assert!(lines[0].plain().starts_with("• "), "got {:?}", lines[0].plain());
        assert!(lines.len() > 1, "should wrap");
        assert!(lines[1].plain().starts_with("  "), "continuation indents");
    }

    /// Wrapping splits spans at word boundaries, so a multi-word code span
    /// arrives as several spans sharing one style rather than a single span.
    /// What matters is that exactly the code text carries the code style.
    #[test]
    fn inline_code_is_styled_separately() {
        let lines = render("run `cargo test` now", 40);
        let styled: String = lines[0].spans.iter()
            .filter(|s| s.style.fg == Some(palette::CODE))
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(styled, "cargo test");
        // And the surrounding words did not pick it up.
        let unstyled: String = lines[0].spans.iter()
            .filter(|s| s.style.fg != Some(palette::CODE))
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(unstyled, "run  now");
    }

    #[test]
    fn bold_is_styled_separately() {
        let lines = render("say **this** loudly", 40);
        let bold: Vec<_> = lines[0].spans.iter()
            .filter(|s| s.style.bold)
            .map(|s| s.text.clone())
            .collect();
        assert_eq!(bold, vec!["this"]);
    }

    /// Markup inside a code span is text, not markup.
    #[test]
    fn markup_inside_inline_code_is_literal() {
        let lines = render("literally `**not bold**` here", 40);
        assert!(lines[0].plain().contains("**not bold**"));
        assert!(!lines[0].spans.iter().any(|s| s.style.bold));
    }

    /// An unmatched delimiter must not eat the line.
    #[test]
    fn unmatched_delimiters_stay_literal() {
        assert_eq!(plain(&render("a * lone asterisk", 40)), vec!["a * lone asterisk"]);
        assert_eq!(plain(&render("unclosed **bold", 40)), vec!["unclosed **bold"]);
        assert_eq!(plain(&render("unclosed `code", 40)), vec!["unclosed `code"]);
    }

    /// A fenced block is drawn in a box, as the TypeScript client drew it: a
    /// top border carrying the language, one bordered row per line, a bottom.
    #[test]
    fn fenced_code_is_not_interpreted() {
        let md = "```\nlet x = **2**; # not a heading\n```";
        let lines = render(md, 60);
        let text = plain(&lines);
        assert_eq!(text.len(), 3, "top border, the code, bottom border: {text:?}");
        assert!(text[0].contains('╭'), "opening border: {text:?}");
        assert!(text[1].contains("let x = **2**; # not a heading"), "verbatim");
        assert!(text[2].contains('╰'), "closing border");
        // Markup inside a fence is text, not markup.
        assert!(!lines[1].spans.iter().any(|s| s.style.bold));
    }

    /// The language goes in the top border and drives highlighting.
    #[test]
    fn a_fence_language_is_labelled_and_highlighted() {
        let lines = render("```rust\nlet x = 1;\n```", 60);
        let text = plain(&lines);
        assert!(text[0].contains("rust"), "labelled: {text:?}");

        // `let` is a keyword, so it must not be the default style.
        let code = &lines[1];
        let keyword = code
            .spans
            .iter()
            .find(|s| s.text.contains("let"))
            .expect("a run with let");
        assert_ne!(keyword.style, Style::default(), "highlighted: {:?}", code.spans);
    }

    /// An unknown language still gets the box, just no colouring.
    #[test]
    fn an_unknown_fence_language_is_still_boxed() {
        let lines = render("```brainfuck\n+++>\n```", 60);
        let text = plain(&lines);
        assert!(text[0].contains("brainfuck"));
        assert!(text[1].contains("+++>"));
        assert_eq!(text.len(), 3);
    }

    #[test]
    fn code_is_clipped_not_wrapped() {
        // Rewrapping code would change what it means, so long lines are cut.
        let md = "```\nthis code line is definitely much wider than the budget\n```";
        let lines = render(md, 20);
        assert_eq!(lines.len(), 3, "border, one code row, border");
        for line in &lines {
            assert!(line.width() <= 20, "{:?} is {} cells", line.plain(), line.width());
        }
    }

    /// A block comment spanning lines must stay grey throughout — the lexer state
    /// has to carry between rows of the same block.
    #[test]
    fn a_multiline_comment_stays_grey_across_rows() {
        let lines = render("```rust\n/* opening\nstill inside\n*/ let x = 1;\n```", 70);
        let comment_style = crate::highlight::Token::Comment.style();
        // Row 2 is the middle of the comment; every run after the border is grey.
        let middle = &lines[2];
        assert!(
            middle.spans.iter().skip(1).all(|s| s.style == comment_style),
            "still inside the comment: {:?}", middle.spans,
        );
    }

    #[test]
    fn blank_lines_are_preserved() {
        let lines = render("a\n\nb", 40);
        assert_eq!(plain(&lines), vec!["a", "", "b"]);
    }

    #[test]
    fn wide_text_wraps_without_splitting_glyphs() {
        let lines = render("日本語日本語日本語日本語", 8);
        for line in &lines {
            assert!(line.width() <= 8);
            // No replacement characters from a torn multi-byte sequence.
            assert!(!line.plain().contains('\u{FFFD}'));
        }
        let joined: String = plain(&lines).concat();
        assert_eq!(joined, "日本語日本語日本語日本語");
    }

    #[test]
    fn zero_columns_renders_nothing() {
        assert!(render("# anything", 0).is_empty());
    }

    #[test]
    fn multibyte_inline_parsing_does_not_panic_or_corrupt() {
        // Byte-indexed scanning has to advance by character, not byte.
        let lines = render("日本 **語** テキスト `コード` 終", 40);
        assert!(lines[0].plain().contains('語'));
        assert!(!lines[0].plain().contains('\u{FFFD}'));
    }
}
