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
        // With today's intersecting candidate set this is always 1.0, so it
        // changes nothing — it is here because the moment partial matches are
        // allowed it becomes the difference between useful and useless, and
        // discovering that later means rediscovering why.
        base * self.coverage
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

/// Score and sort the documents matching `query`.
///
/// Returns at most `limit`, best first. The candidate set comes from the
/// index's intersection, so this ranks documents that contain every term.
pub fn search(index: &Index, query: &str, limit: usize) -> Vec<Hit> {
    let terms: Vec<String> = tokenize::terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let mut candidates = index.candidates(&terms);
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
        let widened = index.candidates_at_least(&terms, least);
        if widened.len() > candidates.len() {
            candidates = widened.into_iter().map(|(doc, _)| doc).collect();
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

    f.coverage = present as f64 / terms.len() as f64;
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
        assert_eq!(hits[0].features.coverage, 0.75);
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
