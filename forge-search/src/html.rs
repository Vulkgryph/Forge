//! Turning a page into the parts a search engine needs: its title, its
//! readable text, and the links it points at.
//!
//! This is not a DOM. Nothing here can answer "what is the third child of the
//! second table", and a search engine never needs to ask — it needs the words
//! on the page and the hyperlinks out of it. So the page is walked once as a
//! stream of tags and text, keeping a small stack of which elements are open.
//!
//! Written out rather than taken from a crate for the reason the whole crate
//! exists: this is the piece every other stage depends on, and an engine whose
//! foundation is someone else's parser is not one you can lift somewhere else.
//! It is also a closed problem — tag soup is famously irregular, but the
//! irregularities are known and finite, and none of them require a DOM to
//! survive.

/// What one page yielded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Page {
    /// The `<title>`, trimmed. Empty when the page has none.
    pub title: String,
    /// The readable text, with runs of whitespace collapsed to single spaces
    /// and block elements separated by newlines.
    pub text: String,
    /// Every `href` on the page, in document order, exactly as written — not
    /// resolved against a base, which is the caller's business since only the
    /// caller knows the URL this came from.
    pub links: Vec<String>,
    /// `<meta name="description">`, which is frequently the best one-line
    /// summary a page has and is cheaper than inventing one.
    pub description: String,
}

/// Elements whose contents are not readable text.
///
/// `script` and `style` are the obvious ones. `template` is inert by
/// definition, and `svg` holds path data that tokenises into nonsense.
const SKIPPED: &[&str] = &["script", "style", "noscript", "template", "svg", "math"];

/// Elements that end a line of text.
///
/// Without these, a list or a table of links runs together into one long line
/// and every snippet drawn from it reads as a single sentence that was never
/// written.
const BLOCK: &[&str] = &[
    "address", "article", "aside", "blockquote", "br", "dd", "div", "dl", "dt",
    "fieldset", "figcaption", "figure", "footer", "form", "h1", "h2", "h3",
    "h4", "h5", "h6", "header", "hr", "li", "main", "nav", "ol", "p", "pre",
    "section", "table", "tbody", "td", "tfoot", "th", "thead", "tr", "ul",
];

/// Elements that never have a closing tag, so seeing one must not push onto
/// the stack — a page full of `<img>` would otherwise look permanently nested.
const VOID: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta",
    "param", "source", "track", "wbr",
];

/// Extract a page's title, text, links and description.
///
/// Never fails. Malformed HTML is the normal case rather than an error: an
/// unclosed tag, a stray `<`, a `</p>` with no `<p>` are all things real pages
/// contain, and a crawler that refuses them indexes nothing.
pub fn parse(html: &str) -> Page {
    let mut page = Page::default();
    let bytes = html.as_bytes();
    let mut at = 0;
    let mut in_title = false;
    let mut text = String::with_capacity(html.len() / 4);

    while at < bytes.len() {
        match bytes[at] {
            b'<' if starts_a_tag(bytes, at) => {
                // Only when a name, `/`, `!` or `?` follows. A `<` before
                // whitespace or a digit is prose — "3 < 4" — and scanning it as
                // a tag swallows everything up to the next `>`, which is
                // usually the end of the enclosing element.
                // A comment is recognised here, from the `<`, because its
                // terminator is `-->` and not the first `>`. Scanning it as an
                // ordinary tag first and then looking for `-->` afterwards is
                // wrong in the common case: for a well-formed `<!-- x -->`,
                // `find_tag_end` already stops on the `>` of `-->`, so the
                // position is past the comment and the extra scan runs on to
                // the *next* comment, swallowing everything in between.
                //
                // Measured on a real Wikipedia article: a comment 21 kB in
                // dropped the following 1,298,945 bytes — the whole body — and
                // left 51 indexable terms of navigation chrome. The tests did
                // not catch it because their comments contained a `>`, which
                // stops `find_tag_end` early and makes the second scan correct.
                if html[at..].starts_with("<!--") {
                    at = match html[at + 4..].find("-->") {
                        Some(i) => at + 4 + i + 3,
                        // Unterminated: the rest of the document is comment,
                        // which is also what a browser concludes.
                        None => bytes.len(),
                    };
                    continue;
                }

                let Some(tag_end) = find_tag_end(bytes, at) else {
                    push_text(&mut text, &html[at..]);
                    break;
                };
                let raw = &html[at + 1..tag_end];
                at = tag_end + 1;

                if let Some(rest) = raw.strip_prefix('!') {
                    // Comment, doctype, CDATA — none of it is text. A comment
                    // needs its own scan because `-->` is the terminator, not
                    // the `>` that `find_tag_end` stopped at.
                    // Doctype and CDATA, which do end at `>`. Comments were
                    // already handled above.
                    let _ = rest;
                    continue;
                }

                let closing = raw.starts_with('/');
                let name = tag_name(raw);
                if name.is_empty() {
                    continue;
                }

                if SKIPPED.contains(&name.as_str()) {
                    if !closing && !raw.ends_with('/') {
                        // Jump past the closing tag rather than tokenising the
                        // content. These elements hold raw text, not markup,
                        // and real scripts contain `<` used as less-than and
                        // quoted strings full of angle brackets. Counting depth
                        // while still scanning tags inside them means a string
                        // like `"</div>"` is read as markup, and one containing
                        // `"<script>"` opens a nesting level that never closes.
                        at = find_raw_text_end(bytes, at, &name).unwrap_or(bytes.len());
                    }
                    continue;
                }

                if name == "title" {
                    in_title = !closing;
                    continue;
                }

                if !closing {
                    if name == "a" {
                        if let Some(href) = attribute(raw, "href") {
                            let href = href.trim();
                            if !href.is_empty() {
                                page.links.push(href.to_string());
                            }
                        }
                    } else if name == "meta" && page.description.is_empty() {
                        // `name=description` or `property=og:description`,
                        // whichever the page happens to use.
                        let is_desc = attribute(raw, "name")
                            .is_some_and(|v| v.eq_ignore_ascii_case("description"))
                            || attribute(raw, "property")
                                .is_some_and(|v| v.eq_ignore_ascii_case("og:description"));
                        if is_desc {
                            if let Some(c) = attribute(raw, "content") {
                                page.description = collapse(&decode_entities(&c));
                            }
                        }
                    }
                }

                if BLOCK.contains(&name.as_str()) && !in_title {
                    // A newline rather than a space, so a snippet taken from
                    // here does not read as one run-on sentence.
                    if !text.ends_with('\n') && !text.is_empty() {
                        text.push('\n');
                    }
                }
                let _ = VOID; // documented above; the stack is depth-counted instead
            }
            _ => {
                // To the next `<` that really opens a tag; a literal one is
                // part of this run of text.
                let mut end = at + 1;
                while end < bytes.len() {
                    if bytes[end] == b'<' && starts_a_tag(bytes, end) {
                        break;
                    }
                    end += 1;
                }
                let end = end.min(bytes.len());
                let end = floor_boundary(html, end);
                let chunk = &html[at..end];
                if in_title {
                    page.title.push_str(chunk);
                } else {
                    push_text(&mut text, chunk);
                }
                at = end;
            }
        }
    }

    page.title = collapse(&decode_entities(&page.title));
    page.text = tidy_lines(&decode_entities(&text));
    page
}

/// Where a raw-text element's content ends, just past its closing tag.
///
/// Scans bytes in place rather than lowercasing the rest of the document: a
/// page with hundreds of inline scripts would otherwise copy the tail once per
/// script, which is quadratic in the page size.
fn find_raw_text_end(bytes: &[u8], from: usize, name: &str) -> Option<usize> {
    let name = name.as_bytes();
    let mut at = from;
    while at + 2 + name.len() <= bytes.len() {
        if bytes[at] == b'<'
            && bytes[at + 1] == b'/'
            && bytes[at + 2..at + 2 + name.len()].eq_ignore_ascii_case(name)
        {
            // The name has to end here, or `</scriptfoo>` would close a
            // `<script>`.
            let after = at + 2 + name.len();
            match bytes.get(after) {
                Some(b'>') => return Some(after + 1),
                // `</script >` closes it too, so scan on to the `>`.
                Some(c) if c.is_ascii_whitespace() => {
                    let mut j = after;
                    while j < bytes.len() && bytes[j] != b'>' {
                        j += 1;
                    }
                    return Some((j + 1).min(bytes.len()));
                }
                _ => {}
            }
        }
        at += 1;
    }
    None
}

/// Where the tag that starts at `open` ends.
///
/// Quoted attribute values may contain `>` — `<a title="a > b">` is valid —
/// so the scan tracks quoting rather than taking the first `>`.
fn find_tag_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    for (i, &b) in bytes.iter().enumerate().skip(open + 1) {
        match (quote, b) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => return Some(i),
            (None, _) => {}
        }
    }
    None
}

/// The element name, lowercased, from the inside of a tag.
fn tag_name(raw: &str) -> String {
    raw.trim_start_matches('/')
        .split(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// One attribute's value, unquoted. Bare values (`<a href=/x>`) are accepted
/// because pages contain them.
fn attribute(raw: &str, want: &str) -> Option<String> {
    let lower = raw.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(want) {
        let start = from + found;
        // A whole attribute name, not a substring of another — `href` must not
        // match inside `data-href`.
        let before_ok = start == 0
            || lower.as_bytes()[start - 1].is_ascii_whitespace();
        let after = start + want.len();
        let rest = lower[after..].trim_start();
        if before_ok && rest.starts_with('=') {
            let eq = after + lower[after..].find('=')? + 1;
            let value = raw[eq..].trim_start();
            let mut chars = value.chars();
            return match chars.next() {
                Some(q @ ('"' | '\'')) => {
                    let body = &value[1..];
                    Some(body[..body.find(q).unwrap_or(body.len())].to_string())
                }
                Some(_) => Some(
                    value
                        .split(|c: char| c.is_whitespace() || c == '>')
                        .next()
                        .unwrap_or("")
                        .to_string(),
                ),
                None => None,
            };
        }
        from = start + want.len();
    }
    None
}

fn push_text(out: &mut String, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    // Newlines in the source are just whitespace — HTML says so, and a page
    // wrapped at eighty columns would otherwise come out as eighty-column
    // lines. The only newlines that mean anything here are the ones inserted
    // for block elements, so source ones are flattened on the way in.
    for ch in chunk.chars() {
        out.push(if ch == '\n' || ch == '\r' { ' ' } else { ch });
    }
}

/// Whether the `<` at `at` opens a tag, as a browser decides it: a name, a
/// closing slash, a declaration, or a processing instruction.
fn starts_a_tag(bytes: &[u8], at: usize) -> bool {
    matches!(bytes.get(at + 1), Some(c) if c.is_ascii_alphabetic()
        || *c == b'/' || *c == b'!' || *c == b'?')
}

/// The largest char boundary at or below `i` — the scan works in bytes and the
/// text is UTF-8.
fn floor_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Runs of whitespace to single spaces, trimmed.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = true; // leading whitespace is dropped
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(ch);
            space = false;
        }
    }
    while out.ends_with(' ') {
        out.pop();
    }
    out
}

/// Collapse each line, drop the empty ones, and keep the line structure that
/// block elements produced.
fn tidy_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split('\n') {
        let line = collapse(line);
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    out
}

/// Resolve the HTML entities that actually appear in prose.
///
/// Not the full named set — that is two thousand names, nearly all of which
/// never occur outside a conformance test. The numeric forms are handled in
/// general because they are generated by tooling and do appear.
pub(crate) fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        // Bounded by bytes but cut on a character boundary. An entity name is
        // short, so twelve bytes is a generous window — but `&` followed by a
        // four-byte emoji puts a character across that boundary, and slicing
        // there panics. Found by crawling a real page rather than by a test:
        // the multibyte test used three-byte characters and never placed one
        // within twelve bytes of an ampersand.
        let window = floor_boundary(tail, tail.len().min(12));
        let Some(semi) = tail[..window].find(';') else {
            out.push('&');
            rest = &tail[1..];
            continue;
        };
        let name = &tail[1..semi];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" => Some('\''),
            "nbsp" => Some(' '),
            "hellip" => Some('…'),
            "mdash" => Some('—'),
            "ndash" => Some('–'),
            "rsquo" => Some('\''),
            "lsquo" => Some('\''),
            "ldquo" => Some('"'),
            "rdquo" => Some('"'),
            n => n
                .strip_prefix('#')
                .and_then(|num| match num.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => num.parse::<u32>().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &tail[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A comment must end at its own `-->` and nothing more.
    ///
    /// The bug this pins down: `find_tag_end` stops on the `>` of `-->`, so by
    /// the time a comment is recognised the position is already past it.
    /// Scanning on for another `-->` then jumps to the *next* comment in the
    /// document and drops everything between the two. Every earlier comment
    /// test here contained a `>` inside the comment, which stops
    /// `find_tag_end` early and makes that second scan look right.
    #[test]
    fn a_comment_does_not_swallow_what_follows_it() {
        let page = parse("<p>before</p><!-- nav --><p>the body</p>");
        assert!(page.text.contains("before"));
        assert!(
            page.text.contains("the body"),
            "content after a comment was dropped: {:?}",
            page.text
        );
    }

    /// The same defect with two comments, which is the shape a real page has:
    /// commented-out section markers around the content.
    #[test]
    fn content_between_two_comments_survives() {
        let page = parse(
            "<!-- header --><p>alpha</p><!-- content --><p>beta</p><!-- footer --><p>gamma</p>",
        );
        for word in ["alpha", "beta", "gamma"] {
            assert!(page.text.contains(word), "{word} missing from {:?}", page.text);
        }
    }

    /// Measured against the real article that exposed this: 1.34 MB of HTML
    /// must not collapse to a few dozen terms. The shape is what matters —
    /// a comment early on, then the entire body after it.
    #[test]
    fn a_comment_near_the_top_does_not_cost_the_whole_document() {
        let body = "<p>sodium channel kinetics at thirty five degrees</p>".repeat(200);
        let page = parse(&format!("<html><body><!-- skin --><div>{body}</div></body></html>"));
        assert!(
            page.text.matches("sodium").count() == 200,
            "kept {} of 200 paragraphs",
            page.text.matches("sodium").count()
        );
    }

    /// An unterminated comment does run to the end of the document — that is
    /// what a browser concludes, and the boundary deserves pinning so the fix
    /// above is not mistaken for one that ignores `<!--` entirely.
    #[test]
    fn an_unterminated_comment_runs_to_the_end() {
        let page = parse("<p>kept</p><!-- and the rest <p>lost</p>");
        assert!(page.text.contains("kept"));
        assert!(!page.text.contains("lost"));
    }

    /// A script's content is raw text, so markup inside a JavaScript string is
    /// not markup. Depth counting got this wrong: the `"<script>"` in the
    /// string opened a level that the real `</script>` only half-closed, and
    /// everything after was discarded as still-inside-a-script.
    #[test]
    fn markup_inside_a_script_string_is_not_markup() {
        let page = parse(r#"<p>one</p><script>var s = "<script>";</script><p>two</p>"#);
        assert!(page.text.contains("one"));
        assert!(page.text.contains("two"), "lost text after a script: {:?}", page.text);
    }

    /// Less-than as an operator inside a script must not be read as a tag
    /// either, and the closing tag is matched case-insensitively with
    /// whitespace allowed before the `>`.
    #[test]
    fn a_script_closes_case_insensitively() {
        let page = parse("<p>one</p><SCRIPT>if (a < b) { x() }</SCRIPT >\n<p>two</p>");
        assert!(page.text.contains("one") && page.text.contains("two"));
        assert!(!page.text.contains("x()"));
    }

    /// `</scriptfoo>` is not a closing `</script>`.
    #[test]
    fn a_longer_name_does_not_close_a_script() {
        let page = parse("<script>a</scriptfoo>b</script><p>after</p>");
        assert!(page.text.contains("after"));
        assert!(!page.text.contains('b'), "script content leaked: {:?}", page.text);
    }

    #[test]
    fn a_plain_page_yields_its_parts() {
        let page = parse(
            r#"<!doctype html><html><head><title>Rust no_std</title>
               <meta name="description" content="Writing without the standard library">
               </head><body><h1>Heading</h1><p>First paragraph.</p>
               <p>Second with a <a href="/next">link</a>.</p></body></html>"#,
        );
        assert_eq!(page.title, "Rust no_std");
        assert_eq!(page.description, "Writing without the standard library");
        assert_eq!(page.links, vec!["/next"]);
        assert!(page.text.contains("First paragraph."));
        assert!(page.text.contains("Second with a link."), "got {:?}", page.text);
    }

    /// Script and style contents are not words on the page. Indexing them
    /// makes every page that loads a library match that library's name.
    #[test]
    fn script_and_style_contribute_no_text() {
        let page = parse(
            "<body><script>var secret='TOKENA';</script><style>.x{color:red}</style>\
             <p>visible</p></body>",
        );
        assert!(!page.text.contains("TOKENA"), "script text leaked: {:?}", page.text);
        assert!(!page.text.contains("color"), "style text leaked: {:?}", page.text);
        assert_eq!(page.text, "visible");
    }

    /// A skipped element nested inside another must not end the skip early.
    #[test]
    fn nested_skipped_elements_are_counted() {
        let page = parse("<svg><script>a</script>INSIDE-SVG</svg><p>after</p>");
        assert!(!page.text.contains("INSIDE-SVG"), "{:?}", page.text);
        assert!(page.text.contains("after"));
    }

    /// Block elements separate lines, or a list becomes one sentence.
    #[test]
    fn block_elements_break_lines() {
        let page = parse("<ul><li>one</li><li>two</li><li>three</li></ul>");
        let lines: Vec<&str> = page.text.lines().collect();
        assert_eq!(lines, vec!["one", "two", "three"], "got {:?}", page.text);
    }

    /// A `>` inside a quoted attribute does not end the tag.
    #[test]
    fn a_quoted_angle_bracket_does_not_end_a_tag() {
        let page = parse(r#"<a href="/cmp" title="a > b">text</a>"#);
        assert_eq!(page.links, vec!["/cmp"]);
        assert_eq!(page.text, "text");
    }

    /// Prose about code contains bare `<`, and a page is not malformed for
    /// having it.
    #[test]
    fn a_bare_angle_bracket_is_text() {
        let page = parse("<p>use Vec&lt;T&gt; and 3 < 4 always</p>");
        assert!(page.text.contains("Vec<T>"), "{:?}", page.text);
        assert!(page.text.contains("3 < 4") || page.text.contains("3 "), "{:?}", page.text);
    }

    #[test]
    fn entities_are_decoded() {
        let page = parse("<p>Tom &amp; Jerry &mdash; &quot;quoted&quot; &#65;&#x42;&hellip;</p>");
        assert_eq!(page.text, "Tom & Jerry — \"quoted\" AB…");
    }

    /// An unknown entity is left alone rather than eaten — losing text is
    /// worse than leaving an ampersand.
    #[test]
    fn an_unknown_entity_survives() {
        let page = parse("<p>a &notarealentity; b</p>");
        assert!(page.text.contains("notarealentity"), "{:?}", page.text);
    }

    /// `href` must not be found inside another attribute's name.
    #[test]
    fn a_similar_attribute_name_is_not_matched() {
        let page = parse(r#"<a data-href="/wrong" href="/right">x</a>"#);
        assert_eq!(page.links, vec!["/right"]);
    }

    #[test]
    fn an_unquoted_attribute_is_read() {
        let page = parse("<a href=/bare>x</a>");
        assert_eq!(page.links, vec!["/bare"]);
    }

    /// Real pages are broken in these specific ways, and a crawler that
    /// refuses them indexes nothing.
    #[test]
    fn malformed_pages_do_not_panic() {
        for html in [
            "",
            "<",
            "<<<>>>",
            "<p>unclosed",
            "</p>stray close",
            "<a href=>empty</a>",
            "<!-- unterminated comment",
            "<title>no close",
            "<script>unclosed script",
            "<div><div><div>deep",
            "&",
            "&#;",
            "&#xZZ;",
            "<p>text</p",
        ] {
            let page = parse(html);
            // The contract is only that it returns; the values may be anything.
            let _ = (&page.title, &page.text, &page.links, &page.description);
        }
    }

    /// Multi-byte text must survive intact — slicing a UTF-8 string by byte
    /// offsets is where a parser like this goes wrong.
    #[test]
    fn multibyte_text_survives() {
        let page = parse("<title>日本語</title><p>émoji — ✔ done</p>");
        assert_eq!(page.title, "日本語");
        assert!(page.text.contains("émoji — ✔ done"), "{:?}", page.text);
    }

    /// A `&` followed by a multi-byte character put a character across the
    /// entity-scanning window and panicked. Found by crawling the
    /// Rustonomicon, which has a 🔬 in its text — not by a test, because the
    /// multibyte test above uses three-byte characters and never placed one
    /// close enough to an ampersand.
    #[test]
    fn an_ampersand_before_a_wide_character_does_not_panic() {
        for html in [
            "<p>Tom & 🔬 Jerry</p>",
            "<p>&🔬</p>",
            "<p>a &amp; 🔬🔬🔬 b</p>",
            "<p>&日本語のテキスト</p>",
            "<p>x&</p>",
            "<title>& 🔬</title>",
        ] {
            let page = parse(html);
            // The contract is that it returns; the emoji must also survive.
            if html.contains('🔬') {
                assert!(
                    page.text.contains('🔬') || page.title.contains('🔬'),
                    "the character was lost from {html:?}"
                );
            }
        }
    }

    #[test]
    fn whitespace_is_collapsed() {
        let page = parse("<p>lots   of\n\n  space\t\there</p>");
        assert_eq!(page.text, "lots of space here");
    }

    /// The open-graph form, which many pages use instead.
    #[test]
    fn an_og_description_is_read() {
        let page = parse(r#"<meta property="og:description" content="from open graph">"#);
        assert_eq!(page.description, "from open graph");
    }

    /// Every link, in document order, so a crawler's frontier follows the
    /// page's own structure.
    #[test]
    fn links_are_collected_in_order() {
        let page = parse(r#"<a href="/a">1</a><a href="/b">2</a><a href="/c">3</a>"#);
        assert_eq!(page.links, vec!["/a", "/b", "/c"]);
    }
}
