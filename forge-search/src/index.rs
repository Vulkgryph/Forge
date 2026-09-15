//! The inverted index: which documents contain a term, and where.
//!
//! A forward index answers "what words are on this page", which no search
//! query ever asks. An inverted one answers "which pages contain this word",
//! which is the only question a search engine is asked — so the whole design
//! is a map from term to the documents holding it.
//!
//! Held in memory and written to one file. That is a deliberate ceiling rather
//! than an oversight: the corpus this is built for is documentation and source
//! that an agent has crawled, which is tens of thousands of pages, not the web.
//! At that size a memory-resident index is faster than anything involving
//! seeks, and a single file is something a person can delete, copy or inspect.
//! When it stops being enough, the format below is versioned so it can change.

use std::collections::HashMap;
use std::path::Path;

use crate::tokenize;

/// A document's identity inside the index.
///
/// An index, not a URL, because the postings repeat it once per term
/// occurrence and a URL is fifty bytes. This is also why documents are never
/// removed by rewriting postings — see [`Index::remove`].
pub type DocId = u32;

/// What is kept about a document besides its words.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Document {
    pub url: String,
    pub title: String,
    /// The page's own description, used for a snippet when the query matches
    /// nothing quotable in the body.
    pub description: String,
    /// The readable text, kept so a snippet can be cut around the match. This
    /// is the bulk of the index's size and the reason it is capped.
    pub text: String,
    /// How many terms the document has, which ranking needs in order to stop
    /// preferring long documents simply for containing more words.
    pub term_count: u32,
    /// Whether the document is still live. A removed document keeps its id and
    /// its postings; see [`Index::remove`].
    pub live: bool,
    /// What share of the text, as a percentage, sits in lines long enough to
    /// be prose.
    ///
    /// The difference between a page with something on it and a page that is a
    /// list of links to pages with something on them. A forum's board index
    /// mentions the subject in forty thread titles and answers nothing; the
    /// thread answers it. Ranked the same, the index wins on title matches and
    /// the answer never surfaces.
    ///
    /// Measured on real pages, and the margin is wide enough to use: board
    /// listings came out at 4% and 9%, discussion threads at 27% and 42%, a
    /// Wikipedia article at 72%.
    ///
    /// Computed over the whole text before it is capped for snippets, which
    /// matters — the kept head of a long page is the navigation, so measuring
    /// the stored text would underestimate exactly the long articles that are
    /// worth most.
    ///
    /// A line rather than a sentence because block elements already end lines,
    /// and the same threshold the snippet fallback uses to step over a
    /// navigation menu.
    pub prose_share: u8,
    /// Who the content belongs to and on what terms — a licence, a required
    /// credit, or empty when the source said nothing.
    ///
    /// Stored with the document rather than alongside the fetch, because the
    /// obligation outlives the request. An index saved to disk and searched a
    /// month later must still be able to say that a passage came from a CC
    /// BY-NC article, since that answer is what decides whether the passage
    /// may be shown, quoted, or used commercially. Metadata kept only in a
    /// crawl report is metadata lost on the first save.
    pub attribution: String,
}

/// One occurrence list: the positions of a term within one document.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Posting {
    pub doc: DocId,
    /// Term positions, ascending. Needed for phrase matching; a count alone
    /// would rank "no_std allocator" the same whether the words are adjacent
    /// or in different sections.
    pub positions: Vec<u32>,
}

/// How much of a document's text is kept for snippets.
///
/// The text dominates the index's size, so it is capped — but only the
/// snippet is affected. Ranking is unaffected: term counts and positions come
/// from the whole document before this is applied.
///
/// That asymmetry is a trap, and it bit. A term deep in a page has a posting
/// and no stored text, so the document ranks as a hit and then has no
/// snippet to show for it — and the fallback shows the head of the page,
/// which on a site with a sidebar is the navigation menu. The earlier value
/// of 8 KB was justified on the grounds that a first match "is almost always
/// early", which was only true while a parser bug was cutting pages down to a
/// few hundred bytes.
///
/// Measured on a thirty-page crawl of Wikipedia neuroscience articles (median
/// page 25 KB of text, largest 121 KB), over seven queries and every hit they
/// returned:
///
/// ```text
///    8 KB   83% of hits had a reachable snippet     242 KB stored
///   16 KB   91%                                     448 KB
///   32 KB  100%                                     728 KB
///   64 KB  100%                                     991 KB
/// ```
///
/// The furthest first match was at byte 22,407, so 32 KB covers the observed
/// worst case with room to spare, and 64 KB buys nothing for a third more
/// space.
const TEXT_KEPT: usize = 32 * 1024;

/// An in-memory inverted index.
#[derive(Clone, Debug, Default)]
pub struct Index {
    /// term → the documents containing it.
    postings: HashMap<String, Vec<Posting>>,
    docs: Vec<Document>,
    /// url → id, so re-crawling a page updates it rather than duplicating it.
    by_url: HashMap<String, DocId>,
    /// Live documents only — the denominator for inverse document frequency,
    /// which is wrong if it counts removed ones.
    live_docs: u32,
    /// Summed term counts over live documents, for the average length ranking
    /// needs.
    total_terms: u64,
}

impl Index {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many live documents the index holds.
    pub fn len(&self) -> usize {
        self.live_docs as usize
    }

    pub fn is_empty(&self) -> bool {
        self.live_docs == 0
    }

    /// The mean document length in terms, which ranking uses to normalise.
    ///
    /// One rather than zero when the index is empty: this is a divisor, and a
    /// ranking function that divides by the average length should not have to
    /// know the index might be empty.
    pub fn average_length(&self) -> f64 {
        if self.live_docs == 0 {
            return 1.0;
        }
        (self.total_terms as f64 / self.live_docs as f64).max(1.0)
    }

    pub fn document(&self, id: DocId) -> Option<&Document> {
        self.docs.get(id as usize).filter(|d| d.live)
    }

    /// The documents containing `term`, or an empty slice.
    pub fn postings(&self, term: &str) -> &[Posting] {
        self.postings.get(term).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// How many live documents contain `term`.
    ///
    /// Counted rather than taken from the posting list's length, because that
    /// includes documents since removed — and this figure decides how much a
    /// term is worth, so counting dead ones understates every rare term.
    pub fn document_frequency(&self, term: &str) -> u32 {
        self.postings(term)
            .iter()
            .filter(|p| self.document(p.doc).is_some())
            .count() as u32
    }

    /// Add a page, or replace it if the URL is already indexed.
    ///
    /// Re-crawling is the normal case, so this is an upsert. Returns the id.
    pub fn add(&mut self, url: &str, title: &str, description: &str, text: &str) -> DocId {
        self.add_attributed(url, title, description, text, "")
    }

    /// Add a document that carries terms of use.
    ///
    /// The same as [`add`](Self::add) but recording an attribution string —
    /// for content from a source that states a licence, which a crawled web
    /// page generally does not and a journal article always does.
    pub fn add_attributed(
        &mut self,
        url: &str,
        title: &str,
        description: &str,
        text: &str,
        attribution: &str,
    ) -> DocId {
        if let Some(&existing) = self.by_url.get(url) {
            self.remove(existing);
        }
        let tokens = tokenize::tokenize(text);
        // Title terms are indexed as well, at positions after the body, so a
        // page whose *title* matches is findable even when the body never
        // repeats the words. Ranking can weight them later; being absent from
        // the index is not something ranking can fix.
        let title_tokens = tokenize::tokenize(title);
        let term_count = (tokens.len() + title_tokens.len()) as u32;

        let id = self.docs.len() as DocId;
        let mut grouped: HashMap<&str, Vec<u32>> = HashMap::new();
        for t in &tokens {
            grouped.entry(&t.term).or_default().push(t.position as u32);
        }
        let body_end = tokens.len() as u32;
        for t in &title_tokens {
            grouped.entry(&t.term).or_default().push(body_end + t.position as u32);
        }
        for (term, mut positions) in grouped {
            positions.sort_unstable();
            self.postings
                .entry(term.to_string())
                .or_default()
                .push(Posting { doc: id, positions });
        }

        self.docs.push(Document {
            url: url.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            text: truncate_on_boundary(text, TEXT_KEPT),
            term_count,
            live: true,
            prose_share: prose_share(text),
            attribution: attribution.to_string(),
        });
        self.by_url.insert(url.to_string(), id);
        self.live_docs += 1;
        self.total_terms += term_count as u64;
        id
    }

    /// Mark a document dead.
    ///
    /// Its postings stay. Removing them means touching every term the document
    /// contained, which is the one operation an inverted index is bad at — and
    /// a search engine deletes rarely and searches constantly, so the cost
    /// belongs at read time, where it is a single check per candidate. The
    /// space is reclaimed by [`Index::compact`].
    pub fn remove(&mut self, id: DocId) -> bool {
        let Some(doc) = self.docs.get_mut(id as usize).filter(|d| d.live) else {
            return false;
        };
        doc.live = false;
        let count = doc.term_count;
        let url = std::mem::take(&mut doc.url);
        self.by_url.remove(&url);
        self.live_docs -= 1;
        self.total_terms = self.total_terms.saturating_sub(count as u64);
        true
    }

    /// Rebuild without the dead documents, returning how many were dropped.
    ///
    /// Ids are reassigned, so anything holding one must discard it — which is
    /// why this is explicit rather than something `remove` does on a threshold.
    pub fn compact(&mut self) -> usize {
        let dead = self.docs.iter().filter(|d| !d.live).count();
        if dead == 0 {
            return 0;
        }
        let mut rebuilt = Index::new();
        for doc in self.docs.iter().filter(|d| d.live) {
            rebuilt.add_attributed(
                &doc.url,
                &doc.title,
                &doc.description,
                &doc.text,
                &doc.attribution,
            );
        }
        *self = rebuilt;
        dead
    }

    /// Drop the oldest documents until at most `max` remain, reclaiming their
    /// space. Returns how many went.
    ///
    /// An index of crawled pages is a cache, and a cache with no bound is a
    /// leak. This one lives next to the project it belongs to, so the leak is
    /// in somebody's working directory: measured at roughly twelve kilobytes
    /// per page, an unbounded index reaches tens of megabytes after a few
    /// afternoons of research and keeps going.
    ///
    /// Oldest by insertion, which is free — document ids are handed out in
    /// order, so the id *is* the arrival order and no timestamp has to be
    /// stored or trusted. It is a proxy for "least likely to be wanted" rather
    /// than a measurement of it, which is the usual trade for an eviction
    /// policy that costs nothing.
    ///
    /// Evicted pages are not lost, only forgotten: a later query that needs
    /// them crawls them again at about a second each. Keeping them forever to
    /// avoid that is the wrong way round.
    pub fn trim_to(&mut self, max: usize) -> usize {
        if self.live_docs as usize <= max {
            return 0;
        }
        let mut dropped = 0;
        // Ascending ids, so oldest first.
        for id in 0..self.docs.len() as DocId {
            if self.live_docs as usize <= max {
                break;
            }
            if self.remove(id) {
                dropped += 1;
            }
        }
        if dropped > 0 {
            // Otherwise the postings of the removed documents stay, which is
            // most of what was taking the space.
            self.compact();
        }
        dropped
    }

    /// Every document containing *all* the given terms.
    ///
    /// Intersection rather than union: a query of several words is a request
    /// for pages about all of them, and a union buries those pages under every
    /// page mentioning the commonest word. Ranking sorts what this returns —
    /// it does not rescue a candidate set that is mostly noise.
    ///
    /// The rarest term is intersected first, since it bounds the work.
    pub fn candidates(&self, terms: &[String]) -> Vec<DocId> {
        if terms.is_empty() {
            return Vec::new();
        }
        let mut ordered: Vec<&String> = terms.iter().collect();
        ordered.sort_by_key(|t| self.document_frequency(t));

        let mut live: Vec<DocId> = self
            .postings(ordered[0])
            .iter()
            .map(|p| p.doc)
            .filter(|&d| self.document(d).is_some())
            .collect();

        for term in &ordered[1..] {
            let with: std::collections::HashSet<DocId> =
                self.postings(term).iter().map(|p| p.doc).collect();
            live.retain(|d| with.contains(d));
            if live.is_empty() {
                break;
            }
        }
        live.sort_unstable();
        live
    }

    /// Every document containing at least `least` of the given terms, paired
    /// with how many it contains.
    ///
    /// The widening companion to [`candidates`](Self::candidates), for when a
    /// strict reading of the query finds nothing. Measured need: a crawl of
    /// neuroscience pages answered nothing for `Q10 temperature coefficient
    /// neuron`, because the page defining the Q10 coefficient does not happen
    /// to contain the word "neuron" — three of four terms, and no result. A
    /// question asked in more words than the corpus uses is the normal case,
    /// not a malformed query.
    ///
    /// The count comes back with each document because ranking needs it:
    /// `Features::coverage` demotes a partial match in proportion to how much
    /// of the query it missed, so a document with everything still outranks
    /// one with half. Without that, widening would just be noise.
    ///
    /// Costs a pass over the postings of every term rather than stopping at
    /// the rarest, which is the price of not requiring all of them.
    pub fn candidates_at_least(&self, terms: &[String], least: usize) -> Vec<(DocId, usize)> {
        if terms.is_empty() || least == 0 {
            return Vec::new();
        }
        let mut counts: std::collections::HashMap<DocId, usize> =
            std::collections::HashMap::new();
        // Distinct terms only, or a query repeating a word would count it
        // twice and clear the threshold on its own.
        let mut seen = std::collections::HashSet::new();
        for term in terms {
            if !seen.insert(term.as_str()) {
                continue;
            }
            for posting in self.postings(term) {
                *counts.entry(posting.doc).or_insert(0) += 1;
            }
        }
        let mut live: Vec<(DocId, usize)> = counts
            .into_iter()
            .filter(|&(doc, n)| n >= least && self.document(doc).is_some())
            .collect();
        live.sort_unstable();
        live
    }

    /// The positions of `term` in `doc`, or an empty slice.
    pub fn positions(&self, term: &str, doc: DocId) -> &[u32] {
        self.postings(term)
            .iter()
            .find(|p| p.doc == doc)
            .map(|p| p.positions.as_slice())
            .unwrap_or(&[])
    }

    /// Every indexed URL, for a crawler deciding what it has already seen.
    pub fn urls(&self) -> impl Iterator<Item = &str> {
        self.docs.iter().filter(|d| d.live).map(|d| d.url.as_str())
    }

    pub fn contains_url(&self, url: &str) -> bool {
        self.by_url.contains_key(url)
    }
}

/// The share of `text`, as a percentage, in lines long enough to be prose.
///
/// See [`Document::prose_share`] for why this is worth knowing and what the
/// measured values look like.
fn prose_share(text: &str) -> u8 {
    const PROSE_LINE: usize = 120;
    let mut total = 0usize;
    let mut prose = 0usize;
    for line in text.lines() {
        let n = line.trim().len();
        total += n;
        if n >= PROSE_LINE {
            prose += n;
        }
    }
    if total == 0 {
        return 0;
    }
    ((prose * 100) / total).min(100) as u8
}

/// Cut `s` to at most `max` bytes without splitting a character.
fn truncate_on_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ── On-disk form ────────────────────────────────────────────────────────────
//
// Written by hand rather than by a serialisation crate, for the reason the
// whole crate has no dependencies. The format is deliberately dull: a magic
// number, a version, then counted records. Everything is little-endian and
// length-prefixed, so a reader never has to guess and a truncated file fails
// at a length check rather than by running off the end.
//
// The version exists because the ceiling in this module's own documentation
// says the in-memory design will eventually stop being enough. When that
// happens, this byte is how a new reader recognises an old file.

const MAGIC: &[u8; 8] = b"FRGSRCH1";
const VERSION: u32 = 3;

impl Index {
    /// Write the index to `path`.
    ///
    /// Only live documents are written, so saving is also how the file gets
    /// compacted — a long-running crawler's index does not accumulate the
    /// pages it has replaced.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        put_u32(&mut out, VERSION);

        let live: Vec<&Document> = self.docs.iter().filter(|d| d.live).collect();
        // New ids, dense, in the order written — the reader rebuilds postings
        // from the documents' own text, so nothing else has to agree.
        put_u32(&mut out, live.len() as u32);
        for doc in live {
            put_str(&mut out, &doc.url);
            put_str(&mut out, &doc.title);
            put_str(&mut out, &doc.description);
            put_str(&mut out, &doc.text);
            put_str(&mut out, &doc.attribution);
            put_u32(&mut out, doc.prose_share as u32);
        }
        std::fs::write(path, out).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// Read an index written by [`Index::save`].
    ///
    /// Postings are rebuilt from the stored text rather than stored themselves.
    /// That trades a little load time for a file that cannot be internally
    /// inconsistent: there is no way for the postings on disk to disagree with
    /// the documents, because there are no postings on disk.
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes =
            std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if bytes.len() < 12 || &bytes[..8] != MAGIC {
            return Err("not a forge-search index".into());
        }
        let mut at = 8;
        let version = take_u32(&bytes, &mut at)?;
        if version != VERSION {
            return Err(format!(
                "index version {version} is not readable by this build (expected {VERSION})"
            ));
        }
        let count = take_u32(&bytes, &mut at)?;
        let mut index = Index::new();
        for _ in 0..count {
            let url = take_str(&bytes, &mut at)?;
            let title = take_str(&bytes, &mut at)?;
            let description = take_str(&bytes, &mut at)?;
            let text = take_str(&bytes, &mut at)?;
            let attribution = take_str(&bytes, &mut at)?;
            let share = take_u32(&bytes, &mut at)?;
            let id = index.add_attributed(&url, &title, &description, &text, &attribution);
            // Recomputing would measure the capped text, which is a different
            // number — see `Document::prose_share`. The stored one is kept.
            if let Some(doc) = index.docs.get_mut(id as usize) {
                doc.prose_share = share.min(100) as u8;
            }
        }
        Ok(index)
    }
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn take_u32(bytes: &[u8], at: &mut usize) -> Result<u32, String> {
    if *at + 4 > bytes.len() {
        return Err("index file ends mid-number".into());
    }
    let v = u32::from_le_bytes([bytes[*at], bytes[*at + 1], bytes[*at + 2], bytes[*at + 3]]);
    *at += 4;
    Ok(v)
}

fn take_str(bytes: &[u8], at: &mut usize) -> Result<String, String> {
    let len = take_u32(bytes, at)? as usize;
    if *at + len > bytes.len() {
        return Err("index file ends mid-string".into());
    }
    let s = std::str::from_utf8(&bytes[*at..*at + len])
        .map_err(|_| "index file holds invalid UTF-8".to_string())?
        .to_string();
    *at += len;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed() -> Index {
        let mut ix = Index::new();
        ix.add(
            "https://a.example/no-std",
            "Writing no_std Rust",
            "A guide",
            "In no_std builds there is no allocator unless you provide one.",
        );
        ix.add(
            "https://b.example/alloc",
            "Allocators",
            "",
            "An allocator manages memory. A no_std crate may bring its own allocator.",
        );
        ix.add(
            "https://c.example/unrelated",
            "Gardening",
            "",
            "Tomatoes need sunshine and water.",
        );
        ix
    }

    #[test]
    fn a_term_finds_the_documents_holding_it() {
        let ix = indexed();
        let hits = ix.candidates(&["allocator".to_string()]);
        assert_eq!(hits.len(), 2, "got {hits:?}");
        let urls: Vec<&str> = hits.iter().map(|&d| ix.document(d).unwrap().url.as_str()).collect();
        assert!(urls.iter().all(|u| u.contains("a.example") || u.contains("b.example")));
    }

    /// Several words is a request for pages about all of them. A union would
    /// bury those pages under everything mentioning the commonest word.
    #[test]
    fn several_terms_intersect_rather_than_union() {
        let ix = indexed();
        let both = ix.candidates(&["no_std".to_string(), "allocator".to_string()]);
        assert_eq!(both.len(), 2);
        let none = ix.candidates(&["allocator".to_string(), "tomatoes".to_string()]);
        assert!(none.is_empty(), "unrelated terms matched: {none:?}");
    }

    /// A page whose title matches is findable even when its body never says
    /// the words — being absent from the index is not something ranking can
    /// correct later.
    #[test]
    fn title_words_are_indexed() {
        let mut ix = Index::new();
        ix.add("https://x.example", "Gardening Almanac", "", "Tomatoes and sunshine.");
        assert_eq!(ix.candidates(&["almanac".to_string()]).len(), 1);
    }

    #[test]
    fn a_missing_term_finds_nothing() {
        let ix = indexed();
        assert!(ix.candidates(&["nonexistentterm".to_string()]).is_empty());
        assert!(ix.candidates(&[]).is_empty());
    }

    /// Re-crawling is the normal case: the same URL replaces its entry rather
    /// than appearing twice.
    #[test]
    fn re_adding_a_url_replaces_it() {
        let mut ix = Index::new();
        ix.add("https://x.example", "Old title", "", "obsolete words here");
        ix.add("https://x.example", "New title", "", "fresh words here");
        assert_eq!(ix.len(), 1, "the URL was indexed twice");
        assert!(ix.candidates(&["obsolete".to_string()]).is_empty(), "stale text still matches");
        assert_eq!(ix.candidates(&["fresh".to_string()]).len(), 1);
        assert_eq!(ix.document(ix.candidates(&["fresh".to_string()])[0]).unwrap().title, "New title");
    }

    /// A removed document stops matching, and stops counting towards the
    /// figures ranking depends on.
    #[test]
    fn removing_a_document_hides_it_and_corrects_the_counts() {
        let mut ix = indexed();
        let before = ix.average_length();
        let victim = ix.candidates(&["tomatoes".to_string()])[0];
        assert!(ix.remove(victim));
        assert_eq!(ix.len(), 2);
        assert!(ix.candidates(&["tomatoes".to_string()]).is_empty());
        assert!(ix.document(victim).is_none());
        assert_ne!(ix.average_length(), before, "average length ignored the removal");
        // Removing twice is not an error and does not double-count.
        assert!(!ix.remove(victim));
        assert_eq!(ix.len(), 2);
    }

    /// Document frequency decides how much a term is worth, so it must not
    /// count documents that have been removed.
    #[test]
    fn document_frequency_ignores_dead_documents() {
        let mut ix = indexed();
        assert_eq!(ix.document_frequency("allocator"), 2);
        let victim = ix.candidates(&["allocator".to_string()])[0];
        ix.remove(victim);
        assert_eq!(ix.document_frequency("allocator"), 1, "a dead document was counted");
    }

    #[test]
    fn compacting_drops_the_dead_and_keeps_the_living() {
        let mut ix = indexed();
        let victim = ix.candidates(&["tomatoes".to_string()])[0];
        ix.remove(victim);
        assert_eq!(ix.compact(), 1);
        assert_eq!(ix.len(), 2);
        assert_eq!(ix.candidates(&["allocator".to_string()]).len(), 2);
        assert_eq!(ix.compact(), 0, "compacting twice found more to do");
    }

    /// Positions are what a phrase query needs.
    #[test]
    fn positions_are_recorded_in_order() {
        let mut ix = Index::new();
        let id = ix.add("https://x.example", "", "", "alpha beta alpha gamma alpha");
        let p = ix.positions("alpha", id);
        assert_eq!(p, &[0, 2, 4]);
        assert_eq!(ix.positions("beta", id), &[1]);
        assert!(ix.positions("missing", id).is_empty());
    }

    #[test]
    fn an_index_survives_a_round_trip_to_disk() {
        let dir = std::env::temp_dir().join(format!("forge-search-io-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.bin");

        let ix = indexed();
        ix.save(&path).expect("save");
        let back = Index::load(&path).expect("load");

        assert_eq!(back.len(), ix.len());
        assert_eq!(
            back.candidates(&["no_std".to_string(), "allocator".to_string()]).len(),
            2,
            "a query that worked before the round trip does not after it"
        );
        let id = back.candidates(&["tomatoes".to_string()])[0];
        let doc = back.document(id).unwrap();
        assert_eq!(doc.url, "https://c.example/unrelated");
        assert_eq!(doc.title, "Gardening");
        assert!(doc.text.contains("Tomatoes"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Saving is also compacting: a crawler that has replaced many pages does
    /// not carry them forever.
    #[test]
    fn saving_omits_dead_documents() {
        let dir = std::env::temp_dir().join(format!("forge-search-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.bin");

        let mut ix = indexed();
        let victim = ix.candidates(&["tomatoes".to_string()])[0];
        ix.remove(victim);
        ix.save(&path).unwrap();

        let back = Index::load(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert!(back.candidates(&["tomatoes".to_string()]).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt or foreign file is refused with a reason, not by panicking —
    /// this reads a file from disk that anything could have written.
    #[test]
    fn a_damaged_file_is_refused_rather_than_fatal() {
        let dir = std::env::temp_dir().join(format!("forge-search-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("not ours", b"some other file entirely".to_vec()),
            ("magic only", MAGIC.to_vec()),
            ("truncated count", [MAGIC.as_slice(), &1u32.to_le_bytes()].concat()),
            (
                "length runs past the end",
                [MAGIC.as_slice(), &1u32.to_le_bytes(), &1u32.to_le_bytes(), &9999u32.to_le_bytes()].concat(),
            ),
        ];
        for (name, bytes) in cases {
            let p = dir.join(name.replace(' ', "_"));
            std::fs::write(&p, &bytes).unwrap();
            assert!(Index::load(&p).is_err(), "{name:?} was accepted");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A future format is refused by version rather than misread — the whole
    /// reason the version is in the header.
    #[test]
    fn a_newer_version_is_refused_by_name() {
        let dir = std::env::temp_dir().join(format!("forge-search-ver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("future.bin");
        std::fs::write(&p, [MAGIC.as_slice(), &99u32.to_le_bytes(), &0u32.to_le_bytes()].concat()).unwrap();

        let err = Index::load(&p).expect_err("should refuse");
        assert!(err.contains("version 99"), "unhelpful: {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The figures ranking depends on, on an empty index. `average_length` is
    /// a divisor, so it must never be zero.
    #[test]
    fn an_empty_index_is_safe_to_rank_against() {
        let ix = Index::new();
        assert!(ix.is_empty());
        assert_eq!(ix.len(), 0);
        assert_eq!(ix.average_length(), 1.0);
        assert_eq!(ix.document_frequency("anything"), 0);
        assert!(ix.candidates(&["anything".to_string()]).is_empty());
    }

    /// The text kept for snippets is capped, and cutting it must not split a
    /// character.
    /// A cache with no bound is a leak, and this one leaks into somebody's
    /// project directory.
    #[test]
    fn trimming_drops_the_oldest_and_reclaims_the_space() {
        let mut ix = Index::new();
        for i in 0..10 {
            ix.add(&format!("https://a.test/{i}"), "", "", &format!("page number {i} content"));
        }
        assert_eq!(ix.trim_to(4), 6);
        assert_eq!(ix.len(), 4);
        let left: Vec<String> = ix.urls().map(str::to_string).collect();
        for gone in 0..6 {
            assert!(
                !left.iter().any(|u| u.ends_with(&format!("/{gone}"))),
                "page {gone} survived: {left:?}",
            );
        }
        for kept in 6..10 {
            assert!(left.iter().any(|u| u.ends_with(&format!("/{kept}"))), "{left:?}");
        }
        // And the postings went with them, which is where the space was.
        assert_eq!(ix.document_frequency("0"), 0, "an evicted page left its postings behind");
        assert!(ix.document_frequency("9") > 0, "a surviving page lost its postings");
    }

    #[test]
    fn trimming_an_index_already_small_enough_does_nothing() {
        let mut ix = Index::new();
        ix.add("https://a.test/1", "", "", "text");
        assert_eq!(ix.trim_to(10), 0);
        assert_eq!(ix.len(), 1);
    }

    /// The surviving documents must still be searchable — a trim that leaves
    /// an index that cannot answer is worse than no trim.
    #[test]
    fn an_index_still_answers_after_a_trim() {
        let mut ix = Index::new();
        for i in 0..20 {
            ix.add(&format!("https://a.test/{i}"), "Scaling", "", "minReplicas controls the replica count");
        }
        ix.trim_to(5);
        let hits = crate::query::search(&ix, "minreplicas replica", 10);
        assert!(!hits.is_empty(), "the trimmed index answers nothing");
        assert!(hits.len() <= 5);
    }

    /// Prose share is stored rather than recomputed on load, because the two
    /// are different numbers: it is measured over the whole text, and only the
    /// capped head survives a save. Recomputing would measure the navigation.
    #[test]
    fn prose_share_survives_a_round_trip_and_is_not_recomputed() {
        let mut ix = Index::new();
        let body = "A sentence long enough to count as prose, repeated so the text \
                    exceeds the snippet cap and the head is not representative. "
            .repeat(900);
        let id = ix.add("https://a.test/long", "Long", "", &body);
        let before = ix.document(id).unwrap().prose_share;
        assert!(before > 50, "fixture is not prose-shaped: {before}");
        assert!(ix.document(id).unwrap().text.len() < body.len(), "fixture was not capped");

        let dir = std::env::temp_dir().join(format!("forge-search-prose-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.bin");
        ix.save(&path).expect("save");
        let back = Index::load(&path).expect("load");
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(back.document(0).unwrap().prose_share, before, "prose share changed across a save");
    }

    /// An attribution that does not survive a save is no attribution at all:
    /// the obligation attaches to the text, and the text persists.
    #[test]
    fn attribution_survives_a_round_trip() {
        let mut ix = Index::new();
        ix.add_attributed(
            "https://europepmc.org/article/MED/1",
            "A paper",
            "",
            "Recordings were made at thirty two degrees.",
            "cc by-nc — Europe PMC, PMC1234567",
        );
        ix.add("https://example.com/page", "A page", "", "No stated terms.");
        let dir = std::env::temp_dir().join(format!("forge-search-attr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("index.bin");
        ix.save(&path).expect("save");
        let back = Index::load(&path).expect("load");
        std::fs::remove_dir_all(&dir).ok();
        let of = |ix: &Index, url: &str| -> String {
            (0..ix.len() as DocId)
                .filter_map(|d| ix.document(d))
                .find(|d| d.url == url)
                .map(|d| d.attribution.clone())
                .expect("document missing after a round trip")
        };
        assert_eq!(of(&back, "https://europepmc.org/article/MED/1"), "cc by-nc — Europe PMC, PMC1234567");
        assert_eq!(of(&back, "https://example.com/page"), "", "a source with no stated terms should stay empty");
    }

    #[test]
    fn stored_text_is_capped_without_splitting_a_character() {
        let mut ix = Index::new();
        let long = "日本語のテキスト ".repeat(4000);
        let id = ix.add("https://x.example", "", "", &long);
        let doc = ix.document(id).unwrap();
        assert!(doc.text.len() <= TEXT_KEPT, "text is {} bytes", doc.text.len());
        assert!(std::str::from_utf8(doc.text.as_bytes()).is_ok());
        assert!(doc.text.len() < long.len(), "the text was not capped at all");
        // Ranking still sees the whole document, not the kept part. Stated as
        // an equality against the full text rather than as a ratio against the
        // kept text: a ratio has to be recalibrated whenever `TEXT_KEPT`
        // changes, and an assertion that needs tuning to keep passing is not
        // asserting much.
        assert_eq!(
            doc.term_count as usize,
            crate::tokenize::terms(&long).len(),
            "term count came from the {} bytes kept for snippets, not the whole document",
            doc.text.len(),
        );
    }
}
