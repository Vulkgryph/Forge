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

/// Sites crawled when the caller names none.
///
/// Small and technical on purpose: a general crawl of the web from a handful
/// of seeds is a research project, while a crawl of the documentation an agent
/// actually asks about is useful immediately.
///
/// They are no longer crawled speculatively, and the reason is a measurement.
/// A real headless run asked what engine oil a Ford 8N takes, sent
/// `"sites": []`, and this list sent it to crawl the Rust standard library
/// documentation — a hundred and twenty pages, two minutes, no results, and a
/// hundred and twenty pages of Rust docs left in the index to be matched
/// against later questions. Crawling the wrong corpus is worse than crawling
/// nothing, because it costs the time *and* pollutes what comes next.
///
/// So with no sites given the index is searched and nothing is fetched. The
/// same run showed why that is the right trade: asked for sites, the agent
/// named tractordata.com, ntractorclub.com, myfordtractors.com and
/// yesterdaystractors.com without being told any of them. The model knows
/// where to look; it only has to be asked.
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
            // Names lowercased, values skipped when they are not UTF-8. The
            // one that matters is `cf-mitigated`, which says a bot-management
            // challenge was served rather than the page — see
            // `Fetched::challenge`.
            let headers: Vec<(String, String)> = response
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str().ok().map(|v| (k.as_str().to_ascii_lowercase(), v.to_string()))
                })
                .collect();
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
                        headers,
                        // Deliberately oversized, so the crawler's own
                        // `max_page_bytes` rejects it and counts it. Reporting
                        // a short body would have it indexed as a short page.
                        body: "x".repeat(self.max_bytes + 1),
                    });
                }
            }

            let body = response.text().await.unwrap_or_default();
            Ok(Fetched { status, final_url, content_type, headers, body })
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
    /// Pages refused by a bot-management challenge, and the hosts that did it.
    ///
    /// Reported separately from everything else because the response is
    /// different: no rewording or re-crawling reaches a page behind a
    /// challenge, and the model should stop trying rather than spend the turn
    /// on it.
    pub challenged: usize,
    pub challenged_hosts: Vec<String>,
    /// Pages dropped from the index to keep it bounded. Reported rather than
    /// silent: an agent that queried a page last week and cannot find it now
    /// should be able to tell eviction from the page having changed.
    pub evicted: usize,
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

    // Seeds from the query when it names a site, so "search docs.rs for
    // tokio" reaches somewhere the default list does not. A query that names
    // no site uses the configured defaults.
    // An empty array counts as "no preference", not as "crawl nothing".
    //
    // A model that does not want to constrain the crawl writes `"sites": []`,
    // which is a reasonable way to say it. Taken literally that produced a
    // crawl with no seeds and the tool failed outright with "no usable seed
    // URLs in []" — observed on the first call of a real headless run, where
    // it cost the agent a turn before it started guessing sites by hand.
    let given: Vec<String> = args
        .get("sites")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str()).map(String::from).collect())
        .unwrap_or_default();
    let asked_for_sites = !given.is_empty();
    let seeds: Vec<String> = if asked_for_sites {
        given
    } else {
        DEFAULT_SEEDS.iter().map(|s| s.to_string()).collect()
    };


    let handle = tokio::runtime::Handle::current();
    // The whole crawl-and-search runs off the runtime's worker threads,
    // because the fetcher blocks and blocking a worker starves everything
    // else the agent is doing.
    let result = tokio::task::spawn_blocking(move || {
        run(&query_text, &seeds, asked_for_sites, index_path, handle)
    })
    .await
    .map_err(|e| anyhow::anyhow!("search task failed: {e}"))?;

    result
}

/// Most pages kept in the on-disk index.
///
/// Raised from 600, which was set to save seven megabytes and was the wrong
/// thing to be saving. What a person reads in a lifetime is small: at 250
/// words a minute, two hours a day for fifty years is 548 million words, about
/// 3.3 GB of text. Ten web pages a day for fifty years is 182,500 pages — 2.9
/// GB at the sixteen kilobytes an indexed page costs here.
///
/// So storage was never the constraint, and 600 pages is about two months of
/// reading. The thing of value in this file is precisely that it does not
/// forget a page somebody already read, and the old cap threw that away to
/// save less disk than a single photograph.
///
/// Fifty thousand, which is thirteen years at ten pages a day and about eight
/// hundred megabytes at the limit. Not a lifetime, and deliberately: this
/// index is per project, so a lifetime of reading spread over many projects
/// wants one shared index instead — a separate decision, and not one to make
/// by quietly growing a per-project file.
///
/// What is now the constraint is query time, not size. A broad single-term
/// query against 50,000 pages takes about six seconds today, because it scores
/// every document to return ten. That is the next thing to fix, and it is the
/// reason this is fifty thousand rather than five hundred thousand.
const MAX_INDEXED_PAGES: usize = 50_000;

/// How many passages to return.
///
/// Not a parameter, deliberately. Every setting a tool exposes is a decision
/// the model has to make correctly, and this is one it has no basis for — a
/// real headless run chose page budgets of 120, 150 and 200 on consecutive
/// calls with no reason to prefer any of them, and 200 pages is over three
/// minutes of crawling. The tools this is modelled on take a query and
/// nothing else for the same reason.
const MAX_RESULTS: usize = 5;

fn run(
    query_text: &str,
    seeds: &[String],
    asked_for_sites: bool,
    index_path: std::path::PathBuf,
    handle: tokio::runtime::Handle,
) -> Result<String> {
    let max_results = MAX_RESULTS;
    let max_pages = DEFAULT_MAX_PAGES;
    let mut timing = Timing::default();

    // An index that cannot be read is replaced rather than fatal. It is a
    // cache of pages that can be fetched again, and refusing to search because
    // a cache file is corrupt would be the wrong trade.
    let mut index = Index::load(&index_path).unwrap_or_else(|_| Index::new());

    let search_start = std::time::Instant::now();
    let mut hits = query::search(&index, query_text, max_results);
    timing.search_ms = search_start.elapsed().as_millis();

    // Crawl when the index cannot answer — or when the caller named sites the
    // index has never read.
    //
    // The second half is the important one. Naming `sites` is an instruction
    // to go and read them, and gating the crawl purely on whether the current
    // index answers the query threw that instruction away: a stale index
    // matched, so the crawl was skipped and the results came from whatever had
    // been crawled before. Observed in a real headless run, where the agent
    // asked for two tractor forums and got pages from a site it had not named,
    // four calls in a row, and never once saw the source it asked for.
    //
    // Phrased as "has this site been read" rather than "were sites named", so
    // asking twice for the same site is answered from the index instead of
    // fetching it again.
    let decision = should_crawl(!hits.is_empty(), asked_for_sites, hosts_already_read(&index, seeds));
    if decision {
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
        timing.challenged = report.challenged;
        timing.challenged_hosts = report.challenged_hosts.clone();
        timing.timed_out = report.timed_out;

        // Bounded before saving, so the file next to the project cannot grow
        // without limit. Oldest pages go first — see `Index::trim_to`.
        let dropped = index.trim_to(MAX_INDEXED_PAGES);
        if dropped > 0 {
            timing.evicted = dropped;
        }

        // Saved even when the search that follows finds nothing: the pages
        // were fetched, and throwing them away means fetching them again for
        // the next query.
        let _ = crate::workdir::ensure_parent_of(&index_path);
        let _ = index.save(&index_path);

        let again = std::time::Instant::now();
        hits = query::search(&index, query_text, max_results);
        timing.search_ms += again.elapsed().as_millis();
    }

    timing.index_size = index.len();
    timing.results = hits.len();
    // Where the sources agree or differ, in the same call. The agent should
    // not have to know a second tool exists, or make a second round trip, to
    // find out that a question is contested.
    let spread = forge_search::query::spread(&index, query_text, 25);
    Ok(render(query_text, &hits, &spread, &timing))
}

/// Whether to fetch anything, given what the index could already do.
///
/// Two rules, both learned from a real headless run.
///
/// Crawling only happens toward sites somebody chose. Without that condition
/// an unanswerable query crawls the default list whatever the question was
/// about — see [`DEFAULT_SEEDS`] for the two minutes of Rust documentation
/// that bought nothing for a question about a tractor.
///
/// And naming a site the index has not read is reason enough on its own, even
/// when the index does answer the query. Gating purely on whether the current
/// index answers threw the instruction away: a stale index matched, the crawl
/// was skipped, and the results came from a site the caller never named. Four
/// calls in a row, in the run this is taken from.
fn should_crawl(index_answered: bool, asked_for_sites: bool, hosts_read: bool) -> bool {
    asked_for_sites && (!index_answered || !hosts_read)
}

/// Whether the index already holds a page from every host named in `seeds`.
///
/// Host-level rather than URL-level: a seed is a starting point for a crawl,
/// not a page anyone asked for by name, so having read the site is what makes
/// re-crawling it pointless.
fn hosts_already_read(index: &Index, seeds: &[String]) -> bool {
    let read: std::collections::HashSet<String> = index
        .urls()
        .filter_map(|u| forge_search::url::Url::parse(u).ok().map(|p| p.host))
        .collect();
    seeds.iter().all(|seed| {
        forge_search::url::Url::parse(seed)
            .map(|u| read.contains(&u.host))
            // An unparseable seed is reported by the crawl itself; it should
            // not make this claim the site was read.
            .unwrap_or(false)
    })
}

/// The tool result the model sees.
///
/// The timing line is for the model as much as for a person: a search that
/// crawled forty pages to answer a question is a search whose next call will
/// be instant, and an agent that knows that will not avoid the tool for being
/// slow.
fn render(
    query_text: &str,
    hits: &[query::Result_],
    spread: &[forge_search::query::Mention],
    timing: &Timing,
) -> String {
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
            render_challenges(&mut out, timing);
        } else {
            out.push_str(&format!(
                "The index holds {} page(s) and none matched, and no crawl was attempted \
                 because no `sites` were given. This tool only knows what it has been pointed \
                 at — name the sites worth reading for this question in `sites` and call again. \
                 Guessing is fine; a site that turns out to be wrong costs one call.\n",
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
    render_challenges(&mut out, timing);
    render_spread(&mut out, spread);

    if timing.crawled {
        out.push_str(&format!(
            "[crawled {} pages in {}ms{}; index now {} pages{}; search {}ms. \
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
            if timing.evicted > 0 {
                format!(" ({} oldest dropped to stay under the cap)", timing.evicted)
            } else {
                String::new()
            },
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

/// Sites that refused the crawler rather than answering it.
///
/// Said plainly and separately, because the useful response is different from
/// every other failure. A page that 404s might be at another address and a
/// query that matches nothing might match different words, so trying again is
/// reasonable in both cases. A page behind a bot-management challenge is at
/// the right address and no wording reaches it — the server is willing to
/// serve it to a browser and not to us. An agent not told the difference
/// spends the turn rephrasing.
fn render_challenges(out: &mut String, timing: &Timing) {
    if timing.challenged == 0 {
        return;
    }
    out.push_str(&format!(
        "{} page(s) were refused by a bot check rather than served",
        timing.challenged,
    ));
    if !timing.challenged_hosts.is_empty() {
        out.push_str(&format!(" ({})", timing.challenged_hosts.join(", ")));
    }
    out.push_str(
        ". Those pages need a browser, so rewording the query or crawling again will not \
         reach them — say so and use what else you have, or ask the user to open the page. \
         Other sites in this search were unaffected.\n\n",
    );
}

/// What several sources say, when several of them say something.
///
/// Ranking answers which passage is most relevant. It does not answer whether
/// the sources agree, and for a contested question that is the thing being
/// asked — what oil an old engine takes has more than one answer, and the
/// useful reply is the spread and not whichever passage scored highest.
///
/// Written only when at least two sources concur on something. A list of terms
/// each mentioned once is the ranking again in a worse format, and padding the
/// result with it costs context for nothing.
fn render_spread(out: &mut String, spread: &[forge_search::query::Mention]) {
    let corroborated: Vec<&forge_search::query::Mention> =
        spread.iter().filter(|m| m.sources.len() > 1).take(8).collect();
    if corroborated.is_empty() {
        return;
    }
    out.push_str("Mentioned by more than one source:\n");
    for m in corroborated {
        out.push_str(&format!(
            "   {} — {} sources ({})\n",
            m.term,
            m.sources.len(),
            m.sources.join(", "),
        ));
    }
    out.push('\n');
}

/// Where the index lives for a workspace.
pub fn index_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root.join(".forge").join("search-index.bin")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without sites, nothing is fetched.
    ///
    /// Measured: a real run asked about a tractor, sent `"sites": []`, and the
    /// default list sent it to crawl the Rust standard library docs for two
    /// minutes — no results, and a hundred and twenty pages of Rust docs left
    /// in the index to be matched against later questions. Crawling the wrong
    /// corpus costs the time and then pollutes what comes next, so it is worse
    /// than crawling nothing.
    #[test]
    fn nothing_is_fetched_when_no_sites_were_named() {
        // index cannot answer, no sites named: still no crawl.
        assert!(!should_crawl(false, false, false));
        // index can answer, no sites named: no crawl either.
        assert!(!should_crawl(true, false, false));
    }

    /// A named site the index has not read is crawled even when the index
    /// answers the query — otherwise the instruction is discarded, which is
    /// what happened for four calls running in the run this comes from.
    #[test]
    fn a_named_unread_site_is_crawled_even_when_the_index_answers() {
        assert!(should_crawl(true, true, false));
        assert!(should_crawl(false, true, false));
    }

    /// And a site already read is answered from the index rather than fetched
    /// again, so asking twice is cheap.
    #[test]
    fn a_named_site_already_read_is_not_refetched() {
        assert!(!should_crawl(true, true, true));
        // Unless the index cannot actually answer, in which case there is
        // nothing to lose by looking again.
        assert!(should_crawl(false, true, true));
    }

    /// An empty `sites` array means "no preference", not "crawl nothing".
    ///
    /// Observed on the first call of a real headless run: the agent wrote
    /// `"sites": []`, which is a fair way to say it does not want to constrain
    /// the crawl, and the tool failed with "no usable seed URLs in []".
    #[test]
    fn an_empty_sites_array_falls_back_to_the_defaults() {
        let args = serde_json::json!({ "query": "x", "sites": [] });
        let given: Vec<String> = args
            .get("sites")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|s| s.as_str()).map(String::from).collect())
            .unwrap_or_default();
        assert!(given.is_empty(), "the fixture does not reproduce the shape");
        // Which is the branch the tool now takes.
        let seeds: Vec<String> = if given.is_empty() {
            DEFAULT_SEEDS.iter().map(|s| s.to_string()).collect()
        } else {
            given
        };
        assert!(!seeds.is_empty(), "an empty array still produced no seeds");
    }

    /// Naming sites is an instruction to read them. The index answering the
    /// query is not a reason to ignore it.
    ///
    /// This is the failure it pins down: a real headless run asked for two
    /// tractor forums and got pages from a site it had not named, four calls
    /// running, because a stale index matched and the crawl was skipped.
    #[test]
    fn a_site_the_index_has_never_read_is_still_crawled() {
        let mut index = Index::new();
        index.add("https://already.test/page", "Oil", "", "engine oil is straight 30 weight");
        // The index answers the query, but not from the site being asked for.
        assert!(!query::search(&index, "engine oil", 3).is_empty());
        assert!(
            !hosts_already_read(&index, &["https://never-read.test/board".to_string()]),
            "a site that was never crawled was treated as read",
        );
        // And a site it has read is not fetched again.
        assert!(hosts_already_read(&index, &["https://already.test/other".to_string()]));
    }

    /// A seed that does not parse must not count as read, or a typo would
    /// silently skip the crawl it was meant to start.
    #[test]
    fn an_unparseable_seed_does_not_count_as_read() {
        let mut index = Index::new();
        index.add("https://a.test/p", "", "", "text");
        assert!(!hosts_already_read(&index, &["not a url".to_string()]));
    }

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
        let crawled = render("q", &hits, &[], &Timing {
            crawled: true, fetched: 40, crawl_ms: 9000, index_size: 40, search_ms: 2,
            ..Default::default()
        });
        assert!(crawled.contains("crawled 40 pages"));
        assert!(crawled.contains("Later searches use the index"));

        let cached = render("q", &hits, &[], &Timing {
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
        let empty = render("q", &[], &[], &Timing {
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
        let out = render("q", &hits, &[], &Timing::default());
        assert!(out.contains("1. First"));
        assert!(out.contains("https://x.example/1"));
        // A page with no title is listed by its URL rather than blank.
        assert!(out.contains("2. https://x.example/2"), "{out}");
    }
}
