// SPDX-License-Identifier: Apache-2.0
//! Readable text out of a document on disk, and a title for it.
//!
//! The crawler's counterpart. The engine's value was only ever reachable
//! through a network fetch, which put it out of reach of the corpus most
//! people actually have — a directory of reports, notes, papers or exported
//! logs, sitting on their own disk. Nothing about an inverted index cares
//! where the text came from.
//!
//! What this is *not* for is code. `search_code` greps, which for source is
//! the better tool: an identifier is an exact string, and exact strings are
//! what a regex is for. Ranked retrieval earns its place when the question is
//! "which of these three thousand documents answers this", which is not a
//! question anyone asks of a function name.
//!
//! Formats are deliberately few. Markdown, plain text and HTML cover prose,
//! which is what ranking is built for. Tabular and record formats — CSV,
//! JSON, NDJSON — are left out on purpose rather than forgotten: a CSV of
//! fifty thousand alerts is fifty thousand documents, not one, and indexing it
//! whole would produce a single document matching every query and answering
//! none of them. Doing that properly means a record-level ingester, which is a
//! different design and not one to arrive at by adding an extension to a list.

/// What a document on disk turned out to contain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extracted {
    /// A heading if the document had one, else the file's own name — never
    /// empty, because a result with no title is a result with nothing to click.
    pub title: String,
    /// The readable text.
    pub text: String,
}

/// The file extensions this can read, lowercase and without the dot.
pub const READABLE: &[&str] = &["md", "markdown", "mdown", "txt", "text", "rst", "org", "html", "htm"];

/// Whether a path looks like something [`extract`] can read.
pub fn is_readable(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|e| READABLE.contains(&e.as_str()))
}

/// Pull the text and a title out of a document's bytes.
///
/// `name` is the file's own name, used as the title when the document has no
/// heading of its own.
///
/// Returns `None` for something that is not text at all. Checked here rather
/// than trusted from the extension, because a `.txt` holding a JPEG is a
/// thing that happens and indexing its bytes as words would fill the
/// vocabulary with rubbish that every later query has to be scored against.
pub fn extract(bytes: &[u8], name: &str, extension: &str) -> Option<Extracted> {
    if looks_binary(bytes) {
        return None;
    }
    // Lossy rather than strict: a report with one bad byte in it is still a
    // report, and refusing the whole document over a single invalid sequence
    // would lose the other nine thousand words.
    let text = String::from_utf8_lossy(bytes);

    let extracted = match extension.to_lowercase().as_str() {
        "html" | "htm" => {
            let page = crate::html::parse(&text);
            Extracted { title: page.title, text: page.text }
        }
        "md" | "markdown" | "mdown" => markdown(&text),
        // Plain text, and anything close enough to it that stripping syntax
        // would be guessing: reStructuredText and Org both mark up with
        // punctuation that is also ordinary punctuation.
        _ => Extracted { title: first_line(&text), text: text.to_string() },
    };

    Some(Extracted {
        title: if extracted.title.trim().is_empty() {
            name.to_string()
        } else {
            extracted.title.trim().to_string()
        },
        text: extracted.text,
    })
}

/// Whether bytes are binary rather than text.
///
/// A NUL in the first few kilobytes, which is the test `grep` and `git` both
/// use and for the same reason: no text encoding this would be asked to read
/// puts a zero byte in the middle of a document, and every binary format does.
fn looks_binary(bytes: &[u8]) -> bool {
    const LOOK: usize = 8 * 1024;
    bytes[..bytes.len().min(LOOK)].contains(&0)
}

/// Markdown as prose: the syntax removed, the words kept.
///
/// Not a Markdown parser and not trying to be. Ranking wants the words and
/// their order; what matters is that `## Rotating the key` indexes as three
/// words rather than as `##` and that a link's text survives while its URL
/// does not — a document full of `https://` fragments matches queries about
/// nothing.
fn markdown(text: &str) -> Extracted {
    let mut title = String::new();
    let mut out = String::with_capacity(text.len());
    let mut fenced = false;

    for line in text.lines() {
        let trimmed = line.trim();

        // Fenced code. Kept, because a command or an error string in a runbook
        // is often the thing being looked for — but not treated as prose, so
        // the fence markers themselves go.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        let stripped = strip_inline(trimmed);

        // The first heading is the document's title. A heading rather than the
        // first line, since a Markdown file often opens with front matter or a
        // badge row.
        if title.is_empty() {
            if let Some(heading) = trimmed.strip_prefix('#') {
                let heading = heading.trim_start_matches('#').trim();
                if !heading.is_empty() {
                    title = strip_inline(heading);
                }
            }
        }

        out.push_str(&stripped);
        out.push('\n');
    }

    Extracted { title, text: out }
}

/// One line of Markdown with its markers taken off.
fn strip_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // Emphasis, headings and quote markers are punctuation standing in
            // for formatting; the tokenizer would drop them anyway, but a
            // snippet shown to a person should not be full of them.
            '*' | '_' | '`' | '#' | '>' | '~' => {}
            // A link or image: keep what it says, drop where it points. The
            // text is the part somebody would search for.
            '[' => {
                for inner in chars.by_ref() {
                    if inner == ']' {
                        break;
                    }
                    out.push(inner);
                }
                // The target, if one follows.
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1;
                    for inner in chars.by_ref() {
                        match inner {
                            '(' => depth += 1,
                            ')' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => out.push(c),
        }
    }
    out.trim().to_string()
}

/// The first line with anything on it, for a title.
fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_markdown_heading_becomes_the_title() {
        let got = extract(b"# Rotating the signing key\n\nDo this yearly.\n", "notes.md", "md").unwrap();
        assert_eq!(got.title, "Rotating the signing key");
        assert!(got.text.contains("Do this yearly."));
        // And the marker itself is not left in the text to be indexed.
        assert!(!got.text.contains('#'), "{:?}", got.text);
    }

    /// A file often opens with front matter or a badge row, so the title is
    /// the first heading rather than the first line.
    #[test]
    fn the_title_is_the_first_heading_not_the_first_line() {
        let md = "[![build](https://img.test/b.svg)](https://ci.test)\n\n# Incident 4412\n\nThe bucket was public.\n";
        let got = extract(md.as_bytes(), "x.md", "md").unwrap();
        assert_eq!(got.title, "Incident 4412");
    }

    /// A link's text is what somebody would search for; its target is noise
    /// that matches nothing.
    #[test]
    fn link_text_survives_and_the_target_does_not() {
        let got = extract(
            b"See the [escalation policy](https://wiki.test/a/b/c?x=1) for details.\n",
            "x.md",
            "md",
        )
        .unwrap();
        assert!(got.text.contains("escalation policy"), "{:?}", got.text);
        assert!(!got.text.contains("wiki.test"), "the URL was indexed: {:?}", got.text);
    }

    /// A command in a runbook is often exactly what is being looked for, so
    /// fenced code is kept — without the fences.
    #[test]
    fn fenced_code_is_kept_without_its_fences() {
        let md = "# Recovery\n\n```sh\nsystemctl restart forge-agent\n```\n\nThen check the log.\n";
        let got = extract(md.as_bytes(), "x.md", "md").unwrap();
        assert!(got.text.contains("systemctl restart forge-agent"), "{:?}", got.text);
        assert!(!got.text.contains("```"), "{:?}", got.text);
        assert!(got.text.contains("Then check the log."));
    }

    #[test]
    fn plain_text_takes_its_first_line_as_a_title() {
        let got = extract(b"\n\nQuarterly review\n\nRevenue was flat.\n", "q.txt", "txt").unwrap();
        assert_eq!(got.title, "Quarterly review");
        assert!(got.text.contains("Revenue was flat."));
    }

    #[test]
    fn html_goes_through_the_page_parser() {
        let html = b"<html><head><title>Deploy guide</title></head><body><p>Set the region first.</p></body></html>";
        let got = extract(html, "d.html", "html").unwrap();
        assert_eq!(got.title, "Deploy guide");
        assert!(got.text.contains("Set the region first."));
        assert!(!got.text.contains("<p>"), "markup was indexed: {:?}", got.text);
    }

    /// A document with no heading and no text still needs a title, or a result
    /// has nothing to click.
    #[test]
    fn a_document_with_no_heading_is_titled_by_its_file_name() {
        let got = extract(b"   \n\n", "2024-11-runbook.md", "md").unwrap();
        assert_eq!(got.title, "2024-11-runbook.md");
    }

    /// A `.txt` holding a JPEG is a thing that happens, and indexing its bytes
    /// as words fills the vocabulary with rubbish every later query is scored
    /// against.
    #[test]
    fn binary_content_is_refused_whatever_the_extension_says() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0u8; 64]);
        assert!(extract(&png, "notes.txt", "txt").is_none());
        assert!(extract(&png, "page.html", "html").is_none());
    }

    /// One bad byte should not lose the other nine thousand words.
    #[test]
    fn invalid_utf8_is_read_lossily_rather_than_refused() {
        let mut bytes = b"The value is ".to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(b" thirty degrees.");
        let got = extract(&bytes, "x.txt", "txt").unwrap();
        assert!(got.text.contains("thirty degrees"), "{:?}", got.text);
    }

    #[test]
    fn empty_input_is_not_an_error() {
        let got = extract(b"", "empty.md", "md").unwrap();
        assert_eq!(got.title, "empty.md");
        assert!(got.text.trim().is_empty());
    }

    #[test]
    fn only_the_listed_extensions_are_claimed() {
        for yes in ["a.md", "a.MD", "a.txt", "a.html", "a.htm", "a.rst", "a.org"] {
            assert!(is_readable(std::path::Path::new(yes)), "{yes} should be readable");
        }
        // Code goes to search_code, and tabular data wants a record-level
        // ingester rather than a line on this list.
        for no in ["a.rs", "a.py", "a.csv", "a.json", "a.pdf", "a.png", "a", "a.tar.gz"] {
            assert!(!is_readable(std::path::Path::new(no)), "{no} should not be readable");
        }
    }
}
