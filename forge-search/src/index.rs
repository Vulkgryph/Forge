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
    /// The readable text, when it is in memory.
    ///
    /// `Some` for a document just added, or one whose text has been read back.
    /// `None` for a document that came from a file and has not been asked for
    /// — which is the ordinary case, and the point.
    ///
    /// Not loaded with the index because almost nothing needs it. A query
    /// needs postings; text is needed only to cut a snippet around a match, so
    /// for ten results out of fifty thousand documents. Loading all of it to
    /// answer anything was a third of a 223 MB file read on every query.
    ///
    /// Read through [`Index::text`], which knows where to find it.
    text: Option<String>,
    /// Where the text is in its segment file, when it is not in memory.
    text_at: Option<(u64, u32)>,
    /// Which segment file holds that text, as an index into
    /// [`Index::segments`].
    ///
    /// Needed because the index is no longer one file. A document written in
    /// the first save stays in the first segment for as long as it lives, and
    /// its offset means nothing without knowing which file to apply it to.
    segment: u16,
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

/// Postings per block, for the per-block score bounds that let a query skip
/// work.
///
/// A hundred and twenty-eight, which is what Lucene fixes its packed block at
/// and for the reason it gives: a smaller block means less variance in the
/// width of the integers in it, hence a smaller index, while a larger one
/// means more efficient bulk reads. The same number is its skip interval.
pub(crate) const BLOCK: usize = 128;

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
    /// Per term, the largest term frequency in each block of postings.
    ///
    /// What makes a query able to skip. For each block it gives an upper bound
    /// on what any document in that block could score for that term, so a
    /// block that cannot beat the current tenth-best result is never read —
    /// which is the difference between answering a broad query and scoring
    /// every document in the corpus to return ten of them.
    ///
    /// Computed when the index is saved rather than as documents are added.
    /// Postings for a term grow by appends, so recomputing per document would
    /// be quadratic in the length of a common term's list; computing once over
    /// the finished index is linear. Empty means "not computed", and search
    /// falls back to scoring everything — correct, just slower.
    block_max_tf: HashMap<String, Vec<u32>>,
    /// The segment files backing this index, in the order the manifest names
    /// them — which is also the order document ids were assigned. Empty for an
    /// index built in memory and never saved, whose text is all in memory
    /// anyway.
    segments: Vec<std::path::PathBuf>,
    /// How many documents are already in a segment.
    ///
    /// The watermark that makes a save append rather than rewrite. Documents
    /// below it are on disk and their bytes are never written again; a save
    /// encodes `docs[persisted..]` into a new file and adds a line to the
    /// manifest. The one case that breaks the rule is removal, which no
    /// append can express — see [`Index::save`].
    persisted: usize,
    /// URLs of documents removed since the last save that were already in a
    /// segment, waiting to be written as the new segment's tombstones.
    ///
    /// The alternative was to compact whenever anything was removed, and that
    /// turned out to undo the format: a crawler that refreshes one stale page
    /// removes one document, and a rewrite-on-removal makes that save write
    /// the whole index. A tombstone is a URL — tens of bytes to retire a page
    /// instead of hundreds of megabytes.
    ///
    /// Only for persisted documents. Removing one added since the last save
    /// needs nothing recorded: it was never written, so there is nothing on
    /// disk to contradict.
    tombstones: Vec<String>,
}

impl Document {
    /// How long the text is, whether or not it is in memory.
    pub fn text_len(&self) -> usize {
        match (&self.text, self.text_at) {
            (Some(t), _) => t.len(),
            (None, Some((_, len))) => len as usize,
            (None, None) => 0,
        }
    }
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
    /// The whole posting list for a term, for a caller walking it in blocks.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn posting_list(&self, term: &str) -> &[Posting] {
        self.postings(term)
    }

    pub fn postings(&self, term: &str) -> &[Posting] {
        // Canonicalised here rather than at every call site. This is the one
        // place every read goes through — `document_frequency`, `positions`,
        // `candidates` and the ranker all reach the postings by this method —
        // so folding here is what makes a lookup and an insertion agree
        // without each caller having to remember to do it.
        let term = crate::tokenize::canonical(term);
        self.postings.get(term.as_ref()).map(|v| v.as_slice()).unwrap_or(&[])
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
            // Re-crawling is not removing. `remove` will have queued a
            // tombstone if the old copy was on disk; the new copy is about to
            // go into a later segment, and a load already prefers the later
            // copy of a URL. Leaving the tombstone in would make a segment
            // both retire and re-add the same page, so which one won would
            // depend on the order a reader applied them in.
            self.tombstones.retain(|t| t != url);
        }
        let tokens = tokenize::tokenize(text);
        // Title terms are indexed as well, at positions after the body, so a
        // page whose *title* matches is findable even when the body never
        // repeats the words. Ranking can weight them later; being absent from
        // the index is not something ranking can fix.
        let title_tokens = tokenize::tokenize(title);
        let term_count = (tokens.len() + title_tokens.len()) as u32;

        let id = self.docs.len() as DocId;
        // Grouped under the canonical form, so the two spellings of one token
        // share a posting list instead of being unrelated terms.
        let mut grouped: HashMap<std::borrow::Cow<str>, Vec<u32>> = HashMap::new();
        for t in &tokens {
            grouped
                .entry(crate::tokenize::canonical(&t.term))
                .or_default()
                .push(t.position as u32);
        }
        let body_end = tokens.len() as u32;
        for t in &title_tokens {
            grouped
                .entry(crate::tokenize::canonical(&t.term))
                .or_default()
                .push(body_end + t.position as u32);
        }
        for (term, mut positions) in grouped {
            positions.sort_unstable();
            self.postings
                .entry(term.into_owned())
                .or_default()
                .push(Posting { doc: id, positions });
        }

        self.docs.push(Document {
            url: url.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            text: Some(truncate_on_boundary(text, TEXT_KEPT)),
            text_at: None,
            segment: 0,
            term_count,
            live: true,
            prose_share: prose_share(text),
            attribution: attribution.to_string(),
        });
        self.by_url.insert(url.to_string(), id);
        // Stale: this document's postings moved the block boundaries for
        // every term it contains. Recomputed on save.
        self.block_max_tf.clear();
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
        let persisted = (id as usize) < self.persisted;
        self.by_url.remove(&url);
        if persisted {
            // On disk, so the removal has to be recorded to survive a reload.
            self.tombstones.push(url);
        }
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
        // Documents are moved and their postings renumbered, not re-derived.
        //
        // Re-adding them would re-tokenise `doc.text`, which is capped for
        // snippets — so compacting a document longer than the cap silently
        // dropped every term past it. The same mistake the save format used to
        // make, and worse here because `trim_to` calls this, so it fired on
        // any index that outgrew its page limit.
        let mut renumbered: std::collections::HashMap<DocId, DocId> =
            std::collections::HashMap::new();
        let mut docs: Vec<Document> = Vec::with_capacity(self.live_docs as usize);
        for (old, doc) in self.docs.iter().enumerate() {
            if doc.live {
                renumbered.insert(old as DocId, docs.len() as DocId);
                docs.push(doc.clone());
            }
        }

        let mut postings: HashMap<String, Vec<Posting>> = HashMap::new();
        for (term, list) in &self.postings {
            let kept: Vec<Posting> = list
                .iter()
                .filter_map(|p| {
                    renumbered.get(&p.doc).map(|&doc| Posting {
                        doc,
                        positions: p.positions.clone(),
                    })
                })
                .collect();
            // A term only the dead documents had goes with them, which is the
            // space this is reclaiming.
            if !kept.is_empty() {
                postings.insert(term.clone(), kept);
            }
        }

        // Nothing dead is left, so nothing needs retiring — and the next save
        // writes every surviving document into one fresh segment anyway.
        self.tombstones.clear();

        self.by_url = docs
            .iter()
            .enumerate()
            .map(|(id, d)| (d.url.clone(), id as DocId))
            .collect();
        self.total_terms = docs.iter().map(|d| d.term_count as u64).sum();
        self.live_docs = docs.len() as u32;
        self.docs = docs;
        self.postings = postings;
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

    /// Upper bounds per block of `term`'s postings, or empty when not
    /// computed.
    ///
    /// Not yet read by a query — the strategy that skips blocks is not
    /// written. Kept because the bounds are the part that has to be in the
    /// file format, and computing them later would mean rewriting every index
    /// again.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn block_max_tf(&self, term: &str) -> &[u32] {
        let term = crate::tokenize::canonical(term);
        self.block_max_tf
            .get(term.as_ref())
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// Compute the per-block bounds, so later searches can skip.
    ///
    /// Linear in the total number of postings. Called by `save`, and callable
    /// directly by anything that builds an index in memory and wants to query
    /// it quickly without a round trip through a file.
    pub fn compute_block_bounds(&mut self) {
        self.block_max_tf.clear();
        for (term, postings) in &self.postings {
            let bounds: Vec<u32> = postings
                .chunks(BLOCK)
                .map(|block| {
                    block
                        .iter()
                        .map(|p| p.positions.len() as u32)
                        .max()
                        .unwrap_or(0)
                })
                .collect();
            self.block_max_tf.insert(term.clone(), bounds);
        }
    }

    /// A document's text by reference, for a caller that has the document.
    fn text_of(&self, doc: &Document) -> String {
        if let Some(text) = &doc.text {
            return text.clone();
        }
        let (Some((offset, len)), Some(path)) =
            (doc.text_at, self.segments.get(doc.segment as usize))
        else {
            return String::new();
        };
        read_at(path, offset, len).unwrap_or_default()
    }

    /// A document's text, read from the file if it is not in memory.
    ///
    /// Empty when the document is gone, or when the read fails — a snippet is
    /// worth having and not worth failing a search for, and the caller has
    /// already got the result it was going to show.
    ///
    /// Not cached. The callers are the snippet for each of a handful of
    /// results, each asking once; a cache would be bookkeeping for a hit rate
    /// of zero.
    pub fn text(&self, doc: DocId) -> String {
        let Some(document) = self.document(doc) else { return String::new() };
        if let Some(text) = &document.text {
            return text.clone();
        }
        let (Some((offset, len)), Some(path)) =
            (document.text_at, self.segments.get(document.segment as usize))
        else {
            return String::new();
        };
        read_at(path, offset, len).unwrap_or_default()
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

/// The index file up to where document text begins.
///
/// The header carries the blob's absolute position, so this reads the first
/// few numbers to find it and then reads only what precedes it. For a
/// 20,000-page index that is 148 MB of postings and metadata instead of 223 MB
/// — the text is read later, per document, and only for documents something
/// actually wants.
fn read_prefix(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;

    let end = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // magic (8) + version (4) + document count (4) + text base (8)
    let mut header = [0u8; 24];
    let base = match file.read_exact(&mut header) {
        Ok(()) => u64::from_le_bytes(header[16..24].try_into().map_err(|_| "bad header")?),
        // Too short to hold a header at all. Read the whole thing and let the
        // parser say what is wrong with it — a truncated or wrong-version file
        // deserves the specific complaint the parser makes, not a generic one
        // from here.
        Err(_) => 0,
    };

    // Zero means an older file, or one too short to say; take all of it.
    let take = if base == 0 || base > end { end } else { base };

    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek {}: {e}", path.display()))?;
    let mut bytes = vec![0u8; take as usize];
    file.read_exact(&mut bytes)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(bytes)
}

/// Read `len` bytes at `offset` from `path`, as text.
///
/// One seek and one read rather than mapping the file: the reads are a few
/// kilobytes, a handful per query, and a mapping would have to be kept and
/// invalidated when the index is rewritten.
fn read_at(path: &Path, offset: u64, len: u32) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut buffer = vec![0u8; len as usize];
    file.read_exact(&mut buffer).ok()?;
    String::from_utf8(buffer).ok()
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
const VERSION: u32 = 7;

/// Compact when dead documents are more than this fraction of the index, as a
/// divisor — 4 for a quarter.
///
/// Lucene's merge policy asks the same question and lands in the same region.
/// Lower, and a maintained index rewrites itself over churn it could have
/// retired by name; higher, and the segments keep bytes nothing will ever read
/// while every query pays to skip over their postings.
const DEAD_SHARE_TO_COMPACT: usize = 4;

impl Index {
    /// Write anything not yet on disk, as a new segment.
    ///
    /// `path` is a directory. Inside it, each segment is a file written once
    /// and never modified again, and a small manifest says which segments
    /// exist and in what order.
    ///
    /// This is the shape that makes a growing index affordable. The previous
    /// format was one file rewritten in full on every save: at the fifty
    /// thousand page cap that is 556 MB written to add sixty pages, and a
    /// permanent crawler saving every sixty pages would write about 35 TB a
    /// day — a fortnight of that ends a consumer SSD. Appending writes the new
    /// pages and a manifest of a few kilobytes, so the cost of a save tracks
    /// what was added rather than what is already there.
    ///
    /// Removing a document is the exception, and deliberately so: a segment
    /// cannot be edited, so eviction rewrites everything as a single fresh
    /// segment. That is a large write, and it happens when the index is
    /// trimmed rather than every time it grows — which is the right place for
    /// it, since compaction is exactly when a full rewrite earns its cost.
    pub fn save(&mut self, path: &Path) -> Result<(), String> {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("create {}: {e}", path.display()))?;

        // A removed document cannot be taken out of the segment holding it, so
        // an append retires it by name instead and only a compaction actually
        // reclaims the space. Compacting on every removal was the first
        // attempt and it defeated the format: refreshing one stale page
        // removes one document, which would have made that save rewrite the
        // whole index.
        //
        // So: compact when enough of the index is dead to be worth the write,
        // append otherwise. At the threshold a rewrite costs a quarter more
        // than the live data it keeps, and it happens once per quarter of the
        // index turning over rather than once per page.
        let dead = self.docs.iter().filter(|d| !d.live).count();
        let rewriting = self.segments.is_empty() || dead * DEAD_SHARE_TO_COMPACT > self.docs.len();

        if rewriting {
            // In memory as well as on disk. Left in place, the dead documents
            // would keep the share above the threshold and make every
            // subsequent save a rewrite.
            self.compact();
            self.persisted = 0;
        }

        let ids: Vec<DocId> = (self.persisted as DocId..self.docs.len() as DocId)
            .filter(|&i| self.docs[i as usize].live)
            .collect();

        // Nothing added and nothing retired: leave the directory alone rather
        // than rewriting a manifest to say what it already says. A save that
        // changes nothing is the common case once a crawl has settled, and it
        // should cost nothing.
        if ids.is_empty() && self.tombstones.is_empty() && !rewriting {
            return Ok(());
        }

        // A name no existing file has, even when compacting. `encode_segment`
        // reads the text of documents out of the segments being replaced, so
        // writing over one of them would be reading and writing the same file;
        // the encode happens to complete first today, and relying on that is
        // the kind of ordering that survives until someone streams the write.
        let bytes = self.encode_segment(&ids);
        let name = format!("seg-{:05}.bin", next_segment_number(path));
        let file = path.join(&name);

        // Where the text blob starts, taken from the header the encoder just
        // back-patched — the same number a reader takes from it.
        let base = u64::from_le_bytes(
            bytes[16..24].try_into().map_err(|_| "bad segment header")?,
        );

        std::fs::write(&file, &bytes).map_err(|e| format!("write {}: {e}", file.display()))?;

        let mut names: Vec<String> = if rewriting {
            Vec::new()
        } else {
            self.segments
                .iter()
                .map(|p| {
                    p.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                })
                .collect()
        };
        names.push(name);
        write_manifest(path, &names)?;

        // Superseded segments are unlinked only after the manifest has stopped
        // naming them. A crash between the two leaves a directory that still
        // opens, with a stale file in it — whereas the other order leaves a
        // manifest naming a file that is gone, which opens as nothing at all.
        let replaced = std::mem::take(&mut self.segments);
        self.segments = names.iter().map(|n| path.join(n)).collect();
        if rewriting {
            for old in replaced {
                let _ = std::fs::remove_file(old);
            }
        }

        // Where each document just written now lives. Text is dropped from
        // memory at the same time, which is the other half of not loading it:
        // a document that has been saved can be read back a page at a time.
        let segment = (self.segments.len() - 1) as u16;
        let mut written = base;
        for &id in &ids {
            let doc = &mut self.docs[id as usize];
            let len = doc.text_len() as u32;
            doc.segment = segment;
            doc.text_at = Some((written, len));
            doc.text = None;
            written += len as u64;
        }
        self.persisted = self.docs.len();
        self.tombstones.clear();
        Ok(())
    }

    /// One segment's bytes: the given documents, with local ids, and only the
    /// postings that point at them.
    fn encode_segment(&self, ids: &[DocId]) -> Vec<u8> {
        let mut local: HashMap<DocId, DocId> = HashMap::new();
        for (n, &id) in ids.iter().enumerate() {
            local.insert(id, n as DocId);
        }

        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        put_u32(&mut out, VERSION);

        let mut text_blob: Vec<u8> = Vec::new();
        let mut spans: Vec<(u64, u32)> = Vec::with_capacity(ids.len());
        for &id in ids {
            let text = self.text_of(&self.docs[id as usize]);
            spans.push((text_blob.len() as u64, text.len() as u32));
            text_blob.extend_from_slice(text.as_bytes());
        }

        put_u32(&mut out, ids.len() as u32);
        let base_at = out.len();
        put_u64(&mut out, 0);
        for (&id, (offset, len)) in ids.iter().zip(&spans) {
            let doc = &self.docs[id as usize];
            put_str(&mut out, &doc.url);
            put_str(&mut out, &doc.title);
            put_str(&mut out, &doc.description);
            put_str(&mut out, &doc.attribution);
            put_u32(&mut out, doc.prose_share as u32);
            put_u32(&mut out, doc.term_count);
            put_u64(&mut out, *offset);
            put_u32(&mut out, *len);
        }

        // Postings, written rather than rebuilt from the text. They used to be
        // rebuilt, and that was wrong in a way only a save revealed: the text
        // kept for snippets is capped, so re-tokenising it produced postings
        // for the capped part alone. A 50 kB page lost every term past 32 kB.
        // The postings *are* the index; deriving them from a lossy copy of
        // their own source was the mistake.
        let mut terms: Vec<&String> = self.postings.keys().collect();
        terms.sort_unstable();
        // Only terms this segment's documents actually use.
        let mut kept: Vec<(&String, Vec<&Posting>)> = Vec::new();
        for term in terms {
            let mine: Vec<&Posting> = self.postings[term]
                .iter()
                .filter(|p| local.contains_key(&p.doc))
                .collect();
            if !mine.is_empty() {
                kept.push((term, mine));
            }
        }
        put_u32(&mut out, kept.len() as u32);
        for (term, mine) in kept {
            put_str(&mut out, term);
            // Bounds over this segment's own blocks, since a reader skips
            // within a segment.
            let bounds: Vec<u32> = mine
                .chunks(BLOCK)
                .map(|b| b.iter().map(|p| p.positions.len() as u32).max().unwrap_or(0))
                .collect();
            put_u32(&mut out, bounds.len() as u32);
            for bound in &bounds {
                put_u32(&mut out, *bound);
            }
            put_u32(&mut out, mine.len() as u32);
            for posting in mine {
                put_u32(&mut out, local[&posting.doc]);
                put_u32(&mut out, posting.positions.len() as u32);
                for position in &posting.positions {
                    put_u32(&mut out, *position);
                }
            }
        }

        // The pages this segment retires, by URL. Last of the record sections
        // so that a reader has already seen this segment's own documents when
        // it applies them — which only matters for the ordering to be stated
        // somewhere, since `add` makes sure a segment never both retires and
        // re-adds the same page.
        put_u32(&mut out, self.tombstones.len() as u32);
        for url in &self.tombstones {
            put_str(&mut out, url);
        }

        let base = out.len() as u64;
        out[base_at..base_at + 8].copy_from_slice(&base.to_le_bytes());
        out.extend_from_slice(&text_blob);
        out
    }

    /// Read an index written by [`save`](Self::save).
    ///
    /// `path` is the directory. Segments are read in the order the manifest
    /// names them, and document ids are assigned in that order — so an id is
    /// stable for as long as no compaction happens, and postings from each
    /// segment are offset onto it.
    pub fn load(path: &Path) -> Result<Self, String> {
        let names = read_manifest(path)?;
        let mut index = Index::new();
        for name in &names {
            let file = path.join(name);
            index.read_segment(&file)?;
            index.segments.push(file);
        }
        index.persisted = index.docs.len();
        Ok(index)
    }

    /// Add one segment's documents and postings to this index.
    fn read_segment(&mut self, path: &Path) -> Result<(), String> {
        // Only the part before the text blob: the blob's position is in the
        // header, so the header is read first and then exactly the prefix that
        // matters. Text is left on disk and read per document.
        let bytes = read_prefix(path)?;
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
        let text_base = take_u64(&bytes, &mut at)?;
        let segment = self.segments.len() as u16;
        let base = self.docs.len() as DocId;

        for _ in 0..count {
            let url = take_str(&bytes, &mut at)?;
            let title = take_str(&bytes, &mut at)?;
            let description = take_str(&bytes, &mut at)?;
            let attribution = take_str(&bytes, &mut at)?;
            let share = take_u32(&bytes, &mut at)?;
            let term_count = take_u32(&bytes, &mut at)?;
            let text_offset = take_u64(&bytes, &mut at)?;
            let text_len = take_u32(&bytes, &mut at)?;
            let id = self.docs.len() as DocId;
            // A later segment holding the same URL is the newer copy of that
            // page, so it replaces the earlier one — which is how a re-crawl
            // updates a page without any segment being edited.
            if let Some(&existing) = self.by_url.get(&url) {
                self.remove(existing);
            }
            self.docs.push(Document {
                url: url.clone(),
                title,
                description,
                text: None,
                text_at: Some((text_base + text_offset, text_len)),
                segment,
                term_count,
                live: true,
                prose_share: share.min(100) as u8,
                attribution,
            });
            self.by_url.insert(url, id);
            self.live_docs += 1;
            self.total_terms += term_count as u64;
        }

        let term_count = take_u32(&bytes, &mut at)?;
        for _ in 0..term_count {
            let term = take_str(&bytes, &mut at)?;
            let bound_count = take_u32(&bytes, &mut at)?;
            let mut bounds = Vec::with_capacity(bound_count as usize);
            for _ in 0..bound_count {
                bounds.push(take_u32(&bytes, &mut at)?);
            }
            let postings = take_u32(&bytes, &mut at)?;
            let mut list = Vec::with_capacity(postings as usize);
            for _ in 0..postings {
                let local = take_u32(&bytes, &mut at)?;
                if local >= count {
                    return Err(format!("posting for document {local}, which is not in {}", path.display()));
                }
                let n = take_u32(&bytes, &mut at)?;
                let mut positions = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    positions.push(take_u32(&bytes, &mut at)?);
                }
                list.push(Posting { doc: base + local, positions });
            }
            // Merged onto whatever earlier segments contributed. Ids rise with
            // segment order, so appending keeps each list sorted.
            self.postings.entry(term.clone()).or_default().extend(list);
            self.block_max_tf.entry(term).or_default().extend(bounds);
        }

        // Pages this segment retires. They are in an earlier segment, which
        // cannot be edited, so a load is where the removal takes effect.
        let retired = take_u32(&bytes, &mut at)?;
        for _ in 0..retired {
            let url = take_str(&bytes, &mut at)?;
            if let Some(&id) = self.by_url.get(&url) {
                self.remove(id);
            }
        }
        // Not carried forward as pending work: these are already recorded in
        // the segment just read, and re-writing them into the next one would
        // grow every segment by the whole history of what has ever been
        // removed. `persisted` is set by the caller once every segment is in.
        self.tombstones.clear();
        Ok(())
    }
}

/// The manifest's name inside an index directory.
const MANIFEST: &str = "segments.txt";

/// Write the list of segments, newest last.
///
/// Text rather than the packed form the segments use, because this is the one
/// file a person might reasonably want to read: it says what an index
/// directory is made of, and a directory whose manifest can be inspected with
/// `cat` is a directory whose state can be diagnosed without this crate.
///
/// Written to a temporary name and renamed over the old one. Rename is atomic
/// on every filesystem this runs on, so a reader either sees the whole old
/// manifest or the whole new one. A manifest half-written is an index that
/// cannot be opened at all, which is the one failure worth ruling out — a
/// segment is immutable and a stale one is harmless, but there is only ever
/// one manifest.
fn write_manifest(dir: &Path, names: &[String]) -> Result<(), String> {
    let mut text = String::new();
    text.push_str("forge-search segments 1\n");
    for name in names {
        text.push_str(name);
        text.push('\n');
    }
    let staging = dir.join("segments.txt.new");
    std::fs::write(&staging, text).map_err(|e| format!("write {}: {e}", staging.display()))?;
    std::fs::rename(&staging, dir.join(MANIFEST))
        .map_err(|e| format!("replace manifest in {}: {e}", dir.display()))
}

/// The next unused segment number in `dir`.
///
/// One past the highest already there, rather than one past the manifest's
/// length, so a name is never reused — not after a compaction that dropped
/// segments from the middle of the count, and not after a crash that left a
/// segment behind that no manifest names.
fn next_segment_number(dir: &Path) -> u32 {
    let mut highest = None;
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(digits) = name.strip_prefix("seg-").and_then(|n| n.strip_suffix(".bin")) else {
            continue;
        };
        if let Ok(n) = digits.parse::<u32>() {
            highest = Some(highest.map_or(n, |h: u32| h.max(n)));
        }
    }
    highest.map_or(0, |h| h + 1)
}

/// The segment names an index directory claims, in order.
fn read_manifest(dir: &Path) -> Result<Vec<String>, String> {
    let path = dir.join(MANIFEST);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut lines = text.lines();
    match lines.next() {
        Some("forge-search segments 1") => {}
        Some(other) => return Err(format!("{} is not a forge-search manifest: {other:?}", path.display())),
        None => return Err(format!("{} is empty", path.display())),
    }
    Ok(lines
        .map(str::trim)
        .filter(|l| !l.is_empty())
        // A name, not a path: a manifest must not be able to name a file
        // outside its own directory.
        .filter(|l| !l.contains('/') && !l.contains('\\') && *l != "." && *l != "..")
        .map(str::to_string)
        .collect())
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn take_u64(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    if *at + 8 > bytes.len() {
        return Err("index file ends mid-number".into());
    }
    let v = u64::from_le_bytes(bytes[*at..*at + 8].try_into().map_err(|_| "bad u64")?);
    *at += 8;
    Ok(v)
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

        let mut ix = indexed();
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
        assert!(ix.text(id).contains("Tomatoes"));

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
        // A manifest naming one segment from a format this build cannot read.
        // The manifest itself is readable, which is the point of keeping it in
        // its own file and its own format: the complaint is about the segment,
        // not about the directory.
        std::fs::write(
            dir.join("seg-00000.bin"),
            [MAGIC.as_slice(), &99u32.to_le_bytes(), &0u32.to_le_bytes()].concat(),
        )
        .unwrap();
        write_manifest(&dir, &["seg-00000.bin".to_string()]).unwrap();

        let err = Index::load(&dir).expect_err("should refuse");
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

    /// A document must not lose its deep terms to a save.
    ///
    /// Postings used to be rebuilt on load by re-tokenising the stored text,
    /// which is capped for snippets — so a page longer than the cap came back
    /// searchable only as far as the cap. Measured on a 50 kB page: term count
    /// fell from 6,409 to 4,097 and a word near the end went from one hit to
    /// none. The first search after a crawl found it and every later one did
    /// not, which is the worst shape a bug can have.
    #[test]
    fn a_term_past_the_snippet_cap_survives_a_save() {
        let mut ix = Index::new();
        let filler = "Filler about unrelated matters. ".repeat(1600);
        ix.add("https://a.test/long", "Long", "", &format!("{filler}thirtytwo degrees celsius"));
        let before = ix.document(0).unwrap().term_count;
        assert!(ix.document(0).unwrap().text_len() < TEXT_KEPT + 1, "fixture not capped");
        assert_eq!(ix.document_frequency("celsius"), 1);

        let dir = std::env::temp_dir().join(format!("forge-deep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("i.bin");
        ix.save(&path).unwrap();
        let back = Index::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(back.document(0).unwrap().term_count, before, "term count shrank on load");
        assert_eq!(back.document_frequency("celsius"), 1, "a term past the cap was lost");
        assert_eq!(crate::query::search(&back, "thirtytwo celsius", 3).len(), 1);
    }

    /// And must not lose them to a compaction either — which is worse, since
    /// `trim_to` compacts and fires on any index that outgrows its cap.
    #[test]
    fn a_term_past_the_snippet_cap_survives_a_compaction() {
        let mut ix = Index::new();
        let filler = "Filler about unrelated matters. ".repeat(1600);
        ix.add("https://a.test/long", "Long", "", &format!("{filler}thirtytwo degrees celsius"));
        ix.add("https://a.test/dead", "Dead", "", "this one goes away");
        let before = ix.document(0).unwrap().term_count;
        assert!(ix.remove(1));
        assert_eq!(ix.compact(), 1);

        assert_eq!(ix.len(), 1);
        assert_eq!(ix.document(0).unwrap().term_count, before, "term count shrank on compaction");
        assert_eq!(ix.document_frequency("celsius"), 1, "a term past the cap was lost");
        // And the dead document's own terms really did go.
        assert_eq!(ix.document_frequency("away"), 0);
    }

    /// A scratch directory named for the test, since tests run as threads of
    /// one process and a name keyed on the process id has already had two of
    /// them delete each other's fixtures.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("forge-seg-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The segment files in an index directory, in manifest order.
    fn segments_in(dir: &Path) -> Vec<String> {
        read_manifest(dir).unwrap()
    }

    /// The whole reason for the format: a save writes what was added, not what
    /// was already there. The earlier segment must come back byte for byte,
    /// because "append-only" is a claim about the bytes on disk and nothing
    /// less checks it.
    #[test]
    fn a_second_save_leaves_the_first_segment_untouched() {
        let dir = scratch("append");
        let mut ix = indexed();
        ix.save(&dir).unwrap();

        let first = segments_in(&dir);
        assert_eq!(first.len(), 1, "a first save should write one segment");
        let before = std::fs::read(dir.join(&first[0])).unwrap();

        ix.add("https://d.example/new", "Later", "", "A page added after the first save.");
        ix.save(&dir).unwrap();

        let after = segments_in(&dir);
        assert_eq!(after.len(), 2, "a second save should add a segment, not replace one");
        assert_eq!(after[0], first[0], "the manifest reordered or renamed a segment");
        assert_eq!(
            std::fs::read(dir.join(&first[0])).unwrap(),
            before,
            "the first segment was rewritten — the save is not append-only"
        );

        // And the second segment is the size of what was added, not of the
        // whole index. This is the number the format exists for.
        let added = std::fs::metadata(dir.join(&after[1])).unwrap().len();
        assert!(
            added < before.len() as u64,
            "adding one page wrote {added} bytes against {} already on disk",
            before.len()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Documents from every segment are searchable, with ids assigned in
    /// manifest order — a reader that stopped at the first segment would still
    /// pass a round-trip test on a freshly built index.
    #[test]
    fn a_query_reaches_documents_in_every_segment() {
        let dir = scratch("across");
        let mut ix = indexed();
        ix.save(&dir).unwrap();
        ix.add("https://d.example/threads", "Thread safety", "", "A no_std crate can still be thread safe.");
        ix.save(&dir).unwrap();

        let back = Index::load(&dir).unwrap();
        assert_eq!(back.len(), 4);
        // Three documents mention no_std, one of them from the second segment.
        assert_eq!(back.document_frequency("no_std"), 3, "postings did not merge across segments");
        let hits = crate::query::search(&back, "thread safe", 5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://d.example/threads");
        // Text lives in the second segment file; a snippet proves the offset
        // was applied to the right one.
        assert!(hits[0].snippet.contains("thread safe"), "snippet: {:?}", hits[0].snippet);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A save with nothing new must not write. Once a crawl settles this is
    /// every save, and a manifest rewritten to say what it already said is the
    /// same wear the format is meant to avoid — in miniature.
    #[test]
    fn a_save_with_nothing_new_writes_nothing() {
        let dir = scratch("idle");
        let mut ix = indexed();
        ix.save(&dir).unwrap();
        let before = segments_in(&dir);
        let stamp = std::fs::metadata(dir.join(MANIFEST)).unwrap().modified().unwrap();

        ix.save(&dir).unwrap();
        ix.save(&dir).unwrap();

        assert_eq!(segments_in(&dir), before, "an idle save added a segment");
        assert_eq!(
            std::fs::metadata(dir.join(MANIFEST)).unwrap().modified().unwrap(),
            stamp,
            "an idle save rewrote the manifest"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Filler pages, so a test can cross or stay under the compaction
    /// threshold on purpose rather than by accident. A small fixture is all
    /// dead the moment anything is removed from it.
    fn padded(n: usize) -> Index {
        let mut ix = Index::new();
        for i in 0..n {
            ix.add(
                &format!("https://pad.test/{i}"),
                &format!("Page {i}"),
                "",
                &format!("Filler page number {i} about nothing in particular."),
            );
        }
        ix
    }

    /// Removing a page must not rewrite the index. This is the case that
    /// defeated the first attempt: a crawler refreshing one stale page removes
    /// one document, and compacting on removal made that save write everything.
    /// The page is retired by name instead.
    #[test]
    fn removing_a_page_is_retired_by_name_not_by_rewriting() {
        let dir = scratch("retire");
        let mut ix = padded(8);
        ix.save(&dir).unwrap();
        let first = segments_in(&dir);
        let before = std::fs::read(dir.join(&first[0])).unwrap();

        let gone = ix.by_url["https://pad.test/3"];
        assert!(ix.remove(gone));
        ix.save(&dir).unwrap();

        let names = segments_in(&dir);
        assert_eq!(names.len(), 2, "a removal should append, not compact");
        assert_eq!(
            std::fs::read(dir.join(&first[0])).unwrap(),
            before,
            "the segment holding the removed page was rewritten"
        );
        // A tombstone is a URL. The segment carrying one is tiny next to the
        // index it retires a page from.
        let tomb = std::fs::metadata(dir.join(&names[1])).unwrap().len();
        assert!(tomb < 200, "retiring one page wrote {tomb} bytes");

        let back = Index::load(&dir).unwrap();
        assert_eq!(back.len(), 7);
        assert!(!back.contains_url("https://pad.test/3"), "the removal did not survive a reload");
        assert!(back.contains_url("https://pad.test/4"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tombstones are not free forever, so past a threshold the index is
    /// rewritten and the space actually comes back — and the superseded files
    /// go with it rather than accumulating.
    #[test]
    fn enough_dead_pages_trigger_a_compaction() {
        let dir = scratch("threshold");
        let mut ix = padded(8);
        ix.save(&dir).unwrap();
        // Two of eight is a quarter, which is not more than a quarter.
        for i in [0usize, 1] {
            let id = ix.by_url[&format!("https://pad.test/{i}")];
            assert!(ix.remove(id));
        }
        ix.save(&dir).unwrap();
        assert_eq!(segments_in(&dir).len(), 2, "a quarter dead should still append");

        // The third crosses it.
        let id = ix.by_url["https://pad.test/2"];
        assert!(ix.remove(id));
        ix.save(&dir).unwrap();

        let names = segments_in(&dir);
        assert_eq!(names.len(), 1, "past the threshold the index should be rewritten");
        let left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("seg-"))
            .collect();
        assert_eq!(left, names, "superseded segments were left behind: {left:?}");

        let back = Index::load(&dir).unwrap();
        assert_eq!(back.len(), 5);
        for i in 0..3 {
            assert!(!back.contains_url(&format!("https://pad.test/{i}")));
        }
        assert!(back.contains_url("https://pad.test/7"));

        // And a compaction leaves nothing pending, so the next idle save is
        // still free.
        let after = std::fs::metadata(dir.join(MANIFEST)).unwrap().modified().unwrap();
        ix.save(&dir).unwrap();
        assert_eq!(
            std::fs::metadata(dir.join(MANIFEST)).unwrap().modified().unwrap(),
            after,
            "a save straight after a compaction wrote again"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Re-crawling a page puts a newer copy in a later segment while the older
    /// one stays where it is. The load has to prefer the later copy, or an
    /// index would answer from a page it has already replaced.
    #[test]
    fn a_later_segment_supersedes_an_earlier_copy_of_the_same_page() {
        let dir = scratch("recrawl");
        // Padded, so replacing one page does not by itself put a quarter of
        // the index out of date and trigger a compaction.
        let mut ix = padded(8);
        ix.add("https://a.test/spec", "Spec", "", "The limit is forty units.");
        ix.save(&dir).unwrap();

        // Append-only on disk, so the older copy is still in segment zero.
        ix.add("https://a.test/spec", "Spec", "", "The limit is ninety units.");
        ix.save(&dir).unwrap();
        assert_eq!(segments_in(&dir).len(), 2);

        let back = Index::load(&dir).unwrap();
        assert_eq!(back.len(), 9, "both copies of the page are live");
        assert_eq!(back.document_frequency("ninety"), 1);
        assert_eq!(back.document_frequency("forty"), 0, "the superseded copy still answers");
        let hits = crate::query::search(&back, "limit units", 3);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("ninety"), "snippet: {:?}", hits[0].snippet);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A manifest must not be able to name a file outside its own directory.
    /// It is a plain text file so that it can be read; that also means it can
    /// be edited, and by something other than a person.
    #[test]
    fn a_manifest_cannot_escape_its_directory() {
        let dir = scratch("escape");
        let mut ix = indexed();
        ix.save(&dir).unwrap();
        let real = segments_in(&dir).remove(0);
        std::fs::write(
            dir.join(MANIFEST),
            format!("forge-search segments 1\n../../etc/passwd\n..\n.\n{real}\n"),
        )
        .unwrap();

        assert_eq!(read_manifest(&dir).unwrap(), vec![real]);
        assert_eq!(Index::load(&dir).unwrap().len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory with no manifest is not an index, and says so rather than
    /// opening as an empty one — an empty index is indistinguishable from a
    /// crawl that found nothing, and silently starting over is how a save then
    /// overwrites what was there.
    #[test]
    fn a_directory_without_a_manifest_is_not_an_index() {
        let dir = scratch("bare");
        assert!(Index::load(&dir).is_err());
        std::fs::write(dir.join(MANIFEST), "some other program's file\n").unwrap();
        let err = Index::load(&dir).expect_err("should refuse");
        assert!(err.contains("not a forge-search manifest"), "unhelpful: {err}");
        let _ = std::fs::remove_dir_all(&dir);
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
        assert!(ix.document(id).unwrap().text_len() < body.len(), "fixture was not capped");

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
        assert!(doc.text_len() <= TEXT_KEPT, "text is {} bytes", doc.text_len());
        assert!(std::str::from_utf8(ix.text(id).as_bytes()).is_ok());
        assert!(doc.text_len() < long.len(), "the text was not capped at all");
        // Ranking still sees the whole document, not the kept part. Stated as
        // an equality against the full text rather than as a ratio against the
        // kept text: a ratio has to be recalibrated whenever `TEXT_KEPT`
        // changes, and an assertion that needs tuning to keep passing is not
        // asserting much.
        assert_eq!(
            doc.term_count as usize,
            crate::tokenize::terms(&long).len(),
            "term count came from the {} bytes kept for snippets, not the whole document",
            doc.text_len(),
        );
    }
}
