// SPDX-License-Identifier: Apache-2.0
//! `web_search`, backed by the engine in `forge-search`.
//!
//! This replaces scraping someone else's results page. The old implementation
//! asked DuckDuckGo for HTML and read it, which failed in the way that was
//! always going to fail: automated queries get challenged, and a search tool
//! that usually returns nothing is worse than one that is absent, because the
//! model spends a turn on it and then reasons about the emptiness.
//!
//! What replaces it crawls and indexes pages itself, so the results come from
//! documents Forge has actually read. The cost is that it only knows what it
//! has crawled — so a query against an empty index crawls first, which is slow
//! once and fast afterwards.
//!
//! The engine is deliberately unaware of Forge. It asks for a
//! [`forge_search::fetch::Fetcher`] and this module provides one over the HTTP
//! client the agent already carries; nothing about the engine knows what a
//! `reqwest` is. That is what keeps it liftable into something that is not
//! Forge.

use anyhow::{Context, Result};
use forge_search::crawl::{self, Limits};
use forge_search::fetch::{Fetched, Fetcher};
use forge_search::index::Index;
use forge_search::query;

/// What Forge calls itself when it crawls.
///
/// The same honesty as the fetch tool: an agent identifies itself and takes
/// the answer it gets. It matters more here, because a crawler that lies about
/// who it is cannot meaningfully claim to be obeying `robots.txt` — the file
/// addresses crawlers by name.
pub(crate) const USER_AGENT: &str = concat!("forge-search/", env!("CARGO_PKG_VERSION"));

/// Where a crawl starts when the query names no site of its own.
///
/// Small and technical on purpose. A general crawl of the web from a handful
/// of seeds is a research project; a crawl of the documentation an agent
/// actually asks about is a few hundred pages and useful immediately. The list
/// is configurable precisely because whoever runs Forge knows better than this
/// file what their agent needs to read.
const DEFAULT_SEEDS: &[&str] = &[
    "https://doc.rust-lang.org/book/",
    "https://doc.rust-lang.org/std/",
    "https://doc.rust-lang.org/nomicon/",
];

/// A [`Fetcher`] over the agent's HTTP client.
///
/// The trait is synchronous and `reqwest` here is not, so each fetch blocks on
/// the runtime handle. That is only sound off a runtime worker thread, which
/// is why every crawl runs inside `spawn_blocking` — see [`search`].
pub(crate) struct HttpFetcher {
    client: reqwest::Client,
    handle: tokio::runtime::Handle,
    /// Largest body to read, so one enormous page cannot exhaust memory
    /// before the crawler's own limit has a chance to reject it.
    max_bytes: usize,
}

impl HttpFetcher {
    /// One configured the way every caller wants it: identified, bounded in
    /// time, and following a sane number of redirects.
    pub(crate) fn new(handle: tokio::runtime::Handle, max_bytes: usize) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::limited(5))
                .build()
                .unwrap_or_default(),
            handle,
            max_bytes,
        }
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(&self, url: &str) -> std::result::Result<Fetched, String> {
        self.handle.block_on(async {
            let response = self
                .client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("{url}: {e}"))?;

            let status = response.status().as_u16();
            let final_url = response.url().to_string();
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| {
                    v.split(';')
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_ascii_lowercase()
                })
                .unwrap_or_default();

            // Length is checked before the body is read where the server
            // declares one, so an enormous page costs a header rather than a
            // download.
            if let Some(len) = response.content_length() {
                if len as usize > self.max_bytes {
                    return Ok(Fetched {
                        status,
                        final_url,
                        content_type,
                        // Deliberately oversized, so the crawler's own
                        // `max_page_bytes` rejects it and counts it. Reporting
                        // a short body would have it indexed as a short page.
                        body: "x".repeat(self.max_bytes + 1),
                    });
                }
            }

            let body = response.text().await.unwrap_or_default();
            Ok(Fetched { status, final_url, content_type, body })
        })
    }

    fn user_agent(&self) -> &str {
        USER_AGENT
    }
}

/// Pages to crawl when the caller does not say.
///
/// A hundred and twenty, which is high enough to be worth defending. Below
/// roughly a hundred pages a crawl of a forum returns board *indexes* rather
/// than discussions — the listing names the subject in forty thread titles and
/// answers nothing, and there is no ranking trick that rescues it because the
/// answers were never fetched. Measured on two tractor forums asked what
/// engine oil an old tractor takes:
///
/// ```text
///    25 pages   4 of the top 5 results were board listings
///   130 pages   0 of 5 — every result a thread, with answers in it
/// ```
///
/// The ranking features were the same in both runs. The corpus was the
/// difference, and no amount of scoring fixes a page that was never read.
const DEFAULT_MAX_PAGES: usize = 120;

/// How long a crawl may take before it is cut short.
///
/// A tool call that never returns is worse than one that returns little, so
/// the wall clock is bounded and not only the page count — one slow host can
/// otherwise hold a twenty-page crawl for minutes.
///
/// It scales with the pages asked for, which the previous fixed 25 seconds did
/// not, and that made every other setting a lie: at one request per second a
/// 25-second budget stops at about 25 pages, so the `max_pages` default of 40
/// was unreachable and its documented ceiling of 500 was fiction. The tool
/// could not leave the regime that returns listings no matter what it was
/// asked for.
///
/// The ceiling is what keeps it honest in the other direction: 500 pages at a
/// second each is over eight minutes, and a tool call that long should stop
/// and say it stopped rather than hold the turn.
fn crawl_budget_secs(max_pages: usize, politeness: f64) -> f64 {
    const OVERHEAD: f64 = 15.0;
    const CEILING: f64 = 300.0;
    (max_pages as f64 * politeness * 1.2 + OVERHEAD).min(CEILING)
}

/// Seconds between requests to one host. A courtesy setting, not a
/// performance one — see `Limits::politeness`.
const POLITENESS: f64 = 1.0;

/// What one search did, for the model and for anyone measuring.
#[derive(Debug, Default)]
pub struct Timing {
    pub crawled: bool,
    pub fetched: usize,
    pub indexed: usize,
    pub disallowed: usize,
    pub crawl_ms: u128,
    pub search_ms: u128,
    pub index_size: usize,
    pub results: usize,
    /// The crawl ran out of time rather than pages.
    ///
    /// Worth telling the model, but not as "ask again and it continues" — the
    /// frontier is not persisted, so a second crawl starts from the seeds and
    /// refetches. What it should do instead is raise `max_pages`, or narrow
    /// `sites`.
    pub timed_out: bool,
}

/// Run a search, crawling first if the index cannot answer it.
///
/// `index_path` is where the index is kept between calls; the first search is
/// slow and later ones are not.
pub async fn web_search(args: &serde_json::Value, index_path: std::path::PathBuf) -> Result<String> {
    let query_text = args["query"]
        .as_str()
        .context("Missing 'query' argument")?
        .to_string();
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .clamp(1, 20) as usize;

    // Seeds from the query when it names a site, so "search docs.rs for
    // tokio" reaches somewhere the default list does not. A query that names
    // no site uses the configured defaults.
    let seeds: Vec<String> = args
        .get("sites")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str()).map(String::from).collect())
        .unwrap_or_else(|| DEFAULT_SEEDS.iter().map(|s| s.to_string()).collect());

    let max_pages = args
        .get("max_pages")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_MAX_PAGES as u64)
        .clamp(1, 500) as usize;

    let handle = tokio::runtime::Handle::current();
    // The whole crawl-and-search runs off the runtime's worker threads,
    // because the fetcher blocks and blocking a worker starves everything
    // else the agent is doing.
    let result = tokio::task::spawn_blocking(move || {
        run(&query_text, &seeds, max_results, max_pages, index_path, handle)
    })
    .await
    .map_err(|e| anyhow::anyhow!("search task failed: {e}"))?;

    result
}

fn run(
    query_text: &str,
    seeds: &[String],
    max_results: usize,
    max_pages: usize,
    index_path: std::path::PathBuf,
    handle: tokio::runtime::Handle,
) -> Result<String> {
    let mut timing = Timing::default();

    // An index that cannot be read is replaced rather than fatal. It is a
    // cache of pages that can be fetched again, and refusing to search because
    // a cache file is corrupt would be the wrong trade.
    let mut index = Index::load(&index_path).unwrap_or_else(|_| Index::new());

    let search_start = std::time::Instant::now();
    let mut hits = query::search(&index, query_text, max_results);
    timing.search_ms = search_start.elapsed().as_millis();

    if hits.is_empty() {
        let fetcher = HttpFetcher::new(handle, 2 * 1024 * 1024);
        let limits = Limits {
            max_pages,
            max_depth: 3,
            politeness: POLITENESS,
            stay_on_host: true,
            max_page_bytes: 2 * 1024 * 1024,
            max_seconds: Some(crawl_budget_secs(max_pages, POLITENESS)),
        };

        let crawl_start = std::time::Instant::now();
        let clock = crawl::SystemClock;
        let mut crawler = crawl::Crawler::new(&fetcher, &clock, limits);
        let mut seeded = 0;
        for seed in seeds {
            if crawler.seed(seed).is_ok() {
                seeded += 1;
            }
        }
        if seeded == 0 {
            anyhow::bail!("no usable seed URLs in {seeds:?}");
        }
        let report = crawler.run(&mut index);
        timing.crawl_ms = crawl_start.elapsed().as_millis();
        timing.crawled = true;
        timing.fetched = report.fetched;
        timing.indexed = report.indexed;
        timing.disallowed = report.disallowed;
        timing.timed_out = report.timed_out;

        // Saved even when the search that follows finds nothing: the pages
        // were fetched, and throwing them away means fetching them again for
        // the next query.
        if let Some(parent) = index_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = index.save(&index_path);

        let again = std::time::Instant::now();
        hits = query::search(&index, query_text, max_results);
        timing.search_ms += again.elapsed().as_millis();
    }

    timing.index_size = index.len();
    timing.results = hits.len();
    Ok(render(query_text, &hits, &timing))
}

/// The tool result the model sees.
///
/// The timing line is for the model as much as for a person: a search that
/// crawled forty pages to answer a question is a search whose next call will
/// be instant, and an agent that knows that will not avoid the tool for being
/// slow.
fn render(query_text: &str, hits: &[query::Result_], timing: &Timing) -> String {
    let mut out = String::new();
    if hits.is_empty() {
        out.push_str(&format!("No results for {query_text:?}.\n"));
        if timing.crawled {
            out.push_str(&format!(
                "Crawled {} pages ({} indexed, {} refused by robots.txt) in {}ms and found \
                 nothing matching. The index now holds {} pages; try different terms, or pass \
                 `sites` to crawl somewhere else. Crawling again repeats the same pages rather \
                 than continuing, so raise `max_pages` instead of retrying as-is.\n",
                timing.fetched, timing.indexed, timing.disallowed, timing.crawl_ms,
                timing.index_size,
            ));
        } else {
            out.push_str(&format!(
                "The index holds {} pages and none matched. Pass `sites` to crawl somewhere \
                 new.\n",
                timing.index_size,
            ));
        }
        return out;
    }

    out.push_str(&format!("{} result(s) for {query_text:?}:\n\n", hits.len()));
    for (i, hit) in hits.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, if hit.title.is_empty() { &hit.url } else { &hit.title }));
        out.push_str(&format!("   {}\n", hit.url));
        if !hit.snippet.is_empty() {
            out.push_str(&format!("   {}\n", hit.snippet));
        }
        out.push('\n');
    }
    if timing.crawled {
        out.push_str(&format!(
            "[crawled {} pages in {}ms{}; index now {} pages; search {}ms. \
             Later searches use the index and do not crawl.]\n",
            timing.fetched,
            timing.crawl_ms,
            if timing.timed_out {
                " (stopped on the time budget; a further crawl restarts from the seeds, so \
                 raise max_pages rather than repeating this call)"
            } else {
                ""
            },
            timing.index_size,
            timing.search_ms,
        ));
    } else {
        out.push_str(&format!(
            "[from the local index of {} pages, {}ms, no crawl]\n",
            timing.index_size, timing.search_ms,
        ));
    }
    out
}

/// Where the index lives for a workspace.
pub fn index_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root.join(".forge").join("search-index.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The budget has to permit the pages that were asked for. A fixed
    /// 25 seconds at one request per second stopped every crawl at about 25
    /// pages, which made the `max_pages` default of 40 unreachable and its
    /// ceiling of 500 fiction — and 25 pages is exactly the regime that
    /// returns board listings instead of answers.
    #[test]
    fn the_time_budget_allows_the_pages_requested() {
        for pages in [25usize, 40, 120] {
            let budget = crawl_budget_secs(pages, POLITENESS);
            assert!(
                budget > pages as f64 * POLITENESS,
                "{pages} pages at {POLITENESS}s each cannot finish in {budget}s",
            );
        }
    }

    /// And it is still bounded, because a tool call that holds the turn for
    /// ten minutes is its own failure.
    #[test]
    fn the_time_budget_is_capped() {
        assert!(crawl_budget_secs(500, POLITENESS) <= 300.0);
        assert!(crawl_budget_secs(100_000, POLITENESS) <= 300.0);
    }

    /// The default is above the measured threshold where a forum crawl starts
    /// returning discussions rather than indexes of discussions.
    #[test]
    fn the_default_page_count_clears_the_measured_threshold() {
        assert!(
            DEFAULT_MAX_PAGES >= 100,
            "{DEFAULT_MAX_PAGES} pages returns board listings, measured",
        );
    }

    #[test]
    fn the_index_path_is_inside_the_workspace() {
        let p = index_path(std::path::Path::new("/work/proj"));
        assert!(p.starts_with("/work/proj/.forge"));
        assert_eq!(p.file_name().unwrap(), "search-index.bin");
    }

    /// The result tells the model whether it paid for a crawl, so it does not
    /// conclude the tool is slow from the one call that seeded the index.
    #[test]
    fn a_crawling_search_says_so_and_a_cached_one_says_it_did_not() {
        let hits = vec![query::Result_ {
            url: "https://x.example/a".into(),
            title: "A".into(),
            snippet: "something".into(),
            score: 1.0,
        }];
        let crawled = render("q", &hits, &Timing {
            crawled: true, fetched: 40, crawl_ms: 9000, index_size: 40, search_ms: 2,
            ..Default::default()
        });
        assert!(crawled.contains("crawled 40 pages"));
        assert!(crawled.contains("Later searches use the index"));

        let cached = render("q", &hits, &Timing {
            crawled: false, index_size: 40, search_ms: 2, ..Default::default()
        });
        assert!(cached.contains("no crawl"), "{cached}");
        assert!(!cached.contains("crawled 40"));
    }

    /// An empty result has to say what was tried, or the model cannot tell
    /// "nothing matched" from "the tool is broken" — which is exactly how the
    /// DuckDuckGo implementation wasted turns.
    #[test]
    fn an_empty_result_explains_itself() {
        let empty = render("q", &[], &Timing {
            crawled: true, fetched: 12, indexed: 10, disallowed: 2, crawl_ms: 3000,
            index_size: 10, ..Default::default()
        });
        assert!(empty.contains("No results"));
        assert!(empty.contains("Crawled 12 pages"));
        assert!(empty.contains("refused by robots.txt"));
        assert!(empty.contains("`sites`"), "does not say how to search elsewhere");
    }

    #[test]
    fn results_are_numbered_with_url_and_snippet() {
        let hits = vec![
            query::Result_ { url: "https://x.example/1".into(), title: "First".into(), snippet: "one".into(), score: 2.0 },
            query::Result_ { url: "https://x.example/2".into(), title: String::new(), snippet: "two".into(), score: 1.0 },
        ];
        let out = render("q", &hits, &Timing::default());
        assert!(out.contains("1. First"));
        assert!(out.contains("https://x.example/1"));
        // A page with no title is listed by its URL rather than blank.
        assert!(out.contains("2. https://x.example/2"), "{out}");
    }
}
