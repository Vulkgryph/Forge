//! What a person typed, and the text to show them back.
//!
//! Two jobs that belong together because both are about the query as written
//! rather than as tokens: understanding the operators in it, and quoting the
//! part of each document that made it match.
//!
//! The snippet is the half people underrate. A result list of titles and URLs
//! tells a reader which page might answer their question; a result list with
//! the matching line tells them whether it does. For an agent it is the
//! difference between having to fetch three pages and having to fetch none —
//! which, in an engine whose whole purpose is to save the agent a round trip,
//! is most of the value.

use crate::index::{DocId, Index};
use crate::tokenize;

/// A parsed query.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Query {
    /// Terms that must appear.
    pub required: Vec<String>,
    /// Terms that must not — written `-term`.
    pub excluded: Vec<String>,
    /// Quoted runs that must appear consecutively, each as its own term list.
    pub phrases: Vec<Vec<String>>,
}

impl Query {
    /// Parse the operators a search box is expected to understand.
    ///
    /// Three, and no more: quoting for a phrase, `-` to exclude, and bare
    /// words. Deliberately not a language — every additional operator is one
    /// more thing a person can get subtly wrong, and one more way for a
    /// literal `-` in an identifier to change the meaning of their search.
    pub fn parse(input: &str) -> Self {
        let mut q = Query::default();
        let chars: Vec<char> = input.chars().collect();
        let mut at = 0;

        while at < chars.len() {
            if chars[at].is_whitespace() {
                at += 1;
                continue;
            }
            // A leading `-` excludes — but only when a word follows it, so a
            // bare dash, or `x86-64`, is not read as an operator.
            let negated = chars[at] == '-'
                && matches!(chars.get(at + 1), Some(&c) if !c.is_whitespace() && c != '-');
            if negated {
                at += 1;
            }
            if chars[at] == '"' {
                at += 1;
                let start = at;
                while at < chars.len() && chars[at] != '"' {
                    at += 1;
                }
                let phrase: String = chars[start..at].iter().collect();
                // An unterminated quote runs to the end of the input, which is
                // what a person half-way through typing has. Refusing it would
                // make the query empty at exactly the moment they are looking
                // at the results.
                if at < chars.len() {
                    at += 1;
                }
                let terms = tokenize::terms(&phrase);
                if terms.is_empty() {
                    continue;
                }
                if negated {
                    q.excluded.extend(terms);
                } else {
                    // A phrase's terms are also required individually: the
                    // candidate set is built from required terms, and a phrase
                    // whose words are absent cannot match anyway.
                    q.required.extend(terms.iter().cloned());
                    if terms.len() > 1 {
                        q.phrases.push(terms);
                    }
                }
                continue;
            }
            let start = at;
            while at < chars.len() && !chars[at].is_whitespace() {
                at += 1;
            }
            let word: String = chars[start..at].iter().collect();
            let terms = tokenize::terms(&word);
            if negated {
                q.excluded.extend(terms);
            } else {
                q.required.extend(terms);
            }
        }
        // A term both required and excluded is a contradiction the person did
        // not mean; the exclusion is honoured, since it is the more specific
        // thing to have typed.
        q.required.retain(|t| !q.excluded.contains(t));
        q.required.dedup();
        q
    }

    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.phrases.is_empty()
    }
}

/// One result, ready to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Result_ {
    pub url: String,
    pub title: String,
    /// The text around the match, with the matching terms marked by the
    /// caller's own markers — see [`snippet`].
    pub snippet: String,
    pub score: f64,
}

/// Search, filter by the query's operators, and build a snippet for each hit.
pub fn search(index: &Index, input: &str, limit: usize) -> Vec<Result_> {
    let query = Query::parse(input);
    if query.is_empty() {
        return Vec::new();
    }
    let mut hits = crate::rank::search(index, &query.required.join(" "), limit * 4);

    hits.retain(|hit| {
        // Excluded terms.
        if query
            .excluded
            .iter()
            .any(|t| !index.positions(t, hit.doc).is_empty())
        {
            return false;
        }
        // Quoted phrases must actually be consecutive, which ranking only
        // rewards rather than requires.
        query.phrases.iter().all(|p| has_phrase(index, hit.doc, p))
    });
    let hits = one_per_page(index, hits);
    let hits = spread_across_hosts(index, hits, limit);

    hits.into_iter()
        .filter_map(|hit| {
            let doc = index.document(hit.doc)?;
            Some(Result_ {
                url: doc.url.clone(),
                title: doc.title.clone(),
                snippet: snippet(&doc.text, &query.required, 240)
                    .or_else(|| {
                        (!doc.description.is_empty()).then(|| doc.description.clone())
                    })
                    .unwrap_or_else(|| first_words(&doc.text, 240)),
                score: hit.score,
            })
        })
        .collect()
}

/// At most this many results from any one host, while other hosts have
/// results to offer.
///
/// Two, not one: a forum thread and the site's own article on the same subject
/// are worth seeing together, and a hard limit of one would hide the better of
/// two passages from the source that knows most about the question.
const MAX_PER_HOST: usize = 2;

/// Keep one result per page, collapsing query-string variants of it.
///
/// A query string very often selects a view of one document rather than a
/// different document: `?pivots=azure-cli`, `?tab=bicep`, `?lang=en`,
/// `?print=1`. Those are the same page to a reader, and each extra copy costs
/// a result slot that a genuinely different page could have had.
///
/// Measured: a search of Microsoft's Container Apps documentation returned
/// `scale-app?pivots=azure-cli`, `scale-app?&pivots=azure-resource-manager`
/// and `scale-app?&pivots=container-apps-bicep` as results one, two and
/// three — three of five slots on one document, while the billing and jobs
/// pages that answered the rest of the question sat below the cut.
///
/// The best-scoring variant wins, so nothing is lost but the duplicates. This
/// runs before the per-host cap: collapsing first is what lets that cap spend
/// its two slots on two different pages.
///
/// It is deliberately not applied at indexing time. A query string sometimes
/// *is* the document — `?id=`, `?page=2`, a search result — and merging those
/// in the index would lose pages. Here the cost of being wrong is one result
/// shown instead of two near-identical ones, which is the safer direction.
fn one_per_page(index: &Index, hits: Vec<crate::rank::Hit>) -> Vec<crate::rank::Hit> {
    let mut seen = std::collections::HashSet::new();
    hits.into_iter()
        .filter(|hit| {
            let key = index
                .document(hit.doc)
                .map(|d| match crate::url::Url::parse(&d.url) {
                    // Host and path, without the query.
                    Ok(u) => format!("{}{}", u.host, u.path),
                    Err(_) => d.url.clone(),
                })
                .unwrap_or_default();
            seen.insert(key)
        })
        .collect()
}

/// Reorder so the results span sources instead of one source's best pages.
///
/// A question with contested answers is the case this exists for. Ranking is
/// per document, so a site with five good pages on a subject takes all five
/// slots and the disagreement becomes invisible — the reader sees one position
/// stated five times and no sign that anywhere else says otherwise. Spreading
/// across hosts makes the spread of opinion visible in the result list itself,
/// which for "what oil does this engine take" is most of the answer.
///
/// Nothing is dropped: a host's third and later passages are deferred to the
/// end in score order and still fill the list when no other source has
/// anything. So the output is no longer strictly sorted by score, which is the
/// intended effect rather than a defect.
fn spread_across_hosts(
    index: &Index,
    hits: Vec<crate::rank::Hit>,
    limit: usize,
) -> Vec<crate::rank::Hit> {
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut kept: Vec<crate::rank::Hit> = Vec::new();
    let mut deferred: Vec<crate::rank::Hit> = Vec::new();
    for hit in hits {
        let host = host_of(index, hit.doc);
        let count = seen.entry(host).or_insert(0);
        if *count < MAX_PER_HOST {
            *count += 1;
            kept.push(hit);
        } else {
            deferred.push(hit);
        }
    }
    kept.truncate(limit);
    for hit in deferred {
        if kept.len() >= limit {
            break;
        }
        kept.push(hit);
    }
    kept
}

/// The host a document came from, or its url when that cannot be parsed.
///
/// The unit of independence. Two threads on one forum are one source agreeing
/// with itself; the same claim on two hosts is two sources.
fn host_of(index: &Index, doc: DocId) -> String {
    index
        .document(doc)
        .map(|d| {
            crate::url::Url::parse(&d.url)
                .map(|u| u.host)
                .unwrap_or_else(|_| d.url.clone())
        })
        .unwrap_or_default()
}

/// One thing the matching passages say, and who says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mention {
    /// The term, as indexed.
    pub term: String,
    /// The distinct hosts whose matching passage contains it, sorted. The
    /// length of this is the number of independent sources.
    pub sources: Vec<String>,
    /// How many passages contained it, which can exceed the number of sources
    /// when one site says it more than once.
    pub passages: usize,
}

/// What the matching passages actually say, and how many independent sources
/// say each thing.
///
/// Ranking answers "which document is most relevant". It does not answer "do
/// the sources agree", and for a question whose answers are contested that is
/// the question being asked. Search a tractor forum for what oil a 1948 engine
/// takes and the honest answer is not the top-ranked passage — it is that
/// three sources say one grade, two say another, and the reason they differ is
/// that the recommendation changed. A single passage presented as the answer
/// hides exactly that.
///
/// So this tallies the distinctive terms appearing near matches, grouped by
/// host, and sorts by how many independent sources carry each. No model and no
/// knowledge of any particular site: a term is distinctive if it is not a stop
/// word, not part of the query, and not in most of the corpus, and sources are
/// counted by host because two pages on one site are one site.
///
/// `consider` is how many ranked documents to read, which bounds the work.
pub fn spread(index: &Index, input: &str, consider: usize) -> Vec<Mention> {
    let query = Query::parse(input);
    if query.is_empty() {
        return Vec::new();
    }
    let asked: std::collections::HashSet<&str> =
        query.required.iter().map(|t| t.as_str()).collect();

    let hits = crate::rank::search(index, &query.required.join(" "), consider);
    // term -> (hosts, passage count)
    let mut tally: std::collections::HashMap<String, (std::collections::HashSet<String>, usize)> =
        std::collections::HashMap::new();

    for hit in &hits {
        let Some(doc) = index.document(hit.doc) else { continue };
        // The passage, not the whole document. A term three screens away from
        // any match is not part of what this document says in answer.
        let passage = snippet(&doc.text, &query.required, SPREAD_WINDOW)
            .unwrap_or_else(|| first_words(&doc.text, SPREAD_WINDOW));
        let host = host_of(index, hit.doc);

        let mut counted = std::collections::HashSet::new();
        for term in crate::tokenize::terms(&passage) {
            if asked.contains(term.as_str()) || term.len() < 2 {
                continue;
            }
            // Function words are a closed class, so a list is the honest tool
            // for them. `tokenize::terms` deliberately keeps them — phrase
            // matching needs "end of the line" to stay four words — so the
            // filtering happens here.
            if crate::tokenize::is_stop_word(&term) {
                continue;
            }
            // Present in nearly every document, so it separates nothing: a
            // site's navigation, or a word the whole corpus shares because of
            // what it is about.
            //
            // The threshold is near-universal rather than a majority, and the
            // first version of this got it wrong in a way worth recording: at
            // "more than half the corpus" a term in three documents out of
            // five was discarded — which is precisely the *majority answer* to
            // a contested question. The thing being looked for looked like
            // boilerplate by frequency alone. Corroboration is the signal
            // here, so only something approaching unanimity is noise.
            if index.document_frequency(&term) as usize * 10 >= index.len() * 9 {
                continue;
            }
            // Once per passage, so one page repeating a word is not agreement.
            if !counted.insert(term.clone()) {
                continue;
            }
            let entry = tally.entry(term).or_insert_with(|| (std::collections::HashSet::new(), 0));
            entry.0.insert(host.clone());
            entry.1 += 1;
        }
    }

    let mut out: Vec<Mention> = tally
        .into_iter()
        .map(|(term, (hosts, passages))| {
            let mut sources: Vec<String> = hosts.into_iter().collect();
            sources.sort();
            Mention { term, sources, passages }
        })
        .collect();
    // Most independently corroborated first. Ties broken by passage count and
    // then by the term, so the order does not depend on hash iteration.
    out.sort_by(|a, b| {
        b.sources
            .len()
            .cmp(&a.sources.len())
            .then(b.passages.cmp(&a.passages))
            .then(a.term.cmp(&b.term))
    });
    out
}

/// How much of each document to read when tallying.
///
/// Wider than a displayed snippet: this is looking for what a passage says
/// rather than cutting a quotation, and the sentence carrying the answer is
/// often just after the one carrying the query's words.
const SPREAD_WINDOW: usize = 600;

/// Whether `terms` appear consecutively, in order, in `doc`.
///
/// Walks the first term's positions and checks the rest follow. A phrase is
/// short, so this is cheaper than anything cleverer.
pub fn has_phrase(index: &Index, doc: DocId, terms: &[String]) -> bool {
    if terms.len() < 2 {
        return true;
    }
    let first = index.positions(&terms[0], doc);
    first.iter().any(|&start| {
        terms.iter().enumerate().skip(1).all(|(offset, term)| {
            index
                .positions(term, doc)
                .binary_search(&(start + offset as u32))
                .is_ok()
        })
    })
}

/// The passage of `text` best covering `terms`, at most `max_bytes`.
///
/// `None` when no term appears, so the caller can fall back to the page's own
/// description rather than showing an arbitrary first paragraph that has
/// nothing to do with the query.
///
/// The window is chosen by counting matches rather than by taking the first
/// one. The first mention of a term is often in a navigation menu or a
/// breadcrumb; the passage where several of the query's words appear together
/// is the one that answers the question.
pub fn snippet(text: &str, terms: &[String], max_bytes: usize) -> Option<String> {
    if terms.is_empty() || text.is_empty() {
        return None;
    }
    let lower = text.to_lowercase();
    // Byte offsets of every occurrence, tagged with which term it was. The tag
    // is what lets a window be scored by how much of the *query* it covers
    // rather than by how many words it happens to repeat.
    let mut marks: Vec<(usize, usize)> = Vec::new();
    for (i, term) in terms.iter().enumerate() {
        let mut from = 0;
        while let Some(found) = lower[from..].find(term.as_str()) {
            let at = from + found;
            if is_word_boundary(&lower, at, term.len()) {
                marks.push((at, i));
            }
            from = at + term.len().max(1);
            if from >= lower.len() {
                break;
            }
        }
    }
    if marks.is_empty() {
        return None;
    }
    marks.sort_unstable();

    // The best window covers the most *distinct* query terms, with total
    // occurrences breaking a tie.
    //
    // Counting occurrences alone gets this wrong in a way that matters: a
    // navigation menu saying "allocator" twice scores the same as the sentence
    // saying "the global allocator in no_std builds", and on a tie the earlier
    // window — the menu — wins. A passage containing both of the query's words
    // is answering the question; one repeating a single word may only be a
    // list of links.
    let mut best_start = marks[0].0;
    let mut best = (0usize, 0usize);
    for &(start, _) in &marks {
        let end = start + max_bytes;
        let inside: Vec<usize> = marks
            .iter()
            .filter(|&&(m, _)| m >= start && m < end)
            .map(|&(_, term)| term)
            .collect();
        let mut distinct: Vec<usize> = inside.clone();
        distinct.sort_unstable();
        distinct.dedup();
        let score = (distinct.len(), inside.len());
        if score > best {
            best = score;
            best_start = start;
        }
    }

    // Open the window a little before the first mark so the match is not
    // flush against the left edge, then settle on word and character
    // boundaries so the snippet reads as prose rather than as a slice.
    let lead = max_bytes / 4;
    let raw_start = best_start.saturating_sub(lead);
    let start = word_start(text, raw_start);
    let end = word_end(text, (start + max_bytes).min(text.len()));

    let mut out = String::with_capacity(max_bytes + 8);
    if start > 0 {
        out.push('…');
    }
    out.push_str(text[start..end].trim());
    if end < text.len() {
        out.push('…');
    }
    Some(out)
}

/// Whether the match at `at` is a whole word rather than part of a longer one.
///
/// Without this, searching for `alloc` highlights the middle of `preallocated`
/// and the snippet points at something that is not what was asked for.
fn is_word_boundary(s: &str, at: usize, len: usize) -> bool {
    let before_ok = at == 0
        || s[..at]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
    let after = at + len;
    let after_ok = after >= s.len()
        || s[after..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
    before_ok && after_ok
}

/// Back up to a character boundary, then to the start of a word.
fn word_start(s: &str, mut at: usize) -> usize {
    at = at.min(s.len());
    while at > 0 && !s.is_char_boundary(at) {
        at -= 1;
    }
    if at == 0 {
        return 0;
    }
    // At most a short way, so a long unbroken run does not push the window
    // back to the start of the document.
    let floor = at.saturating_sub(40);
    let mut i = at;
    while i > floor {
        if s.is_char_boundary(i) && s[i..].starts_with(|c: char| c.is_whitespace()) {
            return i + 1;
        }
        i -= 1;
    }
    at
}

/// Forward to a character boundary, then back to the end of a word.
fn word_end(s: &str, mut at: usize) -> usize {
    at = at.min(s.len());
    while at < s.len() && !s.is_char_boundary(at) {
        at += 1;
    }
    if at >= s.len() {
        return s.len();
    }
    let floor = at.saturating_sub(40);
    let mut i = at;
    while i > floor {
        if s.is_char_boundary(i) && s[i..].starts_with(|c: char| c.is_whitespace()) {
            return i;
        }
        i -= 1;
    }
    at
}

/// The opening of a document, for when nothing matched in its text.
fn first_words(text: &str, max_bytes: usize) -> String {
    // Start at the first line that reads like prose rather than like a menu.
    //
    // Block elements are already on their own lines, so a navigation item is a
    // line of two or three words and a paragraph is a long one. Taking the
    // first long line steps over the sidebar and the table of contents without
    // knowing anything about the site — which matters because this fallback is
    // what a reader sees when the match itself is out of reach, and "Jump to
    // content Main menu move to sidebar hide Navigation" tells them nothing.
    const PROSE: usize = 120;
    let mut at = 0;
    for line in text.split('\n') {
        if line.trim().len() >= PROSE {
            break;
        }
        at += line.len() + 1;
    }
    // No prose found — a page that really is a list of links. Its head is then
    // the most honest thing to show.
    let body = if at >= text.len() { text.trim() } else { text[at..].trim() };
    if body.len() <= max_bytes {
        return body.to_string();
    }
    let end = word_end(body, max_bytes);
    format!("{}…", body[..end].trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    /// A match past the old 8 KB snippet cap must still be reachable.
    ///
    /// The cap applied to the stored text but not to the postings, so a term
    /// deep in a page ranked as a hit and then had no snippet to show — and
    /// the fallback showed the head of the page instead. On real articles
    /// (median 25 KB of text) that lost 17% of snippets outright.
    #[test]
    fn a_match_deep_in_a_page_still_gets_a_snippet() {
        let mut ix = Index::new();
        // Past 8 KB, inside 32 KB.
        let filler = "Filler about unrelated matters. ".repeat(400);
        ix.add(
            "https://x.example/long",
            "A long page",
            "",
            &format!("{filler}The recordings were made at thirty five degrees."),
        );
        let hits = search(&ix, "recordings degrees", 3);
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.contains("thirty five degrees"),
            "snippet never reached the match: {:?}",
            hits[0].snippet
        );
    }

    /// When there is genuinely no snippet to draw, the summary must not be the
    /// sidebar.
    ///
    /// The query term is in the title only, which is how this happens in
    /// practice: the document is a hit on a term its body never uses, so the
    /// fallback runs. Showing "Jump to content / Main menu / move to sidebar /
    /// Navigation" tells a reader nothing about the page.
    #[test]
    fn the_fallback_skips_the_navigation_menu() {
        let mut ix = Index::new();
        ix.add(
            "https://x.example/p",
            "Hodgkin Huxley neuron model",
            "",
            "Jump to content\nMain menu\nmove to sidebar\nNavigation\nMain page\n\
             Contents\nCurrent events\nRandom article\nAbout\nContact us\n\
             The model describes how a cell membrane generates an action potential, \
             and the rate constants it uses were fitted to measurements made on the \
             squid giant axon at a stated temperature.",
        );
        let hits = search(&ix, "neuron", 3);
        assert_eq!(hits.len(), 1, "the title term should still make it a hit");
        assert!(
            hits[0].snippet.starts_with("The model describes"),
            "the sidebar was shown as the summary: {:?}",
            hits[0].snippet
        );
    }

    /// A page that really is only a list of links has no prose to find, and
    /// its head is then the most honest thing to show rather than nothing.
    #[test]
    fn a_page_of_only_links_still_gets_a_summary() {
        let mut ix = Index::new();
        ix.add("https://x.example/toc", "Index of things", "",
               "Alpha\nBeta\nGamma\nDelta");
        let hits = search(&ix, "index", 3);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].snippet.is_empty(), "no summary at all");
    }

    /// Five sites answering one contested question, which is the shape that
    /// motivated both `spread` and host diversification: a 1948 tractor engine
    /// and what oil to put in it, where the forums genuinely disagree.
    fn contested() -> Index {
        let mut ix = Index::new();
        let page = |ix: &mut Index, host: &str, path: &str, body: &str| {
            ix.add(&format!("https://{host}/{path}"), "Oil thread", "", body);
        };
        page(&mut ix, "forum-a.test", "t/1",
             "What engine oil for the tractor? I run straight 30 weight in mine, \
              same as the manual says. Never had a problem in twenty years.");
        page(&mut ix, "forum-b.test", "t/2",
             "For engine oil I switched to 15w-40 diesel oil. It has more zddp \
              than modern car oil, which matters for a flat tappet cam.");
        page(&mut ix, "forum-c.test", "t/3",
             "Engine oil question comes up a lot. 15w-40 is what I run year round \
              and the old timers at the club say the same.");
        page(&mut ix, "forum-d.test", "t/4",
             "The manual calls for straight 30 non-detergent engine oil. \
              Detergent oil in an engine with no filter is asking for trouble.");
        page(&mut ix, "forum-e.test", "t/5",
             "Any decent engine oil works. I use 15w-40 because the tractor \
              shares it with the truck.");
        ix
    }

    /// The point of the whole exercise: a contested question should come back
    /// as a spread of positions with source counts, not as one passage stated
    /// as though it settled the matter.
    #[test]
    fn a_contested_question_reports_who_says_what() {
        let ix = contested();
        let spread = spread(&ix, "engine oil", 20);
        let of = |term: &str| spread.iter().find(|m| m.term == term);

        let grade = of("15w-40").expect("15w-40 was never surfaced");
        let straight = of("30").expect("straight 30 was never surfaced");
        assert_eq!(grade.sources.len(), 3, "{:?}", grade);
        assert_eq!(straight.sources.len(), 2, "{:?}", straight);
        // The better-corroborated position sorts first, but both are present —
        // the minority view is the interesting half of a disagreement.
        assert!(
            spread.iter().position(|m| m.term == "15w-40")
                < spread.iter().position(|m| m.term == "30"),
            "three sources should outrank two: {spread:?}",
        );
    }

    /// Sources are counted by host, because two pages on one site are one site
    /// agreeing with itself rather than corroboration.
    #[test]
    fn one_site_saying_a_thing_twice_is_one_source() {
        let mut ix = Index::new();
        ix.add("https://one.test/a", "", "", "engine oil should be 15w-40 always");
        ix.add("https://one.test/b", "", "", "engine oil, use 15w-40, no question");
        ix.add("https://two.test/a", "", "", "engine oil wants straight 30 weight");
        let spread = spread(&ix, "engine oil", 20);
        let grade = spread.iter().find(|m| m.term == "15w-40").expect("15w-40");
        assert_eq!(grade.sources, vec!["one.test"], "{grade:?}");
        assert_eq!(grade.passages, 2, "both passages should still be counted");
    }

    /// A term in most of the corpus distinguishes nothing, so it is not a
    /// position anyone is taking. This is what keeps ordinary words and
    /// navigation furniture out without a list of either.
    #[test]
    fn words_common_to_the_whole_corpus_are_not_positions() {
        let ix = contested();
        let spread = spread(&ix, "engine oil", 20);
        for common in ["the", "i", "for"] {
            assert!(
                !spread.iter().any(|m| m.term == common),
                "{common:?} was reported as a position: {spread:?}",
            );
        }
    }

    fn hosts_of(ix: &Index, hits: &[Result_]) -> Vec<String> {
        let _ = ix;
        hits.iter()
            .map(|h| crate::url::Url::parse(&h.url).map(|u| u.host).unwrap_or_default())
            .collect()
    }

    /// Query-string variants of one page are one page.
    ///
    /// Measured on Microsoft's Container Apps documentation, where
    /// `scale-app?pivots=azure-cli`, `scale-app?&pivots=azure-resource-manager`
    /// and `scale-app?&pivots=container-apps-bicep` took results one, two and
    /// three — three of five slots on a single document.
    #[test]
    fn one_page_does_not_appear_three_times_under_different_query_strings() {
        let mut ix = Index::new();
        let body = "Scaling settings: minReplicas and maxReplicas control the replica count.";
        for pivot in ["azure-cli", "azure-resource-manager", "container-apps-bicep"] {
            ix.add(
                &format!("https://learn.test/azure/scale-app?pivots={pivot}"),
                "Scaling in Azure Container Apps",
                "",
                body,
            );
        }
        ix.add("https://learn.test/azure/billing", "Billing", "", "Scaling to zero incurs no charges for replicas.");
        ix.add("https://learn.test/azure/jobs", "Jobs", "", "Jobs do not support the replica scaling rules above.");

        let hits = search(&ix, "scaling replicas", 5);
        let paths: Vec<String> = hits
            .iter()
            .map(|h| h.url.split('?').next().unwrap_or("").to_string())
            .collect();
        let scale_app = paths.iter().filter(|p| p.ends_with("/scale-app")).count();
        assert_eq!(scale_app, 1, "the same page appeared {scale_app} times: {paths:?}");
        // And the slots it was taking go to different pages.
        assert!(paths.iter().any(|p| p.ends_with("/billing")), "{paths:?}");
        assert!(paths.iter().any(|p| p.ends_with("/jobs")), "{paths:?}");
    }

    /// A site with plenty to say must not crowd out the sites that disagree
    /// with it. Every host here has enough pages to fill the list on its own.
    #[test]
    fn no_host_exceeds_its_share_when_others_have_pages() {
        let mut ix = Index::new();
        for host in ["loud.test", "other.test", "third.test"] {
            for i in 0..4 {
                ix.add(
                    &format!("https://{host}/{i}"),
                    "Oil",
                    "",
                    "engine oil is straight 30 weight and that is final",
                );
            }
        }
        let hits = search(&ix, "engine oil", 6);
        let hosts = hosts_of(&ix, &hits);
        for host in ["loud.test", "other.test", "third.test"] {
            let n = hosts.iter().filter(|h| *h == host).count();
            assert!(n <= MAX_PER_HOST, "{host} took {n} of 6 slots: {hosts:?}");
        }
        assert_eq!(hosts.iter().collect::<std::collections::HashSet<_>>().len(), 3);
    }

    /// The dissenting source is heard even when it is outnumbered five to one
    /// and its page does not rank best.
    ///
    /// The cap is deliberately soft: here the loud site does end up with three
    /// of four slots, because after the one dissenter there is nothing else to
    /// put in the list and a slot left empty helps nobody. What matters is
    /// that the minority view is not buried, which is the half of a
    /// disagreement a reader cannot get anywhere else.
    #[test]
    fn the_dissenting_source_is_heard() {
        let mut ix = Index::new();
        // The loud site's pages are built to outrank the dissenter: they say
        // the query's words repeatedly and say little else. Without this the
        // test proves nothing — a short page mentioning the terms once already
        // scores well on length normalisation, so the dissenter would reach
        // the list on its own merits and the mechanism would go untested.
        for i in 0..5 {
            ix.add(
                &format!("https://loud.test/{i}"),
                "Engine oil",
                "",
                "engine oil engine oil engine oil straight 30 weight engine oil",
            );
        }
        ix.add(
            "https://quiet.test/1",
            "A long thread",
            "",
            &format!(
                "{} Somebody asked about engine oil and the answer here is 15w-40. {}",
                "Unrelated chatter about implements and paint. ".repeat(20),
                "More unrelated chatter follows for a while. ".repeat(20),
            ),
        );
        let hits = search(&ix, "engine oil", 4);
        let hosts = hosts_of(&ix, &hits);
        assert!(
            hosts.iter().any(|h| h == "quiet.test"),
            "the dissenting source never made the list: {hosts:?}",
        );
        assert_eq!(hits.len(), 4, "a slot was left empty rather than backfilled");
    }

    /// And nothing is dropped: when no other source has anything, the deferred
    /// pages still fill the list.
    #[test]
    fn deferring_is_not_dropping() {
        let mut ix = Index::new();
        for i in 0..5 {
            ix.add(&format!("https://only.test/{i}"), "Oil", "", "engine oil straight 30 weight");
        }
        let hits = search(&ix, "engine oil", 4);
        assert_eq!(hits.len(), 4, "results were lost rather than deferred");
    }

    fn corpus() -> Index {
        let mut ix = Index::new();
        ix.add(
            "https://docs.example/alloc",
            "Allocation",
            "How allocation works",
            "Navigation: home allocator docs. \
             The global allocator is chosen at link time. In no_std builds an \
             allocator must be provided by the crate itself, or nothing can \
             allocate at all. Preallocated buffers avoid the problem.",
        );
        ix.add(
            "https://docs.example/no-std",
            "Writing no_std code",
            "",
            // Deliberately free of the word "gardening": the exclusion test
            // needs a page that mentions the required term and not the
            // excluded one, and an earlier version of this text said "does not
            // discuss gardening" — which contains it, so the page was
            // correctly excluded and the test was wrong about the code.
            "In no_std builds there is no allocator unless you bring one. \
             This page is only about the standard library.",
        );
        ix.add(
            "https://blog.example/gardening",
            "Gardening",
            "Tomatoes and sunshine",
            "Tomatoes need sunshine. An allocator is not relevant to gardening.",
        );
        ix
    }

    // ── Query parsing ───────────────────────────────────────────────────────

    #[test]
    fn bare_words_are_required() {
        let q = Query::parse("no_std allocator");
        assert_eq!(q.required, vec!["no_std", "allocator"]);
        assert!(q.excluded.is_empty());
        assert!(q.phrases.is_empty());
    }

    #[test]
    fn a_leading_dash_excludes() {
        let q = Query::parse("allocator -gardening");
        assert_eq!(q.required, vec!["allocator"]);
        assert_eq!(q.excluded, vec!["gardening"]);
    }

    /// A `-` inside an identifier is not an operator, or searching for
    /// `x86-64` becomes a search for `x86` excluding `64`.
    #[test]
    fn a_dash_inside_a_word_is_not_an_operator() {
        let q = Query::parse("x86-64 utf-8");
        assert_eq!(q.required, vec!["x86-64", "utf-8"]);
        assert!(q.excluded.is_empty());
    }

    #[test]
    fn a_bare_dash_is_not_an_operator() {
        let q = Query::parse("a - b");
        assert_eq!(q.required, vec!["a", "b"]);
        assert!(q.excluded.is_empty());
    }

    #[test]
    fn quotes_make_a_phrase() {
        let q = Query::parse(r#""global allocator" no_std"#);
        assert_eq!(q.phrases, vec![vec!["global".to_string(), "allocator".to_string()]]);
        // The phrase's words are required individually too, since the
        // candidate set is built from required terms.
        assert!(q.required.contains(&"global".to_string()));
        assert!(q.required.contains(&"no_std".to_string()));
    }

    /// Someone half-way through typing has an unterminated quote, and the
    /// results are what they are looking at while they type.
    #[test]
    fn an_unterminated_quote_still_searches() {
        let q = Query::parse(r#"allocator "global alloc"#);
        assert!(!q.is_empty(), "the query became empty mid-typing");
        assert!(q.required.contains(&"allocator".to_string()));
    }

    #[test]
    fn a_negated_phrase_excludes_its_words() {
        let q = Query::parse(r#"allocator -"garden shed""#);
        assert_eq!(q.required, vec!["allocator"]);
        assert!(q.excluded.contains(&"garden".to_string()));
        assert!(q.excluded.contains(&"shed".to_string()));
    }

    /// A contradiction the person did not mean. The exclusion wins because it
    /// is the more specific thing to have typed.
    #[test]
    fn a_term_both_required_and_excluded_is_excluded() {
        let q = Query::parse("allocator -allocator gardening");
        assert!(!q.required.contains(&"allocator".to_string()));
        assert!(q.excluded.contains(&"allocator".to_string()));
        assert_eq!(q.required, vec!["gardening"]);
    }

    #[test]
    fn an_empty_query_is_empty() {
        for input in ["", "   ", "\"\"", "-", "- -", ",,, ;;;"] {
            assert!(Query::parse(input).is_empty(), "{input:?} was not empty");
        }
    }

    // ── Searching with operators ────────────────────────────────────────────

    #[test]
    fn exclusion_removes_a_matching_document() {
        let ix = corpus();
        let with = search(&ix, "allocator", 10);
        assert_eq!(with.len(), 3, "expected all three to mention it");
        let without = search(&ix, "allocator -gardening", 10);
        assert_eq!(without.len(), 2, "{:?}", without.iter().map(|r| &r.url).collect::<Vec<_>>());
        assert!(without.iter().all(|r| !r.url.contains("gardening")));
    }

    /// Ranking rewards proximity; a quoted phrase must *require* it.
    #[test]
    fn a_phrase_requires_the_words_to_be_consecutive() {
        let ix = corpus();
        let loose = search(&ix, "global allocator", 10);
        assert!(!loose.is_empty());
        let strict = search(&ix, r#""global allocator""#, 10);
        assert_eq!(strict.len(), 1, "{:?}", strict.iter().map(|r| &r.url).collect::<Vec<_>>());
        assert!(strict[0].url.contains("/alloc"));

        // And a phrase that appears nowhere matches nothing, even though both
        // words are present in the corpus.
        assert!(search(&ix, r#""allocator gardening""#, 10).is_empty());
    }

    #[test]
    fn a_phrase_must_be_in_order() {
        let mut ix = Index::new();
        ix.add("https://a.example/x", "", "", "the allocator global registry");
        assert!(search(&ix, r#""global allocator""#, 10).is_empty(), "matched out of order");
        assert_eq!(search(&ix, r#""allocator global""#, 10).len(), 1);
    }

    // ── Snippets ────────────────────────────────────────────────────────────

    /// The point of a snippet: the line that answers the question, not the
    /// first line of the page.
    #[test]
    fn a_snippet_quotes_the_matching_passage() {
        let ix = corpus();
        let hits = search(&ix, "no_std allocator", 10);
        let alloc = hits.iter().find(|r| r.url.contains("/alloc")).unwrap();
        assert!(
            alloc.snippet.contains("no_std"),
            "the snippet missed the match: {:?}",
            alloc.snippet
        );
        assert!(alloc.snippet.len() <= 260, "snippet is {} bytes", alloc.snippet.len());
    }

    /// The first mention of a term is often a navigation menu. The passage
    /// where several query words appear together is the one worth showing.
    #[test]
    fn the_densest_passage_is_chosen_over_the_first_mention() {
        let text = "Navigation: allocator docs home. \
                    Filler that goes on for a while without saying anything useful at all here. \
                    The global allocator in no_std builds must be provided by the crate.";
        let got = snippet(text, &["allocator".into(), "no_std".into()], 120).unwrap();
        assert!(got.contains("no_std"), "chose the navigation menu: {got:?}");
    }

    /// Searching for `alloc` must not point at the middle of `preallocated`.
    #[test]
    fn a_match_must_be_a_whole_word() {
        let text = "Preallocated buffers are mentioned first. Later the alloc crate appears.";
        let got = snippet(text, &["alloc".into()], 80).unwrap();
        assert!(got.contains("alloc crate"), "matched inside another word: {got:?}");
    }

    /// No match means the caller should show the page's own description
    /// rather than an arbitrary paragraph.
    #[test]
    fn no_match_yields_no_snippet() {
        assert!(snippet("nothing relevant here", &["quicksilver".into()], 80).is_none());
        assert!(snippet("", &["x".into()], 80).is_none());
        assert!(snippet("text", &[], 80).is_none());
    }

    /// Which is what `search` then does — a result is never left with an
    /// empty snippet.
    #[test]
    fn a_result_always_has_something_to_show() {
        let ix = corpus();
        for query in ["allocator", "no_std", "tomatoes", "gardening"] {
            for r in search(&ix, query, 10) {
                assert!(!r.snippet.trim().is_empty(), "{query:?} gave an empty snippet");
            }
        }
    }

    /// A snippet is shown to a person, so it should not begin or end
    /// mid-word.
    #[test]
    fn a_snippet_falls_on_word_boundaries() {
        let text = "alpha bravo charlie delta echo foxtrot golf hotel india juliet kilo lima \
                    allocator mike november oscar papa quebec romeo sierra tango uniform";
        let got = snippet(text, &["allocator".into()], 60).unwrap();
        let inner = got.trim_matches('…');
        // Every word in the snippet is a whole word from the source.
        for word in inner.split_whitespace() {
            assert!(text.contains(word), "{word:?} is not a whole word from the text");
        }
    }

    #[test]
    fn multibyte_text_snippets_without_splitting_a_character() {
        let text = "日本語のテキストです。allocator はここにあります。さらに文章が続きます。";
        let got = snippet(text, &["allocator".into()], 60).unwrap();
        assert!(std::str::from_utf8(got.as_bytes()).is_ok());
        assert!(got.contains("allocator"));
    }

    /// An ellipsis says the passage was cut, and its absence says it was not.
    #[test]
    fn an_ellipsis_marks_only_a_real_cut() {
        let short = snippet("a short allocator line", &["allocator".into()], 240).unwrap();
        assert!(!short.contains('…'), "marked an uncut snippet: {short:?}");

        let long = snippet(
            &format!("{} allocator {}", "before ".repeat(60), "after ".repeat(60)),
            &["allocator".into()],
            120,
        )
        .unwrap();
        assert!(long.starts_with('…') && long.ends_with('…'), "{long:?}");
    }

    #[test]
    fn results_are_capped_and_ordered() {
        let mut ix = Index::new();
        for i in 0..20 {
            ix.add(&format!("https://a.example/{i}"), "", "", "allocator memory page");
        }
        let hits = search(&ix, "allocator", 5);
        assert_eq!(hits.len(), 5);
        for pair in hits.windows(2) {
            assert!(pair[0].score >= pair[1].score);
        }
    }

    #[test]
    fn searching_an_empty_index_or_with_an_empty_query_is_safe() {
        let empty = Index::new();
        assert!(search(&empty, "anything", 10).is_empty());
        assert!(search(&corpus(), "", 10).is_empty());
        assert!(search(&corpus(), "-only-an-exclusion", 10).is_empty());
    }
}
