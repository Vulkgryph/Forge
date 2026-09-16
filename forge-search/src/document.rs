// SPDX-License-Identifier: Apache-2.0
//! A document on disk, read as the sections it is actually made of.
//!
//! The crawler's counterpart. The engine's value was only ever reachable
//! through a network fetch, which put it out of reach of the corpus most
//! people actually have — a directory of reports, notes, papers or exported
//! logs, sitting on their own disk. Nothing about an inverted index cares
//! where the text came from.
//!
//! ## Why sections rather than files
//!
//! A file was one document at first, and that was wrong in three ways at once.
//! Ranking normalises by length, so a three-thousand-line changelog mentioning
//! a term once scored as a weak match while the same term scattered across
//! twenty unrelated entries scored as a strong one. The title came from the
//! top of the file, so a passage from deep inside was labelled "Changelog" —
//! three results in a real run came back with that title and nothing to tell
//! them apart. And the result was a snippet plus an implicit instruction to go
//! and read the whole file, which for an agent is the expensive part: context
//! is the budget, and spending it on three thousand lines to reach forty is
//! the thing retrieval was supposed to avoid.
//!
//! So a document is its sections. Each one carries the heading trail that
//! leads to it and the lines it occupies, which is exactly what `read_file`
//! takes — the answer to "where is this" becomes a range rather than a file.
//!
//! Boundaries are the document's own, not a fixed byte count. The author
//! already decided where the topics divide and wrote it down as headings;
//! guessing again every four hundred words would be ignoring the one reliable
//! signal in the file. It also means no overlap is needed: overlapping windows
//! exist to stop a fixed-size cut landing mid-answer, and a cut that only ever
//! lands on a heading has much less to protect against.
//!
//! ## What is not read
//!
//! Code. `search_code` greps, and for source that is the better tool: an
//! identifier is an exact string. Ranked retrieval earns its place on the
//! different question — which of three thousand documents answers this.
//!
//! Tabular and record formats, deliberately rather than by omission. A CSV of
//! fifty thousand alerts is fifty thousand documents, not one, and indexing it
//! whole would produce a single document matching every query and answering
//! none. Doing it properly means a record-level ingester, which is a different
//! design and not one to arrive at by adding an extension to a list.

/// One section of a document: a piece of prose with a name and a place.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    /// The headings leading here, outermost first. Empty for a document with
    /// no headings at all.
    ///
    /// The trail rather than the heading alone, because section headings are
    /// generic on their own — "Overview", "Notes", "Fixed" — and it is the
    /// path through them that says which Overview this is.
    pub trail: Vec<String>,
    /// The lines this section occupies, 1-indexed and inclusive, in the file
    /// as it is on disk.
    ///
    /// `None` when the source's own line numbers do not survive extraction,
    /// which is the case for HTML: the text comes out of a parser that
    /// collapses markup, so a line in it is not a line in the file. Better to
    /// say nothing than to report a range that reads back the wrong text.
    pub lines: Option<(usize, usize)>,
    /// The readable text, with markup stripped.
    pub text: String,
}

impl Section {
    /// The section's name: its heading trail, or empty for an unstructured
    /// one. The caller supplies a fallback, since only it knows the file name.
    pub fn title(&self) -> String {
        self.trail.join(" › ")
    }
}

/// What a file turned out to contain.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Read {
    /// The document's own title — its first heading, or its first line.
    pub title: String,
    /// Its sections, in the order they appear. Never empty for a file with
    /// any text in it.
    pub sections: Vec<Section>,
}

/// The file extensions this can read, lowercase and without the dot.
pub const READABLE: &[&str] =
    &["md", "markdown", "mdown", "txt", "text", "rst", "org", "html", "htm"];

/// How many words a section aims for.
///
/// Sized for the thing that reads it. At roughly four-thirds of a token per
/// word this is about five hundred tokens — a page of prose, enough to answer
/// a question without the answer needing its neighbours, and cheap enough that
/// five of them in a result do not crowd out the conversation they are part of.
const TARGET_WORDS: usize = 350;

/// Past this a section is split at a paragraph boundary even with no heading
/// to divide it, because something has to bound the cost of one badly
/// structured file.
const MAX_WORDS: usize = 1_000;

/// Whether a path looks like something [`read`] can read.
pub fn is_readable(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|e| READABLE.contains(&e.as_str()))
}

/// Read a document's bytes into its title and its sections.
///
/// `name` is the file's own name, used as the title when the document has no
/// heading of its own.
///
/// Returns `None` for something that is not text at all. Checked here rather
/// than trusted from the extension, because a `.txt` holding a JPEG is a thing
/// that happens and indexing its bytes as words would fill the vocabulary with
/// rubbish that every later query then has to be scored against.
pub fn read(bytes: &[u8], name: &str, extension: &str) -> Option<Read> {
    if looks_binary(bytes) {
        return None;
    }
    // Lossy rather than strict: a report with one bad byte in it is still a
    // report, and refusing the whole document over a single invalid sequence
    // would lose the other nine thousand words.
    let text = String::from_utf8_lossy(bytes);

    let (title, sections) = match extension.to_lowercase().as_str() {
        "html" | "htm" => {
            let page = crate::html::parse(&text);
            // No line numbers: the parser collapses markup, so a line here is
            // not a line in the file. Paragraph boundaries are all the
            // structure that survives.
            (page.title, by_paragraph(&page.text, None))
        }
        // The title is read from the source rather than taken off the first
        // section, because grouping can join a badge row or a front-matter
        // block onto the first real heading — and then the section that
        // carries the title is not the first one.
        "md" | "markdown" | "mdown" => (first_heading(&text), by_heading(&text)),
        // Plain text, and the markup languages whose syntax is also ordinary
        // punctuation — stripping reStructuredText or Org by guesswork would
        // remove words people search for.
        _ => (first_line(&text), by_paragraph(&text, Some(1))),
    };

    Some(Read {
        title: if title.trim().is_empty() { name.to_string() } else { title.trim().to_string() },
        sections,
    })
}

/// Markdown split at its headings, then regrouped to a useful size.
fn by_heading(source: &str) -> Vec<Section> {
    // Pieces first: one per heading, however small.
    let mut pieces: Vec<Section> = Vec::new();
    let mut trail: Vec<(usize, String)> = Vec::new();
    let mut current = Section { lines: Some((1, 1)), ..Section::default() };
    let mut fenced = false;

    for (n, line) in source.lines().enumerate() {
        let number = n + 1;
        let trimmed = line.trim();

        // A fence opens or closes. Tracked so a `#` comment inside a shell
        // snippet is not read as a heading — which would put a section
        // boundary in the middle of the command somebody is looking for.
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            current.lines = Some((current.lines.map_or(number, |(a, _)| a), number));
            continue;
        }

        if !fenced {
            if let Some(depth) = heading_depth(trimmed) {
                let text = trimmed.trim_start_matches('#').trim();
                if !text.is_empty() {
                    if !current.text.trim().is_empty() {
                        pieces.push(std::mem::take(&mut current));
                    }
                    // Ancestors at a shallower depth stay; same or deeper go.
                    trail.retain(|(d, _)| *d < depth);
                    trail.push((depth, strip_inline(text)));
                    current = Section {
                        trail: trail.iter().map(|(_, t)| t.clone()).collect(),
                        lines: Some((number, number)),
                        text: String::new(),
                    };
                    continue;
                }
            }
        }

        if fenced {
            // Kept verbatim. A command or an error string in a runbook is
            // often the thing being looked for, and stripping punctuation out
            // of it would make it unsearchable.
            current.text.push_str(line);
        } else {
            current.text.push_str(&strip_inline(trimmed));
        }
        current.text.push('\n');
        let start = current.lines.map_or(number, |(a, _)| a);
        current.lines = Some((start, number));
    }
    if !current.text.trim().is_empty() || !current.trail.is_empty() {
        pieces.push(current);
    }

    regroup(pieces)
}

/// A document's first heading, ignoring anything inside a code fence.
fn first_heading(source: &str) -> String {
    let mut fenced = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if heading_depth(trimmed).is_some() {
            let text = strip_inline(trimmed.trim_start_matches('#').trim());
            if !text.is_empty() {
                return text;
            }
        }
    }
    String::new()
}

/// The heading level of an ATX heading line, or `None`.
fn heading_depth(trimmed: &str) -> Option<usize> {
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    // Six is Markdown's limit; more is not a heading, and a bare row of
    // hashes is a rule rather than a section.
    (1..=6).contains(&hashes).then_some(hashes)
}

/// Pieces gathered into sections of a useful size.
///
/// Greedy, and that one rule covers both problems. A stub — a heading with one
/// line under it — is joined to what follows instead of becoming a document
/// that answers nothing; a long section is left alone. The name comes from the
/// first piece in the group, which is the outermost heading of the run and so
/// the one that describes it.
fn regroup(pieces: Vec<Section>) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    for piece in pieces {
        // Anything oversized is split at paragraph boundaries first, so one
        // badly structured file cannot produce a single enormous section.
        let parts = if words(&piece.text) > MAX_WORDS {
            split_long(&piece)
        } else {
            vec![piece]
        };

        for part in parts {
            match out.last_mut() {
                // Join to the previous group while there is room. The heading
                // trail of the group is the first piece's, which is why this
                // appends rather than replacing.
                Some(last) if words(&last.text) < TARGET_WORDS && words(&part.text) < TARGET_WORDS => {
                    // A group that began with preamble — a badge row, front
                    // matter — has no name of its own, so it takes the first
                    // real heading it absorbs. Otherwise the section holding
                    // the document's title would be the one section without a
                    // title.
                    if last.trail.is_empty() && !part.trail.is_empty() {
                        last.trail = part.trail.clone();
                    }
                    if !last.text.ends_with('\n') {
                        last.text.push('\n');
                    }
                    // The joined heading is kept as text, or a search for it
                    // would not find the section it names.
                    if let Some(heading) = part.trail.last() {
                        last.text.push_str(heading);
                        last.text.push('\n');
                    }
                    last.text.push_str(&part.text);
                    last.lines = match (last.lines, part.lines) {
                        (Some((a, _)), Some((_, b))) => Some((a, b)),
                        (a, b) => a.or(b),
                    };
                }
                _ => out.push(part),
            }
        }
    }
    out.retain(|s| !s.text.trim().is_empty());
    out
}

/// One oversized section cut at paragraph boundaries.
fn split_long(section: &Section) -> Vec<Section> {
    let start = section.lines.map(|(a, _)| a);
    let mut out = Vec::new();
    let mut text = String::new();
    let mut first = start;
    let mut line = start;

    for (n, para) in section.text.split("\n\n").enumerate() {
        let _ = n;
        let height = para.lines().count().max(1) + 1;
        if words(&text) >= TARGET_WORDS && !text.trim().is_empty() {
            out.push(Section {
                trail: section.trail.clone(),
                lines: first.zip(line),
                text: std::mem::take(&mut text),
            });
            first = line;
        }
        text.push_str(para);
        text.push_str("\n\n");
        line = line.map(|l| l + height);
    }
    if !text.trim().is_empty() {
        out.push(Section {
            trail: section.trail.clone(),
            lines: first.zip(section.lines.map(|(_, b)| b)),
            text,
        });
    }
    out
}

/// Text with no headings, grouped into sections at blank lines.
///
/// `start` is the line the text begins on, or `None` when the source's line
/// numbers did not survive extraction.
fn by_paragraph(text: &str, start: Option<usize>) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut current = String::new();
    let mut first = start;
    let mut line = start;

    for para in text.split("\n\n") {
        let height = para.lines().count().max(1) + 1;
        if words(&current) >= TARGET_WORDS {
            out.push(Section {
                trail: Vec::new(),
                lines: first.zip(line),
                text: std::mem::take(&mut current),
            });
            first = line;
        }
        current.push_str(para.trim_end());
        current.push_str("\n\n");
        line = line.map(|l| l + height);
    }
    if !current.trim().is_empty() {
        out.push(Section {
            trail: Vec::new(),
            lines: first.zip(line.map(|l| l.saturating_sub(1))).map(|(a, b)| (a, b.max(a))),
            text: current,
        });
    }
    out
}

/// Words, for sizing a section. Whitespace-separated, which is close enough to
/// the tokenizer's count for a threshold and far cheaper than tokenising.
fn words(text: &str) -> usize {
    text.split_whitespace().count()
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

/// One line of Markdown with its markers taken off.
///
/// Not a Markdown parser and not trying to be. Ranking wants the words and
/// their order; what matters is that `## Rotating the key` indexes as three
/// words rather than as `##`, and that a link's text survives while its URL
/// does not — a document full of `https://` fragments matches queries about
/// nothing.
fn strip_inline(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            // Emphasis, headings and quote markers are punctuation standing in
            // for formatting. The tokenizer would drop them anyway, but a
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

    fn read_md(source: &str) -> Read {
        read(source.as_bytes(), "notes.md", "md").unwrap()
    }

    /// The point of the whole module: a long document comes back as the
    /// sections it is made of, each named by the headings leading to it.
    #[test]
    fn a_document_becomes_its_sections_each_named_by_its_heading_trail() {
        let filler = |what: &str| format!("{} ", what).repeat(400);
        let source = format!(
            "# Runbook\n\n{}\n\n## Recovery\n\n{}\n\n### Restarting the agent\n\n{}\n",
            filler("intro"),
            filler("recovery"),
            filler("restart"),
        );
        let got = read_md(&source);
        assert_eq!(got.title, "Runbook");
        assert_eq!(got.sections.len(), 3, "{:?}", got.sections.iter().map(|s| s.title()).collect::<Vec<_>>());
        assert_eq!(got.sections[0].title(), "Runbook");
        assert_eq!(got.sections[1].title(), "Runbook › Recovery");
        // The trail, not the heading alone: "Restarting the agent" on its own
        // says nothing about which runbook it belongs to.
        assert_eq!(got.sections[2].title(), "Runbook › Recovery › Restarting the agent");
        assert!(got.sections[1].text.contains("recovery"));
        assert!(!got.sections[1].text.contains("restart"), "sections bled into each other");
    }

    /// A deeper heading followed by a shallower one pops the trail rather than
    /// nesting forever.
    #[test]
    fn a_shallower_heading_pops_the_trail() {
        let filler = |what: &str| format!("{} ", what).repeat(400);
        let source = format!(
            "# Top\n\n{}\n\n## A\n\n{}\n\n### A1\n\n{}\n\n## B\n\n{}\n",
            filler("top"), filler("aaa"), filler("aone"), filler("bbb"),
        );
        let got = read_md(&source);
        let titles: Vec<String> = got.sections.iter().map(|s| s.title()).collect();
        assert_eq!(
            titles,
            vec!["Top", "Top › A", "Top › A › A1", "Top › B"],
            "{titles:?}"
        );
    }

    /// The lines are what makes a result actionable: `read_file` takes a
    /// range, so the agent can pull the section instead of the file.
    #[test]
    fn a_section_reports_the_lines_it_occupies() {
        let source = "# One\n\nalpha\n\n# Two\n\nbeta\ngamma\n";
        let got = read_md(source);
        // Both are stubs, so they group — and the group spans both.
        let (first, last) = got.sections[0].lines.expect("markdown keeps line numbers");
        assert_eq!(first, 1, "{:?}", got.sections[0]);
        assert_eq!(last, 8, "{:?}", got.sections[0]);

        // Big enough to stay apart, and then each range is its own.
        let filler = |what: &str| format!("{} ", what).repeat(400);
        let source = format!("# One\n\n{}\n\n# Two\n\n{}\n", filler("alpha"), filler("beta"));
        let got = read_md(&source);
        assert_eq!(got.sections.len(), 2);
        let (_, end_of_first) = got.sections[0].lines.unwrap();
        let (start_of_second, _) = got.sections[1].lines.unwrap();
        assert!(
            start_of_second > end_of_first,
            "ranges overlap: {:?} then {:?}",
            got.sections[0].lines,
            got.sections[1].lines
        );
    }

    /// A heading with one line under it is not a document. Grouping stubs is
    /// what stops a changelog becoming four hundred sections that each answer
    /// nothing.
    #[test]
    fn stub_sections_are_grouped_rather_than_indexed_alone() {
        let mut source = String::from("# Changelog\n\n");
        for i in 0..40 {
            source.push_str(&format!("## Version {i}\n\nFixed a thing numbered {i}.\n\n"));
        }
        let got = read_md(&source);
        assert!(
            got.sections.len() < 8,
            "{} sections for forty one-line entries",
            got.sections.len()
        );
        // And nothing was lost in the grouping.
        let all: String = got.sections.iter().map(|s| s.text.as_str()).collect();
        for i in 0..40 {
            assert!(all.contains(&format!("numbered {i}")), "entry {i} was dropped");
        }
        // The headings survive as text, or searching for one would not find
        // the section that carries it.
        assert!(all.contains("Version 7"), "a joined heading was lost");
    }

    /// One badly structured file must not produce one enormous section.
    #[test]
    fn a_section_with_no_headings_in_it_is_split_at_paragraphs() {
        let para = format!("{}\n\n", "word ".repeat(120));
        let source = format!("# Wall\n\n{}", para.repeat(30));
        let got = read_md(&source);
        assert!(got.sections.len() > 1, "a 3,600-word section was left whole");
        for section in &got.sections {
            assert!(
                words(&section.text) <= MAX_WORDS + TARGET_WORDS,
                "a split section is still {} words",
                words(&section.text)
            );
            // Every piece keeps the name of what it came from.
            assert_eq!(section.title(), "Wall");
        }
    }

    /// A `#` inside a shell snippet is a comment, not a heading — splitting
    /// there would cut the command somebody is looking for in half.
    #[test]
    fn a_hash_inside_a_code_fence_is_not_a_heading() {
        let source = "# Recovery\n\n```sh\n# restart it\nsystemctl restart forge-agent\n```\n\nThen check the log.\n";
        let got = read_md(source);
        assert_eq!(got.sections.len(), 1, "{:?}", got.sections.iter().map(|s| s.title()).collect::<Vec<_>>());
        assert!(got.sections[0].text.contains("systemctl restart forge-agent"));
        assert!(!got.sections[0].text.contains("```"));
    }

    #[test]
    fn a_markdown_heading_becomes_the_title() {
        let got = read_md("# Rotating the signing key\n\nDo this yearly.\n");
        assert_eq!(got.title, "Rotating the signing key");
        assert!(got.sections[0].text.contains("Do this yearly."));
        assert!(!got.sections[0].text.contains('#'), "{:?}", got.sections[0].text);
    }

    /// A file often opens with front matter or a badge row, so the title is
    /// the first heading rather than the first line.
    #[test]
    fn the_title_is_the_first_heading_not_the_first_line() {
        let got = read_md(
            "[![build](https://img.test/b.svg)](https://ci.test)\n\n# Incident 4412\n\nThe bucket was public.\n",
        );
        assert_eq!(got.title, "Incident 4412");
    }

    #[test]
    fn link_text_survives_and_the_target_does_not() {
        let got = read_md("See the [escalation policy](https://wiki.test/a/b/c?x=1) for details.\n");
        let text: String = got.sections.iter().map(|s| s.text.as_str()).collect();
        assert!(text.contains("escalation policy"), "{text:?}");
        assert!(!text.contains("wiki.test"), "the URL was indexed: {text:?}");
    }

    #[test]
    fn plain_text_is_sectioned_by_paragraph_and_keeps_its_lines() {
        let para = format!("{}\n\n", "word ".repeat(120));
        let source = format!("Quarterly review\n\n{}", para.repeat(10));
        let got = read(source.as_bytes(), "q.txt", "txt").unwrap();
        assert_eq!(got.title, "Quarterly review");
        assert!(got.sections.len() > 1, "1,200 words came back as one section");
        assert!(got.sections[0].lines.is_some(), "plain text should keep line numbers");
        assert!(got.sections.iter().all(|s| s.trail.is_empty()), "plain text has no headings");
    }

    /// HTML goes through the page parser, which collapses markup — so its line
    /// numbers do not survive, and saying nothing beats reporting a range that
    /// reads back the wrong text.
    #[test]
    fn html_is_sectioned_but_reports_no_lines() {
        let html = b"<html><head><title>Deploy guide</title></head><body><p>Set the region first.</p></body></html>";
        let got = read(html, "d.html", "html").unwrap();
        assert_eq!(got.title, "Deploy guide");
        assert!(got.sections[0].text.contains("Set the region first."));
        assert!(!got.sections[0].text.contains("<p>"));
        assert!(got.sections[0].lines.is_none(), "HTML claimed line numbers it cannot know");
    }

    #[test]
    fn a_document_with_no_heading_is_titled_by_its_file_name() {
        let got = read(b"   \n\n", "2024-11-runbook.md", "md").unwrap();
        assert_eq!(got.title, "2024-11-runbook.md");
    }

    #[test]
    fn binary_content_is_refused_whatever_the_extension_says() {
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0u8; 64]);
        assert!(read(&png, "notes.txt", "txt").is_none());
        assert!(read(&png, "page.html", "html").is_none());
    }

    #[test]
    fn invalid_utf8_is_read_lossily_rather_than_refused() {
        let mut bytes = b"The value is ".to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(b" thirty degrees.");
        let got = read(&bytes, "x.txt", "txt").unwrap();
        assert!(got.sections[0].text.contains("thirty degrees"));
    }

    #[test]
    fn empty_input_is_not_an_error() {
        let got = read(b"", "empty.md", "md").unwrap();
        assert_eq!(got.title, "empty.md");
        assert!(got.sections.is_empty(), "an empty file produced a section");
    }

    #[test]
    fn only_the_listed_extensions_are_claimed() {
        for yes in ["a.md", "a.MD", "a.txt", "a.html", "a.htm", "a.rst", "a.org"] {
            assert!(is_readable(std::path::Path::new(yes)), "{yes} should be readable");
        }
        for no in ["a.rs", "a.py", "a.csv", "a.json", "a.pdf", "a.png", "a", "a.tar.gz"] {
            assert!(!is_readable(std::path::Path::new(no)), "{no} should not be readable");
        }
    }

    /// Every line of the source has to end up in some section. A chunker that
    /// silently drops the text between two boundaries is the worst kind of
    /// broken: it answers, just never with the thing that was missed.
    #[test]
    fn no_text_is_lost_between_sections() {
        let mut source = String::from("# Doc\n\n");
        for i in 0..60 {
            source.push_str(&format!("## Part {i}\n\nSentence {i} with the marker word zebra{i}.\n\n"));
        }
        let got = read_md(&source);
        let all: String = got.sections.iter().map(|s| s.text.as_str()).collect();
        for i in 0..60 {
            assert!(all.contains(&format!("zebra{i}")), "zebra{i} is in no section");
        }
    }
}
