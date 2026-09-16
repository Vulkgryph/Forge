//! Where does the page that actually answers the question rank?
//!
//! The one question worth asking of a ranker, and the only way to change its
//! weights without guessing. Loads real indexes built by real crawls, takes
//! queries whose right answer is known, and reports the rank the current
//! scoring gives that answer alongside what candidate scorings would give it.
//!
//! `Features` is public and `score()` is written out rather than hidden, so a
//! variant is a few lines here rather than a rebuild of the library.
//!
//! Two labelled cases is a thin basis and the output says so. It is enough to
//! catch a change that makes things worse, which is the failure that matters.

use forge_search::index::Index;
use forge_search::rank::{self, Features};

/// A query with a known right answer.
struct Case {
    index: &'static str,
    query: &'static str,
    /// Substring identifying the page that answers it.
    answer: &'static str,
}

/// A candidate way of combining the features.
struct Variant {
    name: &'static str,
    score: fn(&Features) -> f64,
}

const TITLE_W: f64 = 2.5;
const PHRASE_W: f64 = 3.0;
const PROSE_FLOOR: f64 = 0.3;

fn base(f: &Features) -> f64 {
    f.bm25 + f.title * TITLE_W + f.phrase * PHRASE_W + f.url_depth
}

fn base_with_title(f: &Features, title_weight: f64) -> f64 {
    f.bm25 + f.title * title_weight + f.phrase * PHRASE_W + f.url_depth
}

fn prose(f: &Features) -> f64 {
    PROSE_FLOOR + (1.0 - PROSE_FLOOR) * f.prose
}

fn main() {
    let cases = [
        // The tractor question, in the wording the agent actually used. The
        // page that answers it says "30-weight" where the query says "SAE 30",
        // so none of the query's distinctive terms appear on it.
        Case {
            index: "t4",
            query: "Ford 8N engine oil SAE 30 10W30 6 quarts",
            answer: "myfordtractors.com/tune.shtml",
        },
        Case {
            index: "t4",
            query: "Ford 8N tractor engine oil recommended SAE temperature manual capacity",
            answer: "myfordtractors.com/tune.shtml",
        },
        // The Azure question, where the query and the documentation share a
        // vocabulary — the case that must not regress.
        Case {
            index: "az2",
            query: "Azure Container Apps scaling minReplicas maxReplicas scale to zero",
            answer: "container-apps/scale-app",
        },
        Case {
            index: "az2",
            query: "container apps scale to zero billing idle replicas",
            answer: "container-apps/billing",
        },
    ];

    let variants = [
        Variant { name: "current", score: |f| base(f) * f.coverage * prose(f) },
        // Coverage's influence made sub-linear. It exists to stop a half match
        // winning on frequency; it should not let 0.56 against 0.67 overturn a
        // fifty percent lead in BM25.
        Variant { name: "cov=sqrt", score: |f| base(f) * f.coverage.sqrt() * prose(f) },
        Variant { name: "cov=0.5+0.5c", score: |f| base(f) * (0.5 + 0.5 * f.coverage) * prose(f) },
        Variant { name: "cov=0.3+0.7c", score: |f| base(f) * (0.3 + 0.7 * f.coverage) * prose(f) },
        // A title match on the machine's name is not a match on the topic, and
        // 2.5 lets one carry a page a long way.
        Variant { name: "title=1.5", score: |f| base_with_title(f, 1.5) * f.coverage * prose(f) },
        Variant { name: "title=1.5,sqrt", score: |f| base_with_title(f, 1.5) * f.coverage.sqrt() * prose(f) },
        Variant { name: "title=1.0,sqrt", score: |f| base_with_title(f, 1.0) * f.coverage.sqrt() * prose(f) },
    ];

    let root = std::env::args().nth(1).expect("usage: tune_ranker <scratchpad dir>");

    // rank of the answer under each variant, per case
    let mut ranks: Vec<Vec<Option<usize>>> = vec![Vec::new(); variants.len()];

    for case in &cases {
        let path = format!("{root}/{}/.forge/search-index", case.index);
        let Ok(ix) = Index::load(std::path::Path::new(&path)) else {
            eprintln!("skipping {}: no index at {path}", case.index);
            continue;
        };
        // A wide candidate set, so re-scoring can reorder freely.
        let hits = rank::search(&ix, case.query, 50);
        println!("\n{}\n  {} — {} pages, {} candidates", "─".repeat(72), case.query, ix.len(), hits.len());

        for (vi, v) in variants.iter().enumerate() {
            let mut scored: Vec<(f64, String)> = hits
                .iter()
                .filter_map(|h| ix.document(h.doc).map(|d| ((v.score)(&h.features), d.url.clone())))
                .collect();
            scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
            let at = scored.iter().position(|(_, u)| u.contains(case.answer)).map(|i| i + 1);
            ranks[vi].push(at);
            let top = scored.first().map(|(_, u)| u.as_str()).unwrap_or("-");
            println!(
                "    {:<16} answer at {:<5} top: {}",
                v.name,
                at.map(|n| n.to_string()).unwrap_or_else(|| "absent".into()),
                top.replace("https://www.", "").replace("https://", "").chars().take(52).collect::<String>(),
            );
        }
    }

    println!("\n{}\n  summary — rank of the right answer (lower is better)", "═".repeat(72));
    println!("  {:<16} {:>22}  {:>10}  {}", "variant", "ranks", "mean", "top-1 hits");
    for (vi, v) in variants.iter().enumerate() {
        let rs = &ranks[vi];
        let got: Vec<usize> = rs.iter().filter_map(|r| *r).collect();
        let mean = if got.is_empty() { f64::NAN } else { got.iter().sum::<usize>() as f64 / got.len() as f64 };
        let firsts = got.iter().filter(|r| **r == 1).count();
        let shown: Vec<String> = rs.iter().map(|r| r.map(|n| n.to_string()).unwrap_or("-".into())).collect();
        println!("  {:<16} {:>22}  {:>10.2}  {}/{}", v.name, shown.join(","), mean, firsts, got.len());
    }
    println!("\n  Four labelled cases is a thin basis — enough to catch a change that");
    println!("  makes things worse, not enough to claim one is optimal.");
}
