//! Ranking: which of the matching documents to show first.
//!
//! The scoring function is BM25, which is the standard for a reason worth
//! stating rather than deferring to. Three things make a term's appearance in
//! a document meaningful, and BM25 is the arrangement that handles all three:
//!
//! - **How often the term appears**, but with diminishing returns. A page
//!   saying "allocator" forty times is not forty times more about allocators
//!   than one saying it once; it is perhaps three times more about them.
//!   Raw counts reward keyword stuffing, which is what BM25's saturation
//!   prevents.
//! - **How rare the term is.** A query of "the allocator" is a question about
//!   allocators. `the` appears everywhere and therefore distinguishes nothing,
//!   so it must contribute almost nothing — earned from the corpus rather than
//!   asserted by a stop list.
//! - **How long the document is.** A long page contains more of every word by
//!   accident, so term counts are compared against the average length. Without
//!   this, ranking prefers whichever page is longest.
//!
//! On top of the score, three signals that BM25 has no way to see: whether the
//! match is in the title, whether the query's words appear *together*, and how
//! short the URL is. Each is a separate, named contribution rather than a
//! tweak to the score, because [`Features`] is also the feature vector a
//! learned ranker would be trained on — and a feature buried inside a formula
//! cannot be weighted independently later.

use crate::index::{DocId, Index};
use crate::tokenize;

/// Saturation: how quickly repeated occurrences stop adding.
///
/// The standard 1.2. Higher lets frequency matter more, and at zero the score
/// ignores frequency entirely and becomes pure rarity.
const K1: f64 = 1.2;

/// How much document length is corrected for, from 0 (not at all) to 1
/// (fully). The standard 0.75 — full correction over-penalises long reference
/// pages, which in a documentation corpus are often the best answer.
const B: f64 = 0.75;

/// What a title match is worth, as a multiple of the body score.
///
/// A page titled "Writing no_std Rust" is more about `no_std` than one
/// mentioning it in paragraph nine, and no amount of term frequency expresses
/// that — the title is a claim the author made about the whole page.
const TITLE_WEIGHT: f64 = 2.5;

/// What it is worth for the query's words to appear adjacently.
///
/// "no_std allocator" asks about a thing, not about two words. A page with the
/// phrase is answering the question; a page with both words in different
/// sections may only be mentioning them.
const PHRASE_WEIGHT: f64 = 3.0;

/// Why the individual contributions are kept rather than summed away.
///
/// This is both the explanation of a result — useful when a search returns
/// something surprising and someone has to work out why — and the feature
/// vector a trainable ranker learns weights for. Collapsing them into one
/// number early would make the second impossible.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Features {
    /// BM25 over the document's terms.
    pub bm25: f64,
    /// BM25 restricted to terms appearing in the title.
    pub title: f64,
    /// 1.0 when every query term appears consecutively, scaled down for
    /// looser proximity, 0.0 when they are far apart or there is one term.
    pub phrase: f64,
    /// Short URLs are usually closer to the root of a site and more
    /// authoritative than deeply nested ones — `docs/std/vec` over
    /// `blog/2019/04/17/comments/page/3`. A weak signal, weighted as such.
    pub url_depth: f64,
    /// How many of the query's terms the document contains at all. A document
    /// missing a term is a worse answer than one containing all of them even
    /// when its score is higher, which pure BM25 does not guarantee.
    pub coverage: f64,
    /// How much of the document is prose rather than a list of links, from 0.0
    /// to 1.0. See [`Document::prose_share`](crate::index::Document::prose_share).
    pub prose: f64,
}

impl Features {
    /// Combine into one number, with the weights above.
    ///
    /// Written out rather than as a dot product so the arithmetic is visible:
    /// when a ranker is trained later, this is the function whose constants
    /// become learned parameters.
    pub fn score(&self) -> f64 {
        let base = self.bm25
            + self.title * TITLE_WEIGHT
            + self.phrase * PHRASE_WEIGHT
            + self.url_depth;
        // Coverage multiplies rather than adds. A document missing half the
        // query must not outrank a complete match by accumulating frequency on
        // the terms it does have, and addition allows exactly that.
        //
        // Once it was always 1.0, because the candidate set was an
        // intersection and every candidate held every term. Widening to near
        // misses is what gave it work to do.
        //
        // Prose share multiplies too, and for a related reason: a page that is
        // a list of links to answers accumulates title and term matches
        // without containing an answer, and adding a penalty lets a long
        // enough list out-accumulate a short real one. The floor keeps it a
        // demotion rather than an exclusion — a specification table is mostly
        // not prose and is still sometimes the right result.
        base * self.coverage * (PROSE_FLOOR + (1.0 - PROSE_FLOOR) * self.prose)
    }
}

/// One ranked result.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub doc: DocId,
    pub score: f64,
    pub features: Features,
}

/// Inverse document frequency: what one term is worth.
///
/// The `+0.5`/`+1.0` form is BM25's, and it matters here: the textbook IDF goes
/// *negative* for a term in more than half the corpus, which would let a
/// document be penalised for containing a query word. This form stays positive.
pub(crate) fn idf(index: &Index, term: &str) -> f64 {
    let n = index.len() as f64;
    let df = index.document_frequency(term) as f64;
    (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
}

/// The most a document can be demoted for being a list rather than a page.
///
/// At 0.3 a board index keeps a third of its score and a thread keeps about
/// six tenths of its own — enough to reorder them without deciding that a page
/// of links is never what anyone wanted.
const PROSE_FLOOR: f64 = 0.3;

/// Below this many terms, a document is not judged on its shape at all.
///
/// The measure asks whether a page's text is dominated by short fragments
/// *despite having plenty of text*, which is what a list of links looks like.
/// A short page is not a list, it is short — and the first version of this
/// missed the distinction and broke length normalisation with it: a concise
/// answer of a few words has no line long enough to count as prose, so it
/// scored zero and was demoted to the floor, while a document padded with a
/// single nine-kilobyte line of filler scored a perfect hundred. The existing
/// test for padding caught it, which is the only reason it is not still here.
///
/// Roughly a kilobyte of text. Below it the feature is neutral, and BM25's
/// length normalisation is left to do the job it already does.
const MIN_TERMS_TO_JUDGE_SHAPE: u32 = 150;

/// Score and sort the documents matching `query`.
///
/// Returns at most `limit`, best first. The candidate set comes from the
/// index's intersection, so this ranks documents that contain every term.
pub fn search(index: &Index, query: &str, limit: usize) -> Vec<Hit> {
    search_within(index, query, limit, |_| true)
}

/// As [`search`], over the documents `keep` accepts.
///
/// The filter is applied to the candidate set, before scoring and before the
/// limit — not to the results afterwards. Filtering afterwards is the obvious
/// implementation and it is wrong: the limit would already have thrown away
/// the wanted documents to make room for ones about to be discarded, so
/// narrowing to a site the index holds fifty pages from could return nothing
/// at all because a larger site filled the list first.
pub fn search_within(
    index: &Index,
    query: &str,
    limit: usize,
    keep: impl Fn(crate::index::DocId) -> bool,
) -> Vec<Hit> {
    let terms: Vec<String> = tokenize::terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut candidates = index.candidates(&terms);
    candidates.retain(|&doc| keep(doc));
    if candidates.len() < limit && terms.len() > 1 {
        // A strict reading came up short, so accept documents missing some of
        // the query. `coverage` multiplies the score by the fraction found, so
        // these sort below anything that matched in full — widening changes
        // what is reachable, not what wins.
        //
        // The floor is a majority of the terms rather than one of them: the
        // point is a near miss, not every page sharing the query's commonest
        // word. A two-word query does end up as a union, which is the right
        // reading when nothing contains both.
        let least = (terms.len() + 1) / 2;
        let widened: Vec<crate::index::DocId> = index
            .candidates_at_least(&terms, least)
            .into_iter()
            .map(|(doc, _)| doc)
            .filter(|&doc| keep(doc))
            .collect();
        if widened.len() > candidates.len() {
            candidates = widened;
        }
    }
    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .filter_map(|doc| {
            let features = features(index, doc, &terms)?;
            Some(Hit { doc, score: features.score(), features })
        })
        .collect();

    // Descending by score, then by document id so equal scores come out in a
    // stable order rather than however the candidate set happened to be built.
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.doc.cmp(&b.doc))
    });
    hits.truncate(limit);
    hits
}

/// The feature vector for one document against one query.
///
/// `None` when the document is not live, which can happen between a candidate
/// set being built and this running.
pub fn features(index: &Index, doc: DocId, terms: &[String]) -> Option<Features> {
    let document = index.document(doc)?;
    let length = document.term_count.max(1) as f64;
    let average = index.average_length();

    let title_terms: std::collections::HashSet<String> =
        tokenize::terms(&document.title).into_iter().collect();

    let mut f = Features::default();
    let mut present = 0usize;

    for term in terms {
        let positions = index.positions(term, doc);
        if positions.is_empty() {
            continue;
        }
        present += 1;
        let tf = positions.len() as f64;
        let weight = idf(index, term);
        // BM25's saturating term frequency, normalised by document length.
        let saturated = (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * length / average));
        f.bm25 += weight * saturated;

        if title_terms.contains(term) {
            // The title is short, so its own length normalisation would swamp
            // the signal; the weight is applied on the way out instead.
            f.title += weight;
        }
    }

    // Terms the corpus contains at all. A term in no document cannot separate
    // one document from another, so counting it in the denominator measures
    // the query's vocabulary rather than the document's relevance — a search
    // for "10W30" against a corpus that only ever writes "10W-30" penalised
    // every page equally for a word none of them use.
    //
    // It does not reorder anything on its own, since the same denominator
    // applies to every candidate. It makes the number mean what it says,
    // which matters because it is reported and because a learned ranker
    // would otherwise be fitting to a constant.
    let answerable = terms
        .iter()
        .filter(|t| index.document_frequency(t) > 0)
        .count()
        .max(1);
    f.coverage = present as f64 / answerable as f64;
    f.prose = if document.term_count < MIN_TERMS_TO_JUDGE_SHAPE {
        1.0
    } else {
        document.prose_share as f64 / 100.0
    };
    f.phrase = phrase_score(index, doc, terms);
    f.url_depth = url_depth_score(&document.url);
    Some(f)
}

/// How close together the query's terms appear, from 1.0 (adjacent, in order)
/// down to 0.0.
///
/// Looks at the smallest window containing one occurrence of each term. A
/// single-term query has no proximity to measure and scores zero, so it is not
/// rewarded for something it cannot express.
fn phrase_score(index: &Index, doc: DocId, terms: &[String]) -> f64 {
    if terms.len() < 2 {
        return 0.0;
    }
    let lists: Vec<&[u32]> = terms.iter().map(|t| index.positions(t, doc)).collect();
    if lists.iter().any(|l| l.is_empty()) {
        return 0.0;
    }
    // The smallest span covering one position from every list. Walked by
    // repeatedly advancing whichever list is furthest behind, which finds the
    // minimum window in a single pass over each list.
    let mut cursors = vec![0usize; lists.len()];
    let mut best = u32::MAX;
    loop {
        let mut lowest = 0;
        let mut low = u32::MAX;
        let mut high = 0u32;
        for (i, (list, &c)) in lists.iter().zip(&cursors).enumerate() {
            let p = list[c];
            if p < low {
                low = p;
                lowest = i;
            }
            high = high.max(p);
        }
        best = best.min(high - low);
        cursors[lowest] += 1;
        if cursors[lowest] >= lists[lowest].len() {
            break;
        }
    }
    // A window equal to the term count means they are consecutive. Beyond
    // that the score decays, reaching nothing by about thirty terms apart —
    // words in different paragraphs are not a phrase.
    let ideal = terms.len() as u32 - 1;
    let slack = best.saturating_sub(ideal) as f64;
    (1.0 - slack / 30.0).max(0.0)
}

/// A small preference for URLs nearer the root of their site.
///
/// Deliberately weak: it is a heuristic about site structure, not evidence
/// about content, and a strong version would rank a site's front page above
/// the documentation page that actually answers the question.
fn url_depth_score(url: &str) -> f64 {
    let path = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url);
    let segments = path
        .split('/')
        .skip(1)
        .filter(|s| !s.is_empty() && !s.contains('?') && !s.contains('#'))
        .count();
    match segments {
        0 => 0.5,
        1 => 0.4,
        2 => 0.3,
        3 => 0.2,
        4 => 0.1,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Index;

    fn corpus() -> Index {
        let mut ix = Index::new();
        ix.add(
            "https://doc.example/no-std",
            "Writing no_std Rust",
            "",
            "In no_std builds there is no allocator unless you provide one. \
             The allocator is chosen by the crate.",
        );
        ix.add(
            "https://blog.example/2019/04/17/long/post/page/2",
            "A long ramble",
            "",
            &format!(
                "Once upon a time I used an allocator. {} \
                 Later I mentioned no_std in passing.",
                "Filler sentence about unrelated things. ".repeat(200)
            ),
        );
        ix.add(
            "https://doc.example/alloc",
            "Allocators",
            "",
            "An allocator manages memory for a program.",
        );
        ix
    }

    fn url_of(ix: &Index, hit: &Hit) -> String {
        ix.document(hit.doc).unwrap().url.clone()
    }

    /// The whole point: the page that is *about* the query comes first, not
    /// the longest page that happens to contain the words.
    #[test]
    fn the_page_about_the_query_ranks_first() {
        let ix = corpus();
        let hits = search(&ix, "no_std allocator", 10);
        assert!(!hits.is_empty(), "nothing matched");
        assert!(
            url_of(&ix, &hits[0]).contains("doc.example/no-std"),
            "ranked {:?} first",
            url_of(&ix, &hits[0])
        );
    }

    /// A board index must not outrank the thread that answers the question.
    ///
    /// This is the shape measured on a real forum: the listing carries the
    /// subject in forty thread titles and answers nothing, while the thread
    /// carries the answer in two sentences. Ranked on terms alone the listing
    /// wins — it says the words far more often — and the answer never
    /// surfaces. Real values were 4% and 9% prose for listings against 27% and
    /// 42% for threads.
    #[test]
    fn a_page_of_links_does_not_outrank_a_page_with_an_answer() {
        let mut ix = Index::new();
        // A listing: many short lines, the subject named in every one of them.
        let listing: String = (0..60)
            .map(|i| format!("Engine oil thread {i}\nReplies {i} Views 12K\n"))
            .collect();
        ix.add("https://forum.test/board", "Engine oil - Board index", "", &listing);
        // A thread: prose, the subject named twice.
        let thread = format!(
            "{}\n{}\n",
            "Somebody asked what engine oil to run in the old tractor and the \
             answer that came back was straight thirty weight, because that is \
             what the manual in the glovebox calls for and nobody has managed \
             to argue otherwise in seventy years of trying.",
            "The longer discussion about detergent and non-detergent went on for \
             pages after that, and the short version is that an engine with no \
             filter is happier without the detergent holding grit in suspension \
             where the bearings can find it again later on.",
        );
        ix.add("https://forum.test/thread", "What oil - thread", "", &thread);

        let hits = search(&ix, "engine oil", 5);
        assert_eq!(
            url_of(&ix, &hits[0]),
            "https://forum.test/thread",
            "the board index won: {:?}",
            hits.iter().map(|h| (url_of(&ix, h), h.score, h.features.prose)).collect::<Vec<_>>(),
        );
    }

    /// A short page is short, not a list, and must not be demoted for having
    /// no line long enough to look like prose.
    #[test]
    fn a_short_page_is_not_judged_on_its_shape() {
        let mut ix = Index::new();
        ix.add("https://a.test/x", "", "", "The oil is straight 30 weight.");
        let hits = search(&ix, "oil", 5);
        assert_eq!(hits[0].features.prose, 1.0, "a short page was penalised for being short");
    }

    /// Length normalisation, stated as a property: padding a document with
    /// irrelevant text must not improve its rank.
    #[test]
    fn padding_a_document_does_not_promote_it() {
        let mut ix = Index::new();
        ix.add("https://a.example/x", "", "", "allocator design notes");
        ix.add(
            "https://b.example/x",
            "",
            "",
            &format!("allocator {}", "unrelated filler. ".repeat(500)),
        );
        let hits = search(&ix, "allocator", 10);
        assert!(
            url_of(&ix, &hits[0]).contains("a.example"),
            "the padded document won: {:?}",
            hits.iter().map(|h| (url_of(&ix, h), h.score)).collect::<Vec<_>>()
        );
    }

    /// Saturation, as a property: saying a word repeatedly is worth less each
    /// time. Without it, keyword stuffing wins outright.
    #[test]
    fn repetition_has_diminishing_returns() {
        let mut ix = Index::new();
        ix.add("https://once.example/a", "", "", "allocator");
        ix.add("https://many.example/a", "", "", &"allocator ".repeat(40));
        let hits = search(&ix, "allocator", 10);
        let stuffed = hits.iter().find(|h| url_of(&ix, h).contains("many")).unwrap();
        let plain = hits.iter().find(|h| url_of(&ix, h).contains("once")).unwrap();
        // The stuffed page may still rank higher, but not proportionally.
        assert!(
            stuffed.features.bm25 < plain.features.bm25 * 4.0,
            "40x the term gave {}x the score",
            stuffed.features.bm25 / plain.features.bm25
        );
    }

    /// A term appearing everywhere distinguishes nothing, and must contribute
    /// almost nothing — earned from the corpus rather than asserted by a list.
    #[test]
    fn a_term_in_every_document_is_worth_little() {
        let mut ix = Index::new();
        for i in 0..20 {
            ix.add(&format!("https://x.example/{i}"), "", "", "the common word here");
        }
        ix.add("https://rare.example/a", "", "", "the common word here quicksilver");
        let common = idf(&ix, "the");
        let rare = idf(&ix, "quicksilver");
        assert!(rare > common * 3.0, "rare {rare} vs common {common}");
    }

    /// A title match is a claim the author made about the whole page.
    #[test]
    fn a_title_match_outranks_a_body_mention() {
        let mut ix = Index::new();
        ix.add("https://a.example/x", "Garbage Collection", "", "a page about memory and things");
        ix.add("https://b.example/x", "Unrelated", "", "garbage collection is mentioned once here");
        let hits = search(&ix, "garbage collection", 10);
        assert!(
            url_of(&ix, &hits[0]).contains("a.example"),
            "the title match did not win: {:?}",
            hits.iter().map(|h| (url_of(&ix, h), h.score, h.features.title)).collect::<Vec<_>>()
        );
    }

    /// Adjacent words are answering the question; scattered ones may only be
    /// mentioning it.
    #[test]
    fn the_phrase_scores_above_scattered_words() {
        let mut ix = Index::new();
        ix.add("https://near.example/x", "", "", "the no_std allocator is simple");
        ix.add(
            "https://far.example/x",
            "",
            "",
            &format!("no_std is a thing. {} And an allocator exists.", "Filler. ".repeat(40)),
        );
        let hits = search(&ix, "no_std allocator", 10);
        let near = hits.iter().find(|h| url_of(&ix, h).contains("near")).unwrap();
        let far = hits.iter().find(|h| url_of(&ix, h).contains("far")).unwrap();
        assert!(near.features.phrase > far.features.phrase,
                "near {} vs far {}", near.features.phrase, far.features.phrase);
        assert!(url_of(&ix, &hits[0]).contains("near"));
    }

    /// A single term has no proximity to express, so it is not rewarded for
    /// one.
    /// The per-block bounds are the largest term frequency in each block.
    ///
    /// Stored for a query strategy that can skip blocks — see
    /// `Index::compute_block_bounds`. Tested on its own because the strategy
    /// is not written yet and a bound that is too low would make a future
    /// skip silently wrong, which is worse than slow.
    #[test]
    fn block_bounds_really_are_the_maximum_in_each_block() {
        let mut ix = Index::new();
        // Three blocks' worth, with a frequency that varies per document so
        // the maxima differ between blocks.
        for d in 0..300 {
            let repeats = 1 + (d % 11);
            ix.add(
                &format!("https://a.test/{d}"),
                "",
                "",
                &format!("{}tail", "shared ".repeat(repeats)),
            );
        }
        ix.compute_block_bounds();
        let bounds = ix.block_max_tf("shared");
        assert_eq!(bounds.len(), 3, "300 postings should be three blocks of 128");

        // Checked against the postings themselves rather than against the
        // formula that produced them.
        let postings = ix.postings("shared");
        for (b, block) in postings.chunks(128).enumerate() {
            let actual = block.iter().map(|p| p.positions.len() as u32).max().unwrap();
            assert_eq!(bounds[b], actual, "block {b}");
        }
    }

    /// And they survive a save, since a loaded index is the one that most
    /// wants to skip — it has not just built the postings itself.
    #[test]
    fn block_bounds_survive_a_round_trip() {
        let mut ix = Index::new();
        for d in 0..200 {
            ix.add(&format!("https://a.test/{d}"), "", "", &format!("{}x", "w ".repeat(1 + d % 7)));
        }
        let dir = std::env::temp_dir().join(format!("forge-bounds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("i.bin");
        ix.save(&path).unwrap();
        let back = Index::load(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let bounds = back.block_max_tf("w");
        assert!(!bounds.is_empty(), "bounds were not written or not read back");
        let postings = back.posting_list("w");
        for (b, block) in postings.chunks(128).enumerate() {
            let actual = block.iter().map(|p| p.positions.len() as u32).max().unwrap();
            assert_eq!(bounds[b], actual, "block {b} after a round trip");
        }
    }

    #[test]
    fn a_single_term_query_has_no_phrase_score() {
        let ix = corpus();
        let hits = search(&ix, "allocator", 10);
        assert!(hits.iter().all(|h| h.features.phrase == 0.0));
    }

    /// Coverage is the fraction of the query a document contains. It used to
    /// be 1.0 for everything, because only documents with every term were ever
    /// candidates; now that a near miss can reach ranking, it carries real
    /// information and is what keeps widening honest.
    #[test]
    fn coverage_is_the_fraction_of_the_query_found() {
        let ix = corpus();
        // "Allocators" has the allocator but never says no_std.
        let hits = search(&ix, "no_std allocator", 10);
        let full = hits.iter().find(|h| url_of(&ix, h).ends_with("/no-std")).unwrap();
        assert_eq!(full.features.coverage, 1.0);
        let partial = hits.iter().find(|h| url_of(&ix, h).ends_with("/alloc"));
        if let Some(p) = partial {
            assert_eq!(p.features.coverage, 0.5);
        }
    }

    /// The case this was built for, in miniature.
    ///
    /// A real crawl answered nothing for `Q10 temperature coefficient neuron`
    /// even with the defining page indexed, because that page does not contain
    /// the word "neuron" — three terms of four, and no result at all. Asking a
    /// question in more words than the corpus uses is normal, so a strict
    /// reading that finds nothing must widen rather than give up.
    #[test]
    fn a_near_miss_is_found_when_nothing_matches_in_full() {
        let mut ix = Index::new();
        ix.add(
            "https://bio.example/q10",
            "Q10 temperature coefficient",
            "",
            "The q10 coefficient describes how a rate changes with temperature \
             across a ten degree interval.",
        );
        ix.add("https://bio.example/unrelated", "Gardening", "", "Soil and compost and seeds.");
        let hits = search(&ix, "q10 temperature coefficient neuron", 5);
        assert_eq!(hits.len(), 1, "a three-of-four match should be reachable");
        assert_eq!(url_of(&ix, &hits[0]), "https://bio.example/q10");
        // Coverage is measured against what the corpus can answer with.
        // "neuron" appears in no document here, so a page holding the other
        // three has everything available — penalising it for a word nothing
        // uses would measure the query's vocabulary, not the page.
        assert_eq!(hits[0].features.coverage, 1.0);
        // And a term the corpus does have but this page lacks still counts.
        let mut ix2 = Index::new();
        ix2.add("https://bio.example/q10", "Q10", "", "the q10 coefficient and temperature");
        ix2.add("https://bio.example/cells", "Cells", "", "a neuron is a cell");
        let hits2 = search(&ix2, "q10 temperature coefficient neuron", 5);
        let q10 = hits2.iter().find(|h| url_of(&ix2, h).ends_with("/q10")).unwrap();
        assert_eq!(q10.features.coverage, 0.75, "a term another page has must still count");
    }

    /// Widening must not change which document wins: a document with the whole
    /// query outranks one missing part of it, because `coverage` multiplies.
    #[test]
    fn a_full_match_outranks_a_near_miss() {
        let mut ix = Index::new();
        ix.add("https://a.example/both", "Both", "", "alpha and beta together here.");
        // Says alpha many times, so its term frequency alone would win.
        ix.add(
            "https://a.example/one",
            "Only alpha",
            "",
            &"alpha ".repeat(60),
        );
        let hits = search(&ix, "alpha beta", 5);
        assert_eq!(url_of(&ix, &hits[0]), "https://a.example/both",
                   "a partial match with more term frequency must not win");
    }

    /// And widening only happens when the strict reading came up short, so a
    /// query that is well answered is not diluted with near misses.
    #[test]
    fn a_sufficient_strict_result_is_not_widened() {
        let mut ix = Index::new();
        for i in 0..4 {
            ix.add(&format!("https://a.example/{i}"), "Both", "", "alpha beta present.");
        }
        ix.add("https://a.example/partial", "Half", "", "alpha only.");
        let hits = search(&ix, "alpha beta", 4);
        assert_eq!(hits.len(), 4);
        assert!(hits.iter().all(|h| h.features.coverage == 1.0),
                "near misses leaked into a result set that was already full");
    }

    #[test]
    fn a_shorter_url_is_preferred_all_else_equal() {
        assert!(url_depth_score("https://x.example/") > url_depth_score("https://x.example/a/b/c/d/e"));
        assert!(url_depth_score("https://x.example/docs") > url_depth_score("https://x.example/blog/2019/04/17/post"));
    }

    #[test]
    fn results_are_capped_and_ordered() {
        let mut ix = Index::new();
        for i in 0..30 {
            ix.add(&format!("https://x.example/{i}"), "", "", "allocator memory");
        }
        let hits = search(&ix, "allocator", 5);
        assert_eq!(hits.len(), 5);
        for pair in hits.windows(2) {
            assert!(pair[0].score >= pair[1].score, "not sorted: {:?}", hits.iter().map(|h| h.score).collect::<Vec<_>>());
        }
    }

    /// Equal scores must come out in a stable order rather than however the
    /// candidate set happened to be built.
    #[test]
    fn equal_scores_are_ordered_stably() {
        let mut ix = Index::new();
        for i in 0..5 {
            ix.add(&format!("https://x.example/{i}"), "", "", "identical text here");
        }
        let first = search(&ix, "identical", 10);
        let again = search(&ix, "identical", 10);
        assert_eq!(
            first.iter().map(|h| h.doc).collect::<Vec<_>>(),
            again.iter().map(|h| h.doc).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn an_empty_or_unmatched_query_returns_nothing() {
        let ix = corpus();
        assert!(search(&ix, "", 10).is_empty());
        assert!(search(&ix, "   ,,, ", 10).is_empty());
        assert!(search(&ix, "quicksilverzebra", 10).is_empty());
    }

    /// Ranking against an empty index must not divide by zero or panic.
    #[test]
    fn an_empty_index_ranks_nothing_without_panicking() {
        let ix = Index::new();
        assert!(search(&ix, "anything at all", 10).is_empty());
    }

    /// Every score must be a real number. A NaN sorts unpredictably and would
    /// scramble the results rather than obviously failing.
    #[test]
    fn no_score_is_ever_nan_or_infinite() {
        let ix = corpus();
        for query in ["allocator", "no_std allocator", "the allocator is", "a"] {
            for hit in search(&ix, query, 10) {
                assert!(hit.score.is_finite(), "{query:?} scored {}", hit.score);
                let f = &hit.features;
                for (name, v) in [
                    ("bm25", f.bm25), ("title", f.title), ("phrase", f.phrase),
                    ("url_depth", f.url_depth), ("coverage", f.coverage),
                ] {
                    assert!(v.is_finite(), "{query:?} feature {name} is {v}");
                }
            }
        }
    }
}
