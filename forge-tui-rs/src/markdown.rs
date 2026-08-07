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
    /// Link labels. A light blue rather than the terminal's own, which is the
    /// dark indigo that was unreadable on black.
    pub const LINK:    u8 = 117;
    /// Table borders.
    pub const RULE:    u8 = 245;
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
    let dim = Style::fg(palette::BULLET);

    let lines: Vec<&str> = md.lines().collect();
    let mut idx = 0usize;
    while idx < lines.len() {
        let raw = lines[idx];
        idx += 1;
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

        // A pipe row followed by a `|---|` rule is a table. Consumed as a block,
        // since a table cannot be rendered one line at a time.
        if trimmed.starts_with('|') && idx < lines.len() && is_table_rule(lines[idx].trim()) {
            let header = split_row(trimmed);
            idx += 1; // the rule
            let mut rows = Vec::new();
            while idx < lines.len() && lines[idx].trim_start().starts_with('|') {
                rows.push(split_row(lines[idx].trim_start()));
                idx += 1;
            }
            out.extend(table_lines(&header, &rows, cols));
            continue;
        }

        // A horizontal rule, drawn as one rather than left as three hyphens.
        if trimmed == "---" || trimmed == "***" || trimmed == "___" {
            let width = cols.saturating_sub(2).min(40);
            out.push(Line {
                spans: vec![Span::new(format!("  {}", "─".repeat(width)), dim)],
            });
            continue;
        }

        let (prefix, body, style, hanging) = classify(trimmed);
        let spans = parse_inline(body, style);
        out.extend(wrap_spans(&spans, cols, prefix, hanging));
    }

    out
}

/// `|---|:--:|` — the row that makes the line above it a header.
fn is_table_rule(line: &str) -> bool {
    line.starts_with('|')
        && line.contains('-')
        && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Cells of one pipe row, with the empty edges either side dropped.
///
/// Each cell is reduced to its plain text: `**`forge-agent/`**` is measured and
/// drawn as `forge-agent/`, not with its markers showing. The TypeScript client
/// did the same — cells went through `partsToPlain` — and it has to be the plain
/// text, since the column widths are measured from what is actually drawn.
fn split_row(line: &str) -> Vec<String> {
    let inner = line.trim().trim_start_matches('|').trim_end_matches('|');
    inner.split('|').map(|c| inline_plain(c.trim())).collect()
}

/// Inline markdown with the syntax resolved away, keeping only the text.
fn inline_plain(text: &str) -> String {
    parse_inline(text, Style::default())
        .iter()
        .map(|s| s.text.as_str())
        .collect()
}

/// One cell, padded to `width` or truncated into it with an ellipsis.
///
/// Deliberately not this module's own `clip`, which truncates silently — right for
/// a line of code, wrong for a table cell, where the reader has to be able to see
/// that something was cut. The TypeScript client's `truncateCell` marked it too.
fn cell(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let clipped = crate::widgets::clip(text, width);
    let pad = width.saturating_sub(str_width(&clipped));
    format!("{clipped}{}", " ".repeat(pad))
}

/// Fit the columns into the space available, shrinking the widest first so short
/// labels stay readable — the rule the TypeScript client used.
fn fit_widths(raw: &[usize], max_total: usize) -> Vec<usize> {
    let mut widths: Vec<usize> = raw.iter().map(|w| (*w).max(3)).collect();
    let total = |w: &Vec<usize>| w.iter().sum::<usize>() + w.len() * 3 + 1;
    if total(&widths) <= max_total {
        return widths;
    }
    const FLOOR: usize = 4;
    while total(&widths) > max_total {
        let widest = (0..widths.len()).max_by_key(|i| widths[*i]).unwrap_or(0);
        if widths[widest] <= FLOOR {
            // Every column is at the floor; take one from each and stop when it
            // cannot shrink further, rather than looping forever.
            let before = total(&widths);
            for w in widths.iter_mut() {
                if *w > 2 {
                    *w -= 1;
                }
            }
            if total(&widths) >= before {
                break;
            }
        } else {
            widths[widest] -= 1;
        }
    }
    widths
}

/// A table as lines: a framed grid, or a stacked key/value list when a grid
/// cannot fit.
///
/// Cells hold plain text. The TypeScript client measured and drew them through
/// `partsToPlain`, so emphasis inside a cell was never styled there either, and
/// styling it here would mean the frame no longer lines up with what is measured.
fn table_lines(header: &[String], rows: &[Vec<String>], cols: usize) -> Vec<Line> {
    if header.is_empty() {
        return Vec::new();
    }
    let edge = Style::fg(palette::RULE);
    // The TypeScript client budgeted `max(24, columns - 2)` and then clipped every
    // row to it. Two problems with taking that literally: the budget leaves out
    // the two-space indent the rows actually carry, so a "fitting" table came out
    // two columns too wide; and the floor of 24 is wider than the window itself
    // below 24 columns. Both were invisible there because the clip hid them —
    // as a cut border. Budget honestly instead, and never exceed the window.
    let indent = 2usize;
    let max_total = cols.saturating_sub(indent).max(24).min(cols.max(1));

    let raw: Vec<usize> = (0..header.len())
        .map(|c| {
            std::iter::once(header[c].as_str())
                .chain(rows.iter().map(|r| r.get(c).map(String::as_str).unwrap_or("")))
                .map(str_width)
                .max()
                .unwrap_or(3)
                .max(3)
        })
        .collect();
    let widths = fit_widths(&raw, max_total.saturating_sub(indent));
    let frame_width = widths.iter().sum::<usize>() + widths.len() * 3 + 1 + indent;

    // Too many columns, or too narrow even at the floor: stack each row as
    // `label: value` instead of emitting rows wider than the terminal.
    if frame_width > max_total || widths.len() > 6 {
        let mut out = Vec::new();
        for (ri, row) in rows.iter().enumerate() {
            if ri > 0 {
                out.push(Line::default());
            }
            for (ci, value) in row.iter().enumerate() {
                let label = header
                    .get(ci)
                    .filter(|h| !h.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("col{}", ci + 1));
                out.push(Line {
                    spans: vec![Span::new(
                        crate::widgets::clip(&format!("  {label}: {value}"), cols),
                        Style::default(),
                    )],
                });
            }
        }
        return out;
    }

    let rule = |left: &str, mid: &str, right: &str| {
        let bars: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
        Line {
            spans: vec![Span::new(clip(&format!("  {left}{}{right}", bars.join(mid)), cols), edge)],
        }
    };
    // A last guard on the total: a row must never be wider than the window, which
    // is what left a screen-high blank gap under a reply in the ink client.
    let row_line = |cells: &[String], style: Style| {
        let mut spans = vec![Span::new("  │", edge)];
        let mut used = 3usize;
        for (ci, w) in widths.iter().enumerate() {
            let text = cells.get(ci).map(String::as_str).unwrap_or("");
            let body = format!(" {} ", cell(text, *w));
            if used + str_width(&body) + 1 > cols {
                break;
            }
            used += str_width(&body) + 1;
            spans.push(Span::new(body, style));
            spans.push(Span::new("│", edge));
        }
        Line { spans }
    };

    let mut out = vec![rule("┌", "┬", "┐")];
    out.push(row_line(header, Style::default().bold()));
    out.push(rule("├", "┼", "┤"));
    for row in rows {
        out.push(row_line(row, Style::default()));
    }
    out.push(rule("└", "┴", "┘"));
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
        return ("│ ".to_string(), rest, Style::fg(palette::QUOTE), 2);
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
                // Parsed rather than taken raw, so `**`code`**` is bold code and
                // not bold backticks. ink's parser nested its tokens and flattened
                // them recursively; taking the inner text as written left the
                // markers of anything inside it on screen.
                spans.extend(parse_nested(inner, style));
                i += end + 4;
                continue;
            }
        }
        // After `**`, so bold is not mistaken for two italics.
        if bytes[i..].starts_with(b"~~") {
            if let Some(end) = text[i + 2..].find("~~") {
                flush(&mut plain, &mut spans);
                let inner = &text[i + 2..i + 2 + end];
                spans.extend(parse_nested(inner, base.strike()));
                i += end + 4;
                continue;
            }
        }
        if bytes[i] == b'*' || bytes[i] == b'_' {
            let marker = bytes[i] as char;
            // Only when it closes on the same line and wraps something: `a * b`
            // and snake_case identifiers are not emphasis.
            if let Some(end) = text[i + 1..].find(marker) {
                let inner = &text[i + 1..i + 1 + end];
                let closes_a_word = !inner.is_empty()
                    && !inner.starts_with(char::is_whitespace)
                    && !inner.ends_with(char::is_whitespace);
                let bare_underscore = marker == '_'
                    && (i > 0 && text[..i].ends_with(|c: char| c.is_alphanumeric()));
                if closes_a_word && !bare_underscore {
                    flush(&mut plain, &mut spans);
                    spans.extend(parse_nested(inner, base.italic()));
                    i += end + 2;
                    continue;
                }
            }
        }
        // `[label](href)`. The label is styled and the target follows it dimmed,
        // as ink drew it — a terminal cannot make the label itself clickable.
        if bytes[i] == b'[' {
            if let Some(close) = text[i..].find("](") {
                let rest = &text[i + close + 2..];
                if let Some(paren) = rest.find(')') {
                    let label = &text[i + 1..i + close];
                    let href = &rest[..paren];
                    if !label.is_empty() && !href.contains(char::is_whitespace) {
                        flush(&mut plain, &mut spans);
                        spans.push(Span::new(
                            label,
                            Style::fg(palette::LINK).underline(),
                        ));
                        if href != label {
                            spans.push(Span::new(
                                format!(" ({href})"),
                                Style::fg(palette::RULE),
                            ));
                        }
                        i += close + 2 + paren + 1;
                        continue;
                    }
                }
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

/// Inline content inside an emphasis run.
///
/// Separate from `parse_inline` only to keep the empty case out of the recursion:
/// `parse_inline` returns one empty span when it finds nothing, which would add a
/// stray span for every `****`.
fn parse_nested(text: &str, style: Style) -> Vec<Span> {
    if text.is_empty() {
        return Vec::new();
    }
    parse_inline(text, style)
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
    // ── Emphasis, links, rules and tables ────────────────────────────────

    fn spans_of(md: &str, cols: usize) -> Vec<Span> {
        render(md, cols).into_iter().flat_map(|l| l.spans).collect()
    }

    /// `*x*` and `_x_` are emphasis; the markers go and the text is italic.
    #[test]
    fn italics_are_styled_and_their_markers_removed() {
        let spans = spans_of("*soft* and _also_", 60);
        let soft = spans.iter().find(|s| s.text == "soft").expect("the emphasised run");
        assert!(soft.style.italic, "not italic: {:?}", soft.style);
        assert!(spans.iter().any(|s| s.text == "also" && s.style.italic));
        let plain: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!plain.contains('*') && !plain.contains('_'), "markers left: {plain:?}");
    }

    /// An underscore inside a word is not emphasis — `snake_case_word` must not
    /// come out italicised with its underscores eaten.
    #[test]
    fn underscores_inside_a_word_are_left_alone() {
        let plain: String = spans_of("snake_case_word", 60).iter()
            .map(|s| s.text.as_str()).collect();
        assert_eq!(plain, "snake_case_word");
    }

    #[test]
    fn strikethrough_is_styled() {
        let spans = spans_of("~~gone~~", 60);
        let gone = spans.iter().find(|s| s.text == "gone").expect("the struck run");
        assert!(gone.style.strike, "not struck: {:?}", gone.style);
    }

    /// A link shows its label, then its target dimly — a terminal cannot make the
    /// label itself clickable, which is how ink drew it too.
    #[test]
    fn a_link_shows_its_label_then_its_target() {
        let spans = spans_of("see [the docs](https://example.com/x) here", 80);
        // Wrapping rebuilds spans word by word, so match on style rather than on
        // the label arriving as one span.
        let label: Vec<&Span> = spans.iter()
            .filter(|s| s.style.underline && s.style.fg == Some(palette::LINK))
            .collect();
        assert!(!label.is_empty(), "no styled label in {spans:?}");
        let text: String = label.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("docs"), "the label is styled: {text:?}");
        assert!(
            spans.iter().any(|s| s.text.contains("https://example.com/x")),
            "the target is shown: {spans:?}",
        );
        let plain: String = spans.iter().map(|s| s.text.as_str()).collect();
        assert!(!plain.contains("]("), "the syntax is gone: {plain:?}");
    }

    /// A bare `[not a link]` is left as written.
    #[test]
    fn brackets_that_are_not_a_link_are_untouched() {
        let plain: String = spans_of("[not a link] here", 60).iter()
            .map(|s| s.text.as_str()).collect();
        assert_eq!(plain, "[not a link] here");
    }

    #[test]
    fn a_horizontal_rule_is_drawn_as_a_rule() {
        let lines = render("---", 60);
        assert_eq!(lines.len(), 1);
        let text = lines[0].plain();
        assert!(text.trim().chars().all(|c| c == '─'), "not a rule: {text:?}");
        assert!(text.starts_with("  "), "indented like the other blocks");
    }

    const TABLE: &str = "| lang | files |\n|------|------:|\n| rust | 42 |\n| ts | 7 |";

    /// A table was passed through as raw pipes before this; it is a framed grid
    /// now, with every row the same width so the borders line up.
    #[test]
    fn a_table_is_framed_and_aligned() {
        let lines = render(TABLE, 60);
        let text: Vec<String> = lines.iter().map(|l| l.plain()).collect();
        assert!(text[0].contains('┌') && text[0].contains('┬'), "top: {text:?}");
        assert!(text[2].contains('├') && text[2].contains('┼'), "header rule: {text:?}");
        assert!(text[text.len() - 1].contains('└'), "bottom: {text:?}");
        let widths: Vec<usize> = lines.iter().map(|l| l.width()).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "ragged rows: {widths:?}");
        let all = text.join("\n");
        for want in ["lang", "files", "rust", "42", "ts", "7"] {
            assert!(all.contains(want), "lost {want:?} in {all:?}");
        }
    }

    /// A cut cell says so. Silent truncation reads as if that were the whole value.
    #[test]
    fn a_truncated_cell_is_marked() {
        let md = "| description | v |\n|---|---|\n| a description far too long for the column | 1 |";
        let text: Vec<String> = render(md, 34).iter().map(|l| l.plain()).collect();
        // The header also says "description"; the body row is the one with "far".
        let body = text.iter().find(|l| l.contains("far")).expect("the body row");
        assert!(body.contains('…'), "no ellipsis in {body:?}");
    }

    /// A cell's markdown is resolved, not shown. Measuring the raw text would also
    /// make the columns wider than what is drawn.
    #[test]
    fn table_cells_show_text_not_markdown() {
        let md = "| Project | Stack |\n|---|---|\n| **`forge-agent/`** | Rust |";
        let text: Vec<String> = render(md, 60).iter().map(|l| l.plain()).collect();
        let all = text.join("\n");
        assert!(all.contains("forge-agent/"), "the cell's text is there: {all:?}");
        assert!(!all.contains("**"), "bold markers left in a cell: {all:?}");
        assert!(!all.contains('`'), "code markers left in a cell: {all:?}");
    }

    /// Narrow the window and the cells are truncated into it; no row may be wider
    /// than the terminal, which is what made ink leave a blank gap under a reply.
    #[test]
    fn a_table_narrows_without_overflowing() {
        let wide = "| description | value |\n|---|---|\n| a very long description indeed that will not fit | 12345678 |";
        for cols in [80usize, 40, 30, 24, 16] {
            for line in render(wide, cols) {
                assert!(
                    line.width() <= cols,
                    "at {cols}: {:?} is {} wide",
                    line.plain(), line.width(),
                );
            }
        }
    }

    /// More columns than a grid can carry: stacked as `label: value` rather than
    /// emitting rows wider than the window.
    #[test]
    fn too_many_columns_stack_instead_of_overflowing() {
        let many = "| a | b | c | d | e | f | g |\n|---|---|---|---|---|---|---|\n| 1 | 2 | 3 | 4 | 5 | 6 | 7 |";
        let lines = render(many, 60);
        let text: Vec<String> = lines.iter().map(|l| l.plain()).collect();
        assert!(text.iter().all(|l| !l.contains('┌')), "should not be framed: {text:?}");
        assert!(text.iter().any(|l| l.contains("a: 1")), "stacked as label: value: {text:?}");
        for line in &lines {
            assert!(line.width() <= 60, "{:?} overflows", line.plain());
        }
    }
}
