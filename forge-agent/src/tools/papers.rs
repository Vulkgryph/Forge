// SPDX-License-Identifier: Apache-2.0
//! `search_papers`, backed by [`forge_search::epmc`].
//!
//! A separate tool from `web_search` rather than a mode of it, for three
//! reasons that all come down to the two not being the same operation:
//!
//! - **The terms differ.** A crawled web page carries no stated licence and
//!   this keeps none. An article does, and the licence decides whether its
//!   text may be kept at all — so every result here reports the terms it is
//!   held under, because a model that quotes a passage needs to know them.
//! - **The query language differs.** Europe PMC has its own syntax — quoted
//!   phrases, `AND`/`OR`, field prefixes like `AUTH:` and `METHODS:` — and the
//!   query is passed through to it rather than tokenised locally. Overloading
//!   one tool would mean one `query` argument with two meanings.
//! - **The corpus differs.** Mixing thirty Wikipedia pages and six papers in
//!   one index lets an encyclopaedia summary outrank a Methods section for a
//!   question about a measured value, and a paper outrank the encyclopaedia
//!   for a question about what a word means. They are kept apart so each can
//!   be asked for on purpose.
//!
//! What this cannot do is reach a paper that is not open access. Those come
//! back as a citation and a link, which is the honest answer: the model can
//! see that the article exists, say so, and fetch it by other means if the
//! user has access.

use anyhow::{Context, Result};
use forge_search::epmc;
use forge_search::index::Index;
use forge_search::query;

use super::search::HttpFetcher;

/// Most articles kept in the on-disk index.
///
/// Higher per item than the web index allows itself: an article is tens of
/// thousands of terms that cost a licensed request to fetch, and there are far
/// fewer of them than crawled pages.
const MAX_ARTICLES_KEPT: usize = 200;

/// How many passages to return.
///
/// Not a parameter. See the note on `search::MAX_RESULTS`: a knob the model
/// cannot reason about is a knob it sets arbitrarily.
const MAX_RESULTS: usize = 5;

/// How many articles to fetch when the local index cannot answer.
///
/// Each one is a request, and at one request per second a larger default would
/// make the first call on a topic feel broken. Six is enough to answer a
/// specific question and cheap enough to be worth trying.
const DEFAULT_ARTICLES: usize = 6;

/// Seconds between requests to the API.
///
/// A courtesy setting, not a performance one — the same figure the crawler
/// uses. Europe PMC publishes no numeric limit for this endpoint, so the
/// conventional one request per second is what is used, along with a
/// `User-Agent` that says who is asking.
const POLITENESS: f64 = 1.0;

/// Search the literature, fetching articles if the local index cannot answer.
pub async fn search_papers(args: &serde_json::Value, index_path: std::path::PathBuf) -> Result<String> {
    let query_text = args["query"]
        .as_str()
        .context("Missing 'query' argument")?
        .to_string();

    let handle = tokio::runtime::Handle::current();
    // The fetcher blocks on the runtime handle, which is only sound off a
    // worker thread — the same reason `web_search` does this.
    tokio::task::spawn_blocking(move || {
        run(&query_text, MAX_RESULTS, DEFAULT_ARTICLES, index_path, handle)
    })
        .await
        .map_err(|e| anyhow::anyhow!("literature search task failed: {e}"))?
}

fn run(
    query_text: &str,
    max_results: usize,
    max_articles: usize,
    index_path: std::path::PathBuf,
    handle: tokio::runtime::Handle,
) -> Result<String> {
    // A corrupt index is a cache of articles that can be fetched again, so it
    // is replaced rather than fatal.
    let mut index = Index::load(&index_path).unwrap_or_else(|_| Index::new());

    let started = std::time::Instant::now();
    let mut hits = query::search(&index, query_text, max_results);
    let local_ms = started.elapsed().as_millis();
    if !hits.is_empty() {
        return Ok(render_hits(query_text, &hits, &index, Outcome::no(index.len(), local_ms)));
    }

    let fetcher = HttpFetcher::new(handle, 8 * 1024 * 1024);
    let clock = forge_search::crawl::SystemClock;
    let fetch_start = std::time::Instant::now();
    let report = epmc::Connector::new(&fetcher, &clock, POLITENESS)
        .run(query_text, max_articles, &mut index);
    let fetch_ms = fetch_start.elapsed().as_millis();

    // Bounded like the web index, but generously: an article is tens of
    // thousands of terms of text that took a licensed request to get, and
    // there are far fewer of them than crawled pages.
    index.trim_to(MAX_ARTICLES_KEPT);
    let _ = crate::workdir::ensure_parent_of(&index_path);
    // Saved even when the query that follows finds nothing: the articles were
    // fetched, and discarding them means fetching them again.
    let _ = index.save(&index_path);

    let again = std::time::Instant::now();
    hits = query::search(&index, query_text, max_results);
    let search_ms = local_ms + again.elapsed().as_millis();

    let fetched = Outcome {
        ran: true,
        hit_count: report.hit_count,
        examined: report.examined,
        indexed: report.indexed,
        citation_only: report.citation_only,
        unreachable: report.unreachable,
        fetch_ms,
        index_size: index.len(),
        search_ms,
        // Articles whose text could not be kept, so the model can still cite
        // them. Capped, because a long list of things it cannot read is not
        // worth the context.
        cited: report
            .articles
            .iter()
            .filter(|a| !a.may_index_full_text())
            .take(5)
            .map(|a| (a.citation(), a.article_url()))
            .collect(),
    };
    Ok(render_hits(query_text, &hits, &index, fetched))
}

/// What the tool did, beyond the results themselves.
///
/// Named `Outcome` rather than `Fetched` because `forge_search::fetch::Fetched`
/// is a different thing — one HTTP response — and having both in scope under
/// one name is how a mechanical edit across the crate mangled this file.
struct Outcome {
    ran: bool,
    hit_count: usize,
    examined: usize,
    indexed: usize,
    citation_only: usize,
    unreachable: usize,
    fetch_ms: u128,
    index_size: usize,
    search_ms: u128,
    cited: Vec<(String, String)>,
}

impl Outcome {
    fn no(index_size: usize, search_ms: u128) -> Self {
        Self {
            ran: false,
            hit_count: 0,
            examined: 0,
            indexed: 0,
            citation_only: 0,
            unreachable: 0,
            fetch_ms: 0,
            index_size,
            search_ms,
            cited: Vec::new(),
        }
    }
}

fn render_hits(query_text: &str, hits: &[query::Result_], index: &Index, f: Outcome) -> String {
    let mut out = String::new();

    if hits.is_empty() {
        out.push_str(&format!("No open-access full text answering {query_text:?}.\n"));
        if f.ran {
            if f.unreachable > 0 {
                out.push_str(
                    "Europe PMC did not respond. That is a transient failure and worth one \
                     retry, unlike a query that simply matches nothing.\n",
                );
            } else if f.hit_count == 0 {
                out.push_str(
                    "Europe PMC reports no articles matching at all, so the query itself found \
                     nothing — try broader terms rather than more of them.\n",
                );
            } else {
                out.push_str(&format!(
                    "Europe PMC reports {} matching article(s); {} were examined and {} had \
                     full text that may be kept. The text that was kept does not answer this \
                     query — try different or broader terms.\n",
                    f.hit_count, f.examined, f.indexed,
                ));
            }
            render_citations(&mut out, &f);
        } else {
            out.push_str(&format!(
                "The local article index holds {} paper(s) and none matched.\n",
                f.index_size,
            ));
        }
        return out;
    }

    out.push_str(&format!("{} result(s) for {query_text:?}:\n\n", hits.len()));
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n",
            i + 1,
            if hit.title.is_empty() { &hit.url } else { &hit.title }
        ));
        out.push_str(&format!("   {}\n", hit.url));
        // The licence, every time. It is the reason the field is stored, and a
        // passage quoted without its terms is a passage quoted blind.
        if let Some(terms) = attribution_of(index, &hit.url) {
            if !terms.is_empty() {
                out.push_str(&format!("   terms: {terms}\n"));
            }
        }
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
        out.push('\n');
    }

    render_citations(&mut out, &f);

    if f.ran {
        out.push_str(&format!(
            "[Europe PMC: {} match, {} examined, {} indexed in {}ms; index now {} paper(s); \
             search {}ms. Later queries use the index and do not fetch.]\n",
            f.hit_count, f.examined, f.indexed, f.fetch_ms, f.index_size, f.search_ms,
        ));
    } else {
        out.push_str(&format!(
            "[from the local index of {} paper(s), {}ms, nothing fetched]\n",
            f.index_size, f.search_ms,
        ));
    }
    out
}

/// Articles that matched the search but whose text could not be kept.
///
/// Reported rather than dropped: an agent that is not told coverage was
/// limited will read what it got as everything there is.
fn render_citations(out: &mut String, f: &Outcome) {
    if f.cited.is_empty() {
        return;
    }
    out.push_str(&format!(
        "{} matching article(s) are not open access, so no text was kept. They exist and can \
         be cited:\n",
        f.citation_only,
    ));
    for (citation, url) in &f.cited {
        out.push_str(&format!("   - {citation}\n     {url}\n"));
    }
    out.push('\n');
}

fn attribution_of(index: &Index, url: &str) -> Option<String> {
    (0..index.len() as u32)
        .filter_map(|d| index.document(d))
        .find(|d| d.url == url)
        .map(|d| d.attribution.clone())
}

/// Where the article index lives for a workspace.
///
/// Separate from the web index: see the note at the top of the module on why
/// the two corpora are not mixed.
pub fn index_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root.join(".forge").join("paper-index.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_is_separate_from_the_web_index() {
        let root = std::path::Path::new("/work/proj");
        let papers = index_path(root);
        assert!(papers.starts_with("/work/proj/.forge"));
        assert_ne!(
            papers,
            super::super::search::index_path(root),
            "papers and web pages must not share an index"
        );
    }

    /// The licence has to reach the model. It is the reason the field is
    /// carried through the index at all, and a result shown without it invites
    /// a quote with no terms attached.
    #[test]
    fn a_result_reports_the_terms_it_is_held_under() {
        let mut index = Index::new();
        index.add_attributed(
            "https://europepmc.org/article/PMC/PMC1",
            "Recordings at 32 degrees",
            "We measured the rate.",
            "Slices were held at 32 degrees in ACSF.",
            "cc by-nc — Europe PMC PMC1",
        );
        let hits = query::search(&index, "slices acsf", 3);
        assert_eq!(hits.len(), 1);
        let out = render_hits("slices acsf", &hits, &index, Outcome::no(1, 1));
        assert!(out.contains("terms: cc by-nc — Europe PMC PMC1"), "{out}");
    }

    /// An article that could not be kept is still reported, because an agent
    /// told nothing about it will assume there was nothing.
    #[test]
    fn articles_that_could_not_be_kept_are_still_cited() {
        let mut f = Outcome::no(0, 0);
        f.ran = true;
        f.citation_only = 2;
        f.cited = vec![(
            "Alcohol and layer 5 pyramidal neurons — J. Neurosci (2026)".into(),
            "https://europepmc.org/article/PMC/PMC9".into(),
        )];
        let out = render_hits("x", &[], &Index::new(), f);
        assert!(out.contains("not open access"), "{out}");
        assert!(out.contains("Alcohol and layer 5"), "{out}");
        assert!(out.contains("PMC9"), "{out}");
    }

    /// "Nothing matches" and "the API is down" deserve different responses, so
    /// they are not reported with the same words.
    #[test]
    fn an_api_failure_reads_differently_from_an_empty_result() {
        let mut down = Outcome::no(0, 0);
        down.ran = true;
        down.unreachable = 1;
        let out = render_hits("x", &[], &Index::new(), down);
        assert!(out.contains("did not respond"), "{out}");
        assert!(out.contains("worth one retry"), "{out}");

        let mut empty = Outcome::no(0, 0);
        empty.ran = true;
        let out = render_hits("x", &[], &Index::new(), empty);
        assert!(out.contains("no articles matching at all"), "{out}");
        assert!(!out.contains("worth one retry"), "{out}");
    }

    /// Matches that exist but were not answered by the kept text say so, so
    /// the model knows to widen rather than conclude the literature is silent.
    #[test]
    fn matches_that_were_not_reached_are_distinguished_from_none() {
        let mut some = Outcome::no(0, 0);
        some.ran = true;
        some.hit_count = 2636;
        some.examined = 18;
        some.indexed = 6;
        let out = render_hits("x", &[], &Index::new(), some);
        assert!(out.contains("2636 matching article(s)"), "{out}");
        assert!(out.contains("different or broader terms"), "{out}");
    }

    /// The whole path, against the live API.
    ///
    /// Ignored by default: it needs the network, and a test that fails when a
    /// third party is down is a test that trains people to ignore failures.
    /// Run with `cargo test -p forge-agent -- --ignored live_`. It is here
    /// rather than in a scratch binary because the part it covers is not
    /// covered by anything else — argument parsing, the blocking hop off the
    /// runtime, the index round trip, and the licence reaching the output.
    #[tokio::test]
    #[ignore = "needs the network"]
    async fn live_search_papers_returns_cited_passages() {
        let dir = std::env::temp_dir().join(format!("forge-papers-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("paper-index.bin");

        let args = serde_json::json!({
            "query": "\"pyramidal neuron\" AND \"patch clamp\" AND temperature",
            "max_results": 3,
            "max_articles": 3,
        });
        let first = search_papers(&args, path.clone()).await.expect("first call");
        println!("--- first call ---\n{first}");
        assert!(first.contains("Europe PMC"), "no report line: {first}");
        assert!(first.contains("terms:"), "no licence on any result: {first}");
        assert!(path.exists(), "the index was not saved, so the next call refetches");

        // Second call must be answered from the index without fetching.
        let second = search_papers(&args, path.clone()).await.expect("second call");
        println!("--- second call ---\n{second}");
        assert!(
            second.contains("nothing fetched"),
            "the saved index was not used: {second}",
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
