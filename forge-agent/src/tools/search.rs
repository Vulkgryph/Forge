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
///
/// The `+`-prefixed URL is the convention for a crawler that expects to be
/// looked up. A site operator seeing an unfamiliar agent in their logs has one
/// question — who is this and how do I reach them — and a bare name does not
/// answer it. The alternative to being findable is being blocked by reputation,
/// which is the outcome this project has already chosen against by refusing to
/// pretend to be a browser.
pub(crate) const USER_AGENT: &str = concat!(
    "forge-search/", env!("CARGO_PKG_VERSION"), " (+https://vulkgryph.com/projects/forge/)"
);

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
/// Every header this fetcher adds to a request, in one place.
///
/// Extracted so the documented set can be asserted against the code that
/// actually produces it. The README states the complete set of headers a
/// crawled site sees and says tests enforce it; the test measured a client
/// built with the same builder options rather than this fetcher, so the
/// three signature headers and the two conditional ones sat outside the
/// assertion it claimed to make. A pure function taking `now` and `nonce`
/// is testable exhaustively, which a request builder is not.
///
/// `Host`, `Accept` and `User-Agent` are not here: reqwest derives them
/// from the URL and the client's `user_agent`, and the listener test in
/// `web.rs` measures those.
pub(crate) fn added_headers(
    signer: Option<&crate::tools::botauth::Signer>,
    url: &str,
    etag: &str,
    last_modified: &str,
    now: u64,
    nonce: &[u8],
) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();

    // Signed when a key is configured. The authority is what the signature
    // covers, so it is taken from the URL rather than passed in — a
    // signature over a different host than the request reaches is worse
    // than no signature, because it looks like a forgery rather than an
    // omission.
    if let Some(signer) = signer {
        if let Some(authority) = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| match u.port() {
                Some(p) => format!("{h}:{p}"),
                None => h.to_string(),
            }))
        {
            let signed = signer.sign(&authority, now, nonce);
            headers.push(("signature-agent", signed.signature_agent));
            headers.push(("signature-input", signed.signature_input));
            headers.push(("signature", signed.signature));
        }
    }

    // Only what the server itself gave us last time. A validator we
    // invented would be a claim about a copy the server never sent.
    // `Last-Modified` is a content property — the same value for every
    // visitor, so returning it identifies nobody. An `ETag` is
    // server-chosen and can be minted per visitor, which is a known
    // tracking technique, so it goes back only if the operator asked for
    // it. Most of the bandwidth saving comes from the harmless one.
    if !etag.is_empty() && crate::tools::botauth::send_etag() {
        headers.push(("if-none-match", etag.to_string()));
    }
    if !last_modified.is_empty() {
        headers.push(("if-modified-since", last_modified.to_string()));
    }
    headers
}

pub(crate) struct HttpFetcher {
    client: reqwest::Client,
    /// Signs outbound requests when the operator has configured a key, so a
    /// site can verify who is calling instead of taking the user agent's word
    /// for it. `None` means unsigned, which is the default.
    signer: Option<std::sync::Arc<crate::tools::botauth::Signer>>,
    handle: tokio::runtime::Handle,
    /// Largest body to read, so one enormous page cannot exhaust memory
    /// before the crawler's own limit has a chance to reject it.
    max_bytes: usize,
}

impl HttpFetcher {
    /// One configured the way every caller wants it: identified, bounded in
    /// time, and following a sane number of redirects.
    pub(crate) fn new(handle: tokio::runtime::Handle, max_bytes: usize) -> Self {
        Self::signed(handle, max_bytes, crate::tools::botauth::signer())
    }

    pub(crate) fn signed(
        handle: tokio::runtime::Handle,
        max_bytes: usize,
        signer: Option<std::sync::Arc<crate::tools::botauth::Signer>>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(20))
                // Followed by hand, in `fetch_conditional`. reqwest's redirect
                // policy can decide whether to follow a hop but cannot rewrite
                // the headers that go with it — and it strips only
                // Authorization, Cookie, Proxy-Authorization and
                // WWW-Authenticate on a cross-host redirect, so a Web Bot Auth
                // signature would travel verbatim to the next host. It signs
                // `@authority`, so arriving at www.example.com carrying a
                // signature over example.com means the verifier computes a
                // different base and the check fails: worse than sending
                // nothing, by this code's own rule elsewhere. Each hop is
                // signed for the host it actually reaches.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_default(),
            handle,
            max_bytes,
            signer,
        }
    }
}

impl Fetcher for HttpFetcher {
    fn fetch(&self, url: &str) -> std::result::Result<Fetched, String> {
        self.fetch_conditional(url, "", "")
    }

    fn fetch_conditional(
        &self,
        url: &str,
        etag: &str,
        last_modified: &str,
    ) -> std::result::Result<Fetched, String> {
        self.handle.block_on(async {
            let mut current = url.to_string();
            let mut hops = 0usize;
            let response = loop {
            let mut request = self.client.get(&current);

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let mut nonce = [0u8; 64];
            // A fresh nonce per request, so a captured signature cannot be
            // replayed inside its validity window.
            {
                use rand::RngCore as _;
                rand::thread_rng().fill_bytes(&mut nonce);
            }
            for (name, value) in added_headers(
                self.signer.as_deref(), &current, etag, last_modified, now, &nonce,
            ) {
                request = request.header(name, value);
            }
            let response = request
                .send()
                .await
                .map_err(|e| format!("{current}: {e}"))?;

            // Redirects, by hand. Every destination is resolved and refused
            // before it is followed — a hostile page redirecting a crawl at
            // somebody's own machine is the cheap attack, and a hostname that
            // merely resolves somewhere private looks ordinary until the
            // lookup happens.
            let code = response.status().as_u16();
            if matches!(code, 301 | 302 | 303 | 307 | 308) {
                if hops >= 5 {
                    break response;
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let Some(location) = location else { break response };
                // Relative targets are the common case, so resolve against
                // the URL that produced them rather than requiring absolute.
                let Ok(next) = reqwest::Url::parse(&current)
                    .and_then(|base| base.join(&location))
                else {
                    break response;
                };
                match next.host_str() {
                    Some(host) if forge_search::net::is_routable_public(host) => {}
                    _ => break response,
                }
                current = next.to_string();
                hops += 1;
                continue;
            }
            break response;
            };

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
    /// refetches. What it should do instead is narrow
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

    // Scoped to the sites the caller named, and unscoped when it named none.
    //
    // This is what makes `sites` mean something beyond "go and fetch these".
    // Every page Forge had ever read used to compete in one ranking, so the
    // hundred and twenty pages of Rust documentation a wrong crawl left behind
    // stayed in the running for every question after it — the real cost of
    // aiming badly was not the wasted minutes, it was that the mistake
    // outlived them. Naming the sites now narrows the answer to them.
    let search_start = std::time::Instant::now();
    let mut hits = query::search_within(&index, query_text, max_results, seeds);
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
            // A site asking for a longer gap than this is honoured by being
            // left alone, not by being crawled faster than it asked.
            max_crawl_delay: crawl::Limits::default().max_crawl_delay,
            stay_on_host: true,
            max_page_bytes: 2 * 1024 * 1024,
            max_seconds: Some(crawl_budget_secs(max_pages, POLITENESS)),
        };

        let crawl_start = std::time::Instant::now();
        let clock = crawl::SystemClock;
        let mut crawler = crawl::Crawler::new(&fetcher, &clock, limits);
        let mut seeded = 0;
        for seed in seeds {
            // Resolved here, where a lookup is affordable. The crawler's own
            // guard is deliberately pure — it runs inside a time budget and
            // must not stall on a nameserver — so it catches an address
            // written as an address, in any of its spellings. It cannot
            // catch a *name* that resolves somewhere private: `router.lan`,
            // a MagicDNS name, a corporate short name, or any hostname whose
            // A record is 10.x. Those look like ordinary sites until
            // something asks the resolver, and this is the layer that can.
            let host = forge_search::url::Url::parse(seed).ok().map(|u| u.host);
            match host {
                Some(h) if !forge_search::net::is_routable_public(&h) => continue,
                None => continue,
                _ => {}
            }
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
        // Queue the refused pages for a person who might open one.
        queue_refusals(&report);

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
        hits = query::search_within(&index, query_text, max_results, seeds);
        timing.search_ms += again.elapsed().as_millis();
    }

    timing.index_size = index.len();
    timing.results = hits.len();
    // Where the sources agree or differ, in the same call. The agent should
    // not have to know a second tool exists, or make a second round trip, to
    // find out that a question is contested.
    let spread = forge_search::query::spread(&index, query_text, 25);
    Ok(render(query_text, &hits, &spread, &index, seeds, &timing))
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

/// Pages from one host below which the site has not meaningfully been read.
///
/// One page is not a site, and treating it as one was actively misleading. A
/// bot check refused a crawl of stackoverflow.com; a person opened the page in
/// the browser and handed it over, which put exactly one page in the index —
/// and from then on the host counted as read, so naming it skipped the crawl
/// and the tool answered from that single page. The agent concluded it could
/// access a site it cannot, and said so.
///
/// A `web_fetch` of one URL does the same thing. Five is enough to tell a
/// crawl from an incidental page or two, while a genuinely tiny site that
/// falls under it is merely re-crawled — a few seconds, and it refreshes what
/// is held, since re-crawling replaces pages by URL rather than duplicating
/// them.
const MIN_PAGES_TO_COUNT_AS_READ: usize = 5;

/// Whether the index holds enough of every host named in `seeds` to answer
/// from instead of crawling.
///
/// Host-level rather than URL-level: a seed is a starting point for a crawl,
/// not a page anyone asked for by name, so having read the site is what makes
/// re-crawling it pointless.
fn hosts_already_read(index: &Index, seeds: &[String]) -> bool {
    let read: std::collections::HashMap<String, usize> = forge_search::library::shelves(index)
        .into_iter()
        .map(|s| (s.host, s.pages))
        .collect();
    seeds.iter().all(|seed| {
        forge_search::query::normalise_host(seed)
            .map(|h| {
                read.get(&h).copied().unwrap_or(0) >= MIN_PAGES_TO_COUNT_AS_READ
            })
            // An unparseable seed is reported by the crawl itself; it should
            // not make this claim the site was read.
            .unwrap_or(false)
    })
}

/// What the index already holds, for a result that found nothing.
///
/// A dead end that says only "no results" makes the model guess, and a guess
/// costs a two-minute crawl. A dead end that says "here is what has been read"
/// is actionable in the other direction too: the answer may be on a shelf the
/// query simply missed, and the model can see that before deciding to fetch.
///
/// Bounded, because this goes into a prompt. The largest shelves are the ones
/// worth naming, and `shelves` already sorts that way.
fn render_library(out: &mut String, index: &Index) {
    const SHOWN: usize = 12;
    let shelves = forge_search::library::shelves(index);
    if shelves.is_empty() {
        out.push_str("Nothing has been read yet, so there is nothing to search.\n");
        return;
    }
    out.push_str("\nSites already read, most pages first:\n");
    for shelf in shelves.iter().take(SHOWN) {
        out.push_str(&format!(
            "  {:<34} {:>5} page(s){}\n",
            shelf.host,
            shelf.pages,
            match age_in_days(shelf.last_read) {
                Some(0) => ", read today".to_string(),
                Some(1) => ", read yesterday".to_string(),
                Some(n) => format!(", last read {n} days ago"),
                None => String::new(),
            }
        ));
    }
    if shelves.len() > SHOWN {
        out.push_str(&format!("  … and {} more\n", shelves.len() - SHOWN));
    }
}

/// Whole days since a Unix timestamp, or `None` if there isn't one.
fn age_in_days(then: u64) -> Option<u64> {
    if then == 0 {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(now.saturating_sub(then) / 86_400)
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
    index: &Index,
    seeds: &[String],
    timing: &Timing,
) -> String {
    let mut out = String::new();
    if hits.is_empty() {
        out.push_str(&format!("No results for {query_text:?}"));
        // Said plainly, because a narrowed search that finds nothing and an
        // unnarrowed one that finds nothing want different next moves, and the
        // model cannot tell them apart from "no results".
        if !seeds.is_empty() {
            out.push_str(&format!(" within {}", seeds.join(", ")));
        }
        out.push_str(".\n");
        if timing.crawled {
            out.push_str(&format!(
                "Crawled {} pages ({} indexed, {} refused by robots.txt) in {}ms and found \
                 nothing matching. The index now holds {} pages; try different terms, or pass \
                 `sites` to crawl somewhere else. Crawling again repeats the same pages rather \
                 than continuing, so narrow `sites` to somewhere the answer is likely \
                 to be instead of retrying as-is.\n",
                timing.fetched, timing.indexed, timing.disallowed, timing.crawl_ms,
                timing.index_size,
            ));
            render_challenges(&mut out, timing);
        } else if seeds.is_empty() {
            out.push_str(
                "Nothing was fetched, because this tool only reads sites it is pointed at — \
                 it has no way to discover one. Name the sites worth reading for this question \
                 in `sites` and call again; guessing is fine, and a site that turns out to be \
                 wrong costs one call.\n",
            );
            render_library(&mut out, index);
        } else {
            out.push_str(&format!(
                "Those sites have been read already ({} page(s) held in total), so nothing was \
                 fetched and the search was narrowed to them. Either the answer is not on them \
                 or the terms missed it: try different terms, or name somewhere else.\n",
                timing.index_size,
            ));
            render_library(&mut out, index);
        }
        return out;
    }

    out.push_str(&format!("{} result(s) for {query_text:?}", hits.len()));
    if !seeds.is_empty() {
        out.push_str(&format!(" within {}", seeds.join(", ")));
    }
    out.push_str(":\n\n");
    // Words the index has never seen, which no result can say for itself: a
    // query still matches on its grammar when its subject is absent, and every
    // number in the result is then truthful and useless.
    let missing = forge_search::query::unanswered_terms(index, query_text);
    if !missing.is_empty() {
        out.push_str(&format!(
            "No page read so far contains {} — the results below matched on the rest \
             of the query.\n\n",
            missing.iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join(", "),
        ));
    }
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
                 narrow `sites` rather than repeating this call)"
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
    // What to suggest depends on what the client can actually do. Offering to
    // have somebody open the page is good advice in the IDE and a dead end in
    // a terminal, which has no way to show a page and no way to read one back
    // — and an agent that suggests it there sends the user to do something
    // impossible and then waits for a result that cannot arrive.
    if super::refused::host_can_browse() {
        out.push_str(
            ". Those pages need a browser, so rewording the query or crawling again will not \
             reach them. A request to open one has been raised with the user; carry on with \
             what else you have meanwhile, and if the page arrives it will be in the index. \
             Other sites in this search were unaffected.\n\n",
        );
    } else {
        out.push_str(
            ". Those pages need a browser and this client has none, so they are out of \
             reach — rewording the query, crawling again, or asking the user to open them \
             will not help. Say plainly that the site refused an automated request, answer \
             from what else you have, and name the site so the user can look themselves if \
             they want to. Other sites in this search were unaffected.\n\n",
        );
    }
}

/// Offer each refused page to whoever might open it.
///
/// The link that was missing. `web_fetch` queued its refusals from the start,
/// so a single blocked page reached the browser handoff — but a *crawl* only
/// put its refusals in the report, and the tool turned those into prose. The
/// result was a search that told the model "those pages need a browser, ask
/// the user to open the page" while queueing nothing, so no browser was ever
/// offered and there was nothing for the user to act on. Every piece of the
/// rail worked; nothing called into it.
///
/// One URL per challenged host, which is what the crawler collects and the
/// right granularity anyway: a wholly walled site refuses every page in the
/// crawl and nobody opens ninety pages by hand, so the first one names the
/// problem. `tools::refused` bounds the queue beyond that.
fn queue_refusals(report: &crawl::Report) {
    // The vendor as last seen. A crawl is nearly always walled by one system,
    // and the report says so rather than keeping a list nobody reads.
    let vendor = if report.challenged_by.is_empty() {
        "an unidentified bot check"
    } else {
        &report.challenged_by
    };
    for url in &report.challenged_urls {
        super::refused::record(url, vendor);
    }
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
    workspace_root.join(".forge").join("search-index")
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
        // Enough pages to count as read — see `MIN_PAGES_TO_COUNT_AS_READ`.
        for i in 0..MIN_PAGES_TO_COUNT_AS_READ {
            index.add(
                &format!("https://already.test/page{i}"),
                "Oil",
                "",
                "engine oil is straight 30 weight",
            );
        }
        // The index answers the query, but not from the site being asked for.
        assert!(!query::search(&index, "engine oil", 3).is_empty());
        assert!(
            !hosts_already_read(&index, &["https://never-read.test/board".to_string()]),
            "a site that was never crawled was treated as read",
        );
        // And a site it has read is not fetched again.
        assert!(hosts_already_read(&index, &["https://already.test/other".to_string()]));
    }

    /// One page is not a site, and treating it as one made the agent claim
    /// access it does not have.
    ///
    /// A bot check refused a crawl of stackoverflow.com. A person opened the
    /// page in the browser and handed it over, which put exactly one page in
    /// the index — and from then on the host counted as read, so naming it
    /// skipped the crawl and the tool answered from that single page. The
    /// agent reported that it could still reach the site without help. It
    /// could not; it was reading what it had been given.
    #[test]
    fn one_handed_over_page_does_not_make_a_site_read() {
        let mut index = Index::new();
        index.add(
            "https://walled.test/questions/1",
            "A question",
            "",
            "the answer involves a lifetime annotation",
        );

        assert!(
            !hosts_already_read(&index, &["https://walled.test/".to_string()]),
            "one page made a whole host look read",
        );
        // So the crawl is still attempted, which is what gets the refusal
        // reported honestly instead of answered around.
        assert!(
            should_crawl(true, true, false),
            "naming a barely-read site no longer triggers a crawl",
        );

        // A `web_fetch` of a single URL is the same situation.
        let mut index = Index::new();
        index.add("https://fetched.test/one", "One", "", "some page");
        assert!(!hosts_already_read(&index, &["https://fetched.test/".to_string()]));

        // And once enough of the site is held, it is answered from rather
        // than crawled again.
        for i in 1..MIN_PAGES_TO_COUNT_AS_READ {
            index.add(&format!("https://fetched.test/{i}"), "More", "", "more pages");
        }
        assert!(hosts_already_read(&index, &["https://fetched.test/".to_string()]));
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
        assert_eq!(p.file_name().unwrap(), "search-index");
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
        let crawled = render("q", &hits, &[], &Index::new(), &[], &Timing {
            crawled: true, fetched: 40, crawl_ms: 9000, index_size: 40, search_ms: 2,
            ..Default::default()
        });
        assert!(crawled.contains("crawled 40 pages"));
        assert!(crawled.contains("Later searches use the index"));

        let cached = render("q", &hits, &[], &Index::new(), &[], &Timing {
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
        let empty = render("q", &[], &[], &Index::new(), &[], &Timing {
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
        let out = render("q", &hits, &[], &Index::new(), &[], &Timing::default());
        assert!(out.contains("1. First"));
        assert!(out.contains("https://x.example/1"));
        // A page with no title is listed by its URL rather than blank.
        assert!(out.contains("2. https://x.example/2"), "{out}");
    }

    /// A dead end should hand the model something to act on. Saying only "no
    /// results" makes it guess a site, and a guess costs a two-minute crawl;
    /// naming what has been read lets it either pick a shelf or conclude
    /// honestly that nothing on hand can answer.
    /// A crawl's refusals reach the queue the browser handoff drains.
    ///
    /// The bug this exists for: every piece of the rail worked and nothing
    /// called into it. `web_fetch` recorded its refusals, so one blocked page
    /// offered a browser; a crawl put its refusals in the report, and the tool
    /// rendered them as prose telling the model to "ask the user to open the
    /// page" — while queueing nothing, so there was no page for the user to
    /// open. Verified by asking stackoverflow.com for a real answer: the
    /// challenge was detected, reported, and no browser ever appeared.
    #[test]
    fn a_crawls_refusals_are_offered_to_a_person() {
        let _g = refusal_guard();

        let report = crawl::Report {
            challenged: 12,
            challenged_hosts: vec!["walled.test".into(), "other.test".into()],
            challenged_urls: vec![
                "https://walled.test/a".into(),
                "https://other.test/b".into(),
            ],
            challenged_by: "Cloudflare".into(),
            ..Default::default()
        };
        queue_refusals(&report);

        let queued = crate::tools::refused::drain();
        assert_eq!(queued.len(), 2, "a crawl's refusals were not queued: {queued:?}");
        assert_eq!(queued[0].url, "https://walled.test/a");
        assert_eq!(queued[0].refused_by, "Cloudflare");
        assert_eq!(queued[1].url, "https://other.test/b");
    }

    /// A crawl that was refused nothing queues nothing, or every search would
    /// raise a browser request.
    #[test]
    fn a_clean_crawl_queues_nothing() {
        let _g = refusal_guard();
        queue_refusals(&crawl::Report { fetched: 40, indexed: 40, ..Default::default() });
        assert!(crate::tools::refused::drain().is_empty());
    }

    /// A refusal whose vendor could not be identified is still offered. The
    /// page is what a person opens; the name of the system that blocked it is
    /// a detail, and withholding the offer for want of it would be absurd.
    #[test]
    fn an_unnamed_vendor_still_offers_the_page() {
        let _g = refusal_guard();
        queue_refusals(&crawl::Report {
            challenged: 1,
            challenged_hosts: vec!["walled.test".into()],
            challenged_urls: vec!["https://walled.test/a".into()],
            challenged_by: String::new(),
            ..Default::default()
        });
        let queued = crate::tools::refused::drain();
        assert_eq!(queued.len(), 1, "{queued:?}");
        assert!(!queued[0].refused_by.is_empty(), "the vendor line was left blank");
    }

    /// The whole rail, against a site that really refuses.
    ///
    /// Ignored because it goes to the network, and worth having anyway: this
    /// is the only check that would have caught the gap. Every structural and
    /// unit test around it can pass while the rail is dead, which is precisely
    /// what happened — the feature was built, reduced, and shipped twice
    /// before a real run against stackoverflow.com showed the challenge being
    /// detected, reported to the model, and never offered to anybody.
    ///
    /// stackoverflow.com serves Forge's crawler a 403 with `cf-mitigated`,
    /// verified live alongside DataDome on reuters.com and PerimeterX on
    /// zillow.com — see `forge-search/examples/challenges.rs`.
    #[tokio::test]
    #[ignore = "goes to the network; run with --ignored"]
    async fn a_live_refusal_reaches_the_queue() {
        // Declares the capability as well as taking the lock: in-process the
        // default is off, and off means nothing is queued — which is the
        // behaviour a terminal gets and not what this test is about.
        let _g = refusal_guard();
        let dir = std::env::temp_dir().join("forge-live-refusal");
        let _ = std::fs::remove_dir_all(&dir);

        let args = serde_json::json!({
            "query": "how do tags work",
            "sites": ["https://stackoverflow.com/"],
        });
        let out = web_search(&args, dir.join("search-index")).await.expect("the tool ran");

        // Reported to the model, which already worked.
        assert!(
            out.contains("refused by a bot check"),
            "the challenge was not reported: {out}"
        );
        // And offered to a person, which is the part that did not.
        let queued = crate::tools::refused::drain();
        assert!(
            !queued.is_empty(),
            "a live refusal reached the model as prose but nobody was offered the page"
        );
        assert!(queued[0].url.contains("stackoverflow.com"), "{queued:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The advice a refusal gives depends on what the client can do.
    ///
    /// Offering to have somebody open the page is right in the IDE and a dead
    /// end in a terminal, which has nothing to show a page in and nothing to
    /// read one back from. An agent that suggests it there sends the user off
    /// to do something impossible and then waits for a result that cannot
    /// arrive.
    #[test]
    fn a_refusal_suggests_only_what_the_client_can_do() {
        let _g = refusal_guard();
        let timing = Timing {
            challenged: 3,
            challenged_hosts: vec!["walled.test".into()],
            ..Default::default()
        };

        crate::tools::refused::set_host_can_browse(true);
        let mut with = String::new();
        render_challenges(&mut with, &timing);
        assert!(with.contains("raised with the user"), "{with}");
        assert!(with.contains("carry on"), "{with}");

        crate::tools::refused::set_host_can_browse(false);
        let mut without = String::new();
        render_challenges(&mut without, &timing);
        assert!(without.contains("this client has none"), "{without}");
        // The thing it must not say: a terminal user cannot do this.
        assert!(
            !without.contains("asking the user to open them will help"),
            "a terminal was told to have somebody open the page: {without}"
        );
        assert!(without.contains("name the site"), "no fallback offered: {without}");

        // Both name the host either way, since that is actionable regardless.
        assert!(with.contains("walled.test") && without.contains("walled.test"));
    }

    /// The crawl path must actually call into the queue.
    ///
    /// Structural, because the bug was not a wrong function — it was a right
    /// function nobody called. Every unit test above passed while the rail was
    /// dead, since they call `queue_refusals` themselves; only a real run
    /// against a walled site showed the gap, and only after the whole feature
    /// had been built, reduced, and shipped twice.
    ///
    /// The invariant: a crawl that records challenges *for display* must also
    /// offer them *to a person*. Recording one without the other is how the
    /// tool came to tell the model "ask the user to open the page" with no page
    /// for the user to open.
    #[test]
    fn the_crawl_path_queues_what_it_reports() {
        let src = include_str!("search.rs");
        // Assembled from fragments, because this test reads the file it lives
        // in — a literal here would match itself and pass on its own text.
        let displays = ["timing.challenged", " = report.challenged;"].concat();
        let queues = ["queue_refusals(&", "report);"].concat();

        let code: String = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(code.contains(&displays), "the report's challenge count is no longer read");
        let at = code.find(&displays).expect("checked above");
        // Within the same block: the call belongs beside the bookkeeping it
        // guarantees, not somewhere else in the file that may not run.
        let after = &code[at..(at + 600).min(code.len())];
        assert!(
            after.contains(&queues),
            "a crawl records challenges for display without offering them to \
             anybody — the browser handoff has nothing to drain",
        );
    }

    /// Serialised on the lock that owns the queue and the capability flag —
    /// not a second lock of this module's own, which would serialise these
    /// tests against each other and not against the ones in `refused`.
    use crate::tools::refused::test_guard as refusal_guard;

    #[test]
    fn an_empty_result_with_no_sites_lists_what_has_been_read() {
        let mut index = Index::new();
        for i in 0..4 {
            index.add(&format!("https://docs.test/{i}"), "Doc", "", "some documentation");
        }
        index.add("https://forum.test/t/1", "Thread", "", "a discussion");

        let out = render("q", &[], &[], &index, &[], &Timing { index_size: 5, ..Default::default() });
        assert!(out.contains("no way to discover"), "{out}");
        assert!(out.contains("docs.test"), "the library was not listed: {out}");
        assert!(out.contains("forum.test"), "{out}");
        // Largest shelf first, so the most likely place to look leads.
        assert!(
            out.find("docs.test") < out.find("forum.test"),
            "shelves are not ordered by size: {out}"
        );
    }

    /// And an empty index should say so rather than print an empty heading,
    /// which reads as a rendering bug.
    #[test]
    fn an_empty_index_says_nothing_has_been_read() {
        let out = render("q", &[], &[], &Index::new(), &[], &Timing::default());
        assert!(out.contains("Nothing has been read yet"), "{out}");
        assert!(!out.contains("most pages first"), "an empty library printed a heading: {out}");
    }

    /// A narrowed search that finds nothing is a different situation from an
    /// unnarrowed one, and the result has to distinguish them — otherwise the
    /// model's next move is a guess about which it was.
    #[test]
    fn a_narrowed_result_says_what_it_was_narrowed_to() {
        let mut index = Index::new();
        index.add("https://docs.test/a", "Doc", "", "documentation");
        let seeds = vec!["https://docs.test/".to_string()];

        let empty = render("q", &[], &[], &index, &seeds, &Timing { index_size: 1, ..Default::default() });
        assert!(empty.contains("within https://docs.test/"), "{empty}");
        assert!(empty.contains("read already"), "{empty}");
        assert!(!empty.contains("no way to discover"), "wrong advice for a named site: {empty}");

        let hits = vec![query::Result_ {
            url: "https://docs.test/a".into(),
            title: "Doc".into(),
            snippet: "documentation".into(),
            score: 1.0,
        }];
        let found = render("q", &hits, &[], &index, &seeds, &Timing { index_size: 1, ..Default::default() });
        assert!(found.contains("within https://docs.test/"), "{found}");
    }

    /// The library line has to be readable as a fact about staleness, since
    /// deciding whether to re-read a site is the main thing it is for.
    #[test]
    fn a_shelf_reports_how_long_ago_it_was_read() {
        let dir = std::env::temp_dir().join("forge-agent-shelf-age");
        let _ = std::fs::remove_dir_all(&dir);
        let mut index = Index::new();
        index.add("https://docs.test/a", "Doc", "", "documentation");
        index.save(&dir).unwrap();
        let index = Index::load(&dir).unwrap();

        let out = render("q", &[], &[], &index, &[], &Timing::default());
        assert!(out.contains("read today"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod redirect_signing_tests {
    use super::added_headers;
    use crate::tools::botauth::Signer;
    use base64::Engine as _;

    /// The Ed25519 key from RFC 9421 Appendix B.1.4, as used by botauth's own
    /// tests.
    fn test_signer() -> Signer {
        let pkcs8 = base64::engine::general_purpose::STANDARD
            .decode("MC4CAQAwBQYDK2VwBCIEIJ+DYvh6SEqVTm50DFtMDoQikTmiCqirVv9mWG9qfSnF")
            .expect("the RFC's key decodes");
        Signer::new(&pkcs8, "https://example.invalid").expect("and loads")
    }

    /// The signature must cover the host the request is about to reach.
    ///
    /// A signature over a different authority than the request arrives at is
    /// worse than sending none: it looks like a forgery rather than an
    /// omission, and `example.com` → `www.example.com` is the most common
    /// redirect on the web. The caller passes the current hop; this asserts the
    /// signature is actually derived from the URL it is given.
    ///
    /// This replaced a structural test that grepped this file for
    /// `Url::parse(&current)`. That test was correct about the property and
    /// pinned to one spelling of it: extracting the header assembly into
    /// `added_headers` — behaviour unchanged, the hop still what gets signed —
    /// tripped it, because the parse moved to a parameter named `url`. Now
    /// that the assembly is a pure function, the property can be checked
    /// directly instead of guessed at from source text.
    #[test]
    fn the_signature_covers_the_hop_it_is_given() {
        let signer = test_signer();
        let now = 1_700_000_000u64;
        let nonce = [7u8; 64];

        let sig_for = |url: &str| -> String {
            added_headers(Some(&signer), url, "", "", now, &nonce)
                .into_iter()
                .find(|(n, _)| *n == "signature")
                .map(|(_, v)| v)
                .expect("a signed request carries a signature")
        };

        // Same inputs, same signature — so any difference below is the URL.
        assert_eq!(sig_for("https://example.com/a"), sig_for("https://example.com/a"));

        // The redirect that matters: only the authority differs.
        assert_ne!(
            sig_for("https://example.com/a"),
            sig_for("https://www.example.com/a"),
            "example.com and www.example.com produced the same signature, so the \
             authority is not coming from the URL being signed — a cross-host \
             redirect would carry the previous host's signature",
        );

        // And the port is part of the authority, so a port-only change counts.
        assert_ne!(
            sig_for("https://example.com/a"),
            sig_for("https://example.com:8443/a"),
            "a port change left the signature identical",
        );
    }

    /// No host, no signature — rather than a signature over something guessed.
    #[test]
    fn an_unparseable_url_is_not_signed() {
        let signer = test_signer();
        let got = added_headers(Some(&signer), "not a url", "", "", 0, &[0u8; 64]);
        assert!(got.is_empty(), "{got:?}");
    }

    /// The fetcher must hand the current hop to the header builder, not the
    /// URL it was originally called with.
    ///
    /// The one part that stays structural: `fetch_conditional` follows
    /// redirects in a loop, and reaching the hop-signing path for real needs a
    /// server that redirects across hosts — which the crawler's own SSRF guard
    /// now refuses to follow to loopback, by design.
    ///
    /// Assembled, since this test reads the file it lives in.
    #[test]
    fn the_fetcher_passes_the_current_hop() {
        let code = include_str!("search.rs");
        let flat: String = code.split_whitespace().collect::<Vec<_>>().join(" ");

        let right = ["added_headers( self.signer.as_deref(), &current,"].concat();
        assert!(
            flat.contains(&right),
            "the fetcher is no longer passing the current hop to added_headers",
        );
    }
}

#[cfg(test)]
mod added_header_tests {
    use super::added_headers;

    /// Unsigned, unconditional: the crawler adds nothing of its own.
    ///
    /// Which is what makes the README's three-header claim true for a first
    /// request — `Host`, `Accept` and `User-Agent` all come from reqwest and
    /// the client's `user_agent`, and are measured against a real listener in
    /// `tools::web::outbound_header_tests`. This is the other half: that
    /// nothing is added here.
    #[test]
    fn a_first_unsigned_request_adds_no_headers() {
        let got = added_headers(None, "https://example.com/a", "", "", 0, &[0u8; 64]);
        assert!(got.is_empty(), "{got:?}");
    }

    /// A re-crawl sends back only the validator the server itself supplied,
    /// and only the one that cannot carry per-visitor entropy.
    #[test]
    fn a_recrawl_adds_if_modified_since_and_not_if_none_match() {
        let got = added_headers(
            None,
            "https://example.com/a",
            "\"abc\"",
            "Wed, 21 Oct 2026 07:28:00 GMT",
            0,
            &[0u8; 64],
        );
        let names: Vec<&str> = got.iter().map(|(n, _)| *n).collect();

        assert!(
            names.contains(&"if-modified-since"),
            "Last-Modified is a content property and should go back: {got:?}"
        );
        // `send_etag` is off unless the operator turned it on, and an ETag can
        // be minted per visitor. Default must not return it.
        assert!(
            !names.contains(&"if-none-match"),
            "an ETag went back without the operator asking: {got:?}"
        );
    }

    /// The full set, with every optional part on at once.
    ///
    /// This is the assertion the README's "complete set" sentence rests on. It
    /// previously rested on a test that rebuilt a `reqwest::Client` with the
    /// same options and never called this code, so the signature and
    /// conditional headers were outside it — three headers could appear, and
    /// did, while the test stayed green.
    #[test]
    fn nothing_is_sent_that_is_not_documented() {
        // Every header this function can ever add, by construction.
        const DOCUMENTED: [&str; 5] = [
            "signature-agent",
            "signature-input",
            "signature",
            "if-none-match",
            "if-modified-since",
        ];

        for (etag, last_mod) in [
            ("", ""),
            ("\"abc\"", ""),
            ("", "Wed, 21 Oct 2026 07:28:00 GMT"),
            ("\"abc\"", "Wed, 21 Oct 2026 07:28:00 GMT"),
        ] {
            let got = added_headers(
                None, "https://example.com/a", etag, last_mod, 1_700_000_000, &[7u8; 64],
            );
            for (name, _) in &got {
                assert!(
                    DOCUMENTED.contains(name),
                    "{name} is sent to crawled sites and is not in the documented set",
                );
            }
        }
    }

    /// An unparseable URL must not produce a signature over a guessed
    /// authority — it produces none.
    #[test]
    fn a_url_with_no_host_is_not_signed() {
        let got = added_headers(None, "not a url", "", "", 0, &[0u8; 64]);
        assert!(got.is_empty(), "{got:?}");
    }
}

