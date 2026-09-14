//! Just enough XML to read the two documents this engine needs: a Europe PMC
//! search response, and a JATS article.
//!
//! This is not a general XML parser and does not try to be. It does not
//! validate, resolve custom entities, follow a DTD, or track namespaces — a
//! document that needs any of that is not one of the two documents above.
//! What it does is find elements by name, read the text inside them, and get
//! the nesting right, because both callers depend on nesting: a search
//! response has a `<title>` as a direct child of each `<result>`, and a JATS
//! article has `<title>` on every section heading. Something that returned
//! "the first `<title>`" would read the wrong one in one of the two cases.
//!
//! The reason it is separate from [`crate::html`] is that the two have
//! opposite jobs. HTML parsing here is lossy on purpose: it flattens a page to
//! prose and throws the structure away, and it assumes the markup is broken
//! because real pages are. XML from an API is well formed, and its structure
//! is the part that matters — which element a string sits in is the difference
//! between a licence and an author's surname.

/// The text inside each element named `name`, at any depth.
///
/// Nesting of the same name is counted, so `elements(xml, "sec")` over a JATS
/// article with subsections returns each outer section once, with its
/// subsections still inside it, rather than closing the outer one on the first
/// `</sec>` it meets.
pub fn elements<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.after;
        if tag.closing || !tag.name.eq_ignore_ascii_case(name) {
            continue;
        }
        if tag.self_closing {
            found.push("");
            continue;
        }
        if let Some(end) = close_of(xml, tag.after, name) {
            found.push(&xml[tag.after..end.content_end]);
            at = end.after;
        }
    }
    found
}

/// The text inside the first *direct child* named `name`.
///
/// Direct child, not descendant. A Europe PMC `<result>` has `<title>` as its
/// own child and also, further down, `<authorList>` entries with elements of
/// their own; asking for a descendant would let a deeper match win whenever
/// the expected child is absent, which is worse than finding nothing because
/// it is wrong rather than empty.
pub fn child<'a>(inner: &'a str, name: &str) -> Option<&'a str> {
    let mut at = 0;
    let mut depth = 0usize;
    while let Some(tag) = next_tag(inner, at) {
        at = tag.after;
        if tag.closing {
            depth = depth.saturating_sub(1);
            continue;
        }
        if tag.self_closing {
            continue;
        }
        if depth == 0 && tag.name.eq_ignore_ascii_case(name) {
            let end = close_of(inner, tag.after, name)?;
            return Some(&inner[tag.after..end.content_end]);
        }
        depth += 1;
    }
    None
}

/// The text of the first direct child named `name`, tags removed.
pub fn child_text(inner: &str, name: &str) -> Option<String> {
    child(inner, name).map(text)
}

/// All the text inside a span, with tags removed, entities decoded and runs of
/// whitespace collapsed.
///
/// Element boundaries become a single space rather than nothing, or
/// `<p>one</p><p>two</p>` would read as "onetwo".
pub fn text(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len() / 2);
    let mut at = 0;
    while at < inner.len() {
        match next_tag(inner, at) {
            Some(tag) => {
                push_text(&mut out, &inner[at..tag.start]);
                // CDATA is text that happens to be written as a tag-looking
                // thing, so its content is kept rather than skipped.
                if let Some(cdata) = tag.cdata {
                    push_text(&mut out, cdata);
                } else {
                    // A boundary between elements is a word boundary.
                    if !out.ends_with(' ') && !out.is_empty() {
                        out.push(' ');
                    }
                }
                at = tag.after;
            }
            None => {
                push_text(&mut out, &inner[at..]);
                break;
            }
        }
    }
    crate::html::decode_entities(out.trim()).trim().to_string()
}

fn push_text(out: &mut String, chunk: &str) {
    for ch in chunk.chars() {
        if ch.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(ch);
        }
    }
}

/// One tag, or one of the tag-shaped things XML allows where a tag could go.
struct Tag<'a> {
    /// Byte offset of the `<`.
    start: usize,
    /// Byte offset just past the `>`.
    after: usize,
    name: &'a str,
    closing: bool,
    self_closing: bool,
    /// The content of a CDATA section, when this is one. Comments and
    /// processing instructions have no content worth keeping and are `None`
    /// with an empty name, which makes them skipped without being text.
    cdata: Option<&'a str>,
}

/// The next tag at or after `from`.
///
/// A bare `<` that does not start a tag is not treated as one, because it does
/// occur in text that an API forgot to escape, and reading it as a tag
/// swallows everything to the next `>`.
fn next_tag<'a>(xml: &'a str, from: usize) -> Option<Tag<'a>> {
    let bytes = xml.as_bytes();
    let mut at = from;
    loop {
        let found = xml[at..].find('<')? + at;
        let rest = &xml[found..];

        // The tag-shaped things below are *returned* rather than skipped over
        // internally. A caller reading text uses `start` as the end of the
        // run of text before the tag, so anything quietly stepped over here
        // comes out as literal text — which is how `<!-- hidden -->` first
        // appeared in the middle of a snippet.
        if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let content_start = found + 9;
            let (content_end, after) = match body.find("]]>") {
                Some(i) => (content_start + i, content_start + i + 3),
                // Unterminated: the rest of the document is content, which is
                // the most that can be recovered from it.
                None => (xml.len(), xml.len()),
            };
            return Some(Tag {
                start: found,
                after,
                name: "",
                closing: false,
                self_closing: true,
                cdata: Some(&xml[content_start..content_end]),
            });
        }
        if rest.starts_with("<!--") {
            let after = xml[found + 4..]
                .find("-->")
                .map_or(xml.len(), |i| found + 4 + i + 3);
            return Some(skipped(found, after));
        }
        if rest.starts_with("<?") || rest.starts_with("<!") {
            let after = tag_end(bytes, found).map_or(xml.len(), |i| i + 1);
            return Some(skipped(found, after));
        }

        let next = bytes.get(found + 1).copied();
        let opens = matches!(next, Some(c) if c.is_ascii_alphabetic() || c == b'_' || c == b'/');
        if !opens {
            at = found + 1;
            if at >= xml.len() {
                return None;
            }
            continue;
        }

        let gt = tag_end(bytes, found)?;
        let raw = &xml[found + 1..gt];
        let closing = raw.starts_with('/');
        let body = raw.trim_start_matches('/');
        let self_closing = raw.ends_with('/');
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        return Some(Tag {
            start: found,
            after: gt + 1,
            name: &body[..name_end],
            closing,
            self_closing: self_closing && !closing,
            cdata: None,
        });
    }
}

/// A tag-shaped span with nothing in it worth keeping: a comment, a doctype,
/// a processing instruction. Marked self-closing and nameless so that element
/// matching and depth counting both pass it by, while text reading still sees
/// it as a boundary rather than as words.
fn skipped(start: usize, after: usize) -> Tag<'static> {
    Tag { start, after, name: "", closing: false, self_closing: true, cdata: None }
}

/// Where the tag opening at `open` ends, skipping `>` inside quoted attribute
/// values.
fn tag_end(bytes: &[u8], open: usize) -> Option<usize> {
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

struct Close {
    /// Byte offset of the `<` of the closing tag — the end of the content.
    content_end: usize,
    /// Byte offset just past the `>`.
    after: usize,
}

/// The closing tag matching an element named `name` whose content starts at
/// `from`, counting nested elements of the same name.
fn close_of(xml: &str, from: usize, name: &str) -> Option<Close> {
    let mut at = from;
    let mut depth = 0usize;
    while let Some(tag) = next_tag(xml, at) {
        at = tag.after;
        if !tag.name.eq_ignore_ascii_case(name) || tag.self_closing {
            continue;
        }
        if tag.closing {
            if depth == 0 {
                return Some(Close { content_end: tag.start, after: tag.after });
            }
            depth -= 1;
        } else {
            depth += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_element_yields_its_text() {
        assert_eq!(elements("<a><b>hi</b></a>", "b"), vec!["hi"]);
        assert_eq!(text("<b>hi</b>"), "hi");
    }

    #[test]
    fn every_element_of_a_name_is_found() {
        let xml = "<r><t>one</t></r><r><t>two</t></r>";
        assert_eq!(elements(xml, "r").len(), 2);
        assert_eq!(elements(xml, "t"), vec!["one", "two"]);
    }

    /// Same-name nesting is counted rather than closed on the first match.
    /// JATS sections nest, so getting this wrong truncates an article at its
    /// first subsection.
    #[test]
    fn nesting_of_the_same_name_is_counted() {
        let xml = "<sec>outer <sec>inner</sec> tail</sec>";
        let secs = elements(xml, "sec");
        assert_eq!(secs.len(), 1);
        assert_eq!(text(secs[0]), "outer inner tail");
    }

    /// The distinction the whole module exists for: a direct child, not any
    /// descendant. A Europe PMC result has its own `<title>`; so does every
    /// entry further down inside it.
    #[test]
    fn a_child_is_a_direct_child_not_a_descendant() {
        let inner = "<title>The paper</title><authorList><author><title>Dr</title></author></authorList>";
        assert_eq!(child_text(inner, "title").as_deref(), Some("The paper"));
    }

    /// And when the direct child is absent the answer is nothing, rather than
    /// a deeper element that happens to share the name.
    #[test]
    fn a_missing_child_does_not_fall_through_to_a_descendant() {
        let inner = "<authorList><author><title>Dr</title></author></authorList>";
        assert_eq!(child_text(inner, "title"), None);
    }

    #[test]
    fn element_boundaries_are_word_boundaries() {
        // Without this, two paragraphs read as one run-on word.
        assert_eq!(text("<p>one</p><p>two</p>"), "one two");
    }

    #[test]
    fn entities_are_decoded_and_whitespace_collapsed() {
        assert_eq!(text("<p>a &amp; b</p>"), "a & b");
        assert_eq!(text("<p>one\n\n   two</p>"), "one two");
        assert_eq!(text("<p>&lt;not a tag&gt;</p>"), "<not a tag>");
    }

    #[test]
    fn a_self_closing_element_has_no_content() {
        assert_eq!(text("<p>a<br/>b</p>"), "a b");
        assert_eq!(elements("<x/>", "x"), vec![""]);
    }

    /// CDATA is text written in a shape that looks like markup, so its
    /// content is kept and not scanned for tags inside.
    #[test]
    fn cdata_is_text_not_markup() {
        assert_eq!(text("<p><![CDATA[a <b> c]]></p>"), "a <b> c");
    }

    #[test]
    fn comments_and_declarations_are_skipped() {
        assert_eq!(text("<p>a<!-- hidden -->b</p>"), "a b");
        let xml = "<?xml version=\"1.0\"?><!DOCTYPE x><r><t>kept</t></r>";
        assert_eq!(elements(xml, "t"), vec!["kept"]);
    }

    /// A `>` inside a quoted attribute value does not end the tag.
    #[test]
    fn a_quoted_attribute_may_contain_a_bracket() {
        assert_eq!(text(r#"<p title="a > b">text</p>"#), "text");
        assert_eq!(elements(r#"<r id="a>b"><t>x</t></r>"#, "t"), vec!["x"]);
    }

    /// An unescaped `<` in text is not a tag. APIs do emit these, and reading
    /// one as a tag swallows the rest of the record.
    #[test]
    fn a_bare_less_than_in_text_is_not_a_tag() {
        assert_eq!(text("<p>recorded at 32 < 35 degrees</p>"), "recorded at 32 < 35 degrees");
    }

    /// An unclosed element yields nothing rather than a panic or a span
    /// running to the end of the document.
    #[test]
    fn an_unclosed_element_is_not_returned() {
        assert!(elements("<a><b>no end", "b").is_empty());
        assert_eq!(child_text("<t>unterminated", "t"), None);
    }

    #[test]
    fn names_are_matched_case_insensitively_but_not_by_prefix() {
        assert_eq!(elements("<T>x</T>", "t"), vec!["x"]);
        // `<titlegroup>` must not answer a request for `<title>`.
        assert!(elements("<titlegroup>x</titlegroup>", "title").is_empty());
    }

    #[test]
    fn hyphenated_and_underscored_names_work() {
        // JATS is full of these: article-title, ref-list, pub-date.
        assert_eq!(child_text("<article-title>T</article-title>", "article-title").as_deref(), Some("T"));
        assert_eq!(child_text("<_x>y</_x>", "_x").as_deref(), Some("y"));
    }

    #[test]
    fn empty_input_is_not_an_error() {
        assert_eq!(text(""), "");
        assert!(elements("", "a").is_empty());
        assert_eq!(child_text("", "a"), None);
    }
}
