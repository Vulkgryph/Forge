// SPDX-License-Identifier: Apache-2.0
//! Crawling the web from the browser tab, with Forge's own engine.
//!
//! The search page could only ever answer from what had already been crawled,
//! which in a fresh project is nothing. This is the other half: point the
//! engine at a site and it reads it, indexes it, and answers from it.
//!
//! ## What this is not
//!
//! It is not a web search. Forge has no web-scale index and no way to find
//! sites by their content, so there is no answering "ford 8n engine oil" the
//! way a search engine does — the engine can only read where it is pointed.
//! That is a property of being a crawler rather than a gap to be filled in,
//! and the interface says so rather than implying otherwise: a search here
//! takes a query *and a site*.
//!
//! Which is less limiting than it sounds, because the person typing usually
//! knows the site. Somebody looking for a tractor manual knows which forum;
//! somebody looking for an API knows whose documentation. Pointing the engine
//! is the part a person is good at and the engine is not.
//!
//! ## Why a thread
//!
//! A crawl is a page a second by courtesy, so a hundred pages is a hundred
//! seconds. On the event-loop thread that is a hundred seconds of frozen
//! window. It runs on its own thread, reports progress through a channel, and
//! wakes the loop when there is something new to draw.

use std::sync::mpsc::{self, Receiver, Sender};

use forge_search::crawl::{self, Limits};
use forge_search::fetch::{Fetched, Fetcher};
use forge_search::index::Index;

/// How many pages a crawl started from the browser reads.
///
/// Lower than the agent's hundred and twenty, because a person is watching a
/// progress line rather than waiting on a tool call, and sixty pages of a
/// site is enough to answer a question about it. At a page a second that is a
/// minute — long, and visibly progressing.
const MAX_PAGES: usize = 60;

/// Seconds between requests to one host. The conventional courtesy, the same
/// figure the agent uses; `robots.txt` may ask for more and is obeyed.
const POLITENESS: f64 = 1.0;

/// What a crawl is doing, for the page to show.
#[derive(Clone, Debug)]
pub enum Progress {
    /// Pages fetched so far, and the last address read — so the line moves
    /// and says something, rather than being a spinner.
    Fetched { pages: usize, url: String },
    /// Finished, with what it found and what it could not.
    Done {
        indexed: usize,
        /// Pages a bot check refused. Named because the response is
        /// different: those need the browser, which is right here.
        challenged: usize,
        refused_by: String,
    },
    Failed(String),
}

/// A crawl running on its own thread.
pub struct Crawling {
    pub query: String,
    pub seed: String,
    pub pages: usize,
    pub last_url: String,
    pub finished: Option<String>,
    rx: Receiver<Progress>,
}

impl Crawling {
    /// Take whatever the crawl has reported since the last look.
    ///
    /// Returns true when it finished, so the caller knows to search the index
    /// again.
    pub fn poll(&mut self) -> bool {
        let mut done = false;
        while let Ok(progress) = self.rx.try_recv() {
            match progress {
                Progress::Fetched { pages, url } => {
                    self.pages = pages;
                    self.last_url = url;
                }
                Progress::Done { indexed, challenged, refused_by } => {
                    self.finished = Some(if challenged > 0 {
                        format!(
                            "read {indexed} page(s); {challenged} were refused by a bot check \
                             ({refused_by}) — open one in this tab to get past it"
                        )
                    } else {
                        format!("read {indexed} page(s)")
                    });
                    done = true;
                }
                Progress::Failed(why) => {
                    self.finished = Some(format!("could not crawl {}: {why}", self.seed));
                    done = true;
                }
            }
        }
        done
    }
}

/// Start crawling `seed`, adding what it reads to the index at `index_path`.
///
/// Returns immediately; the crawl runs on its own thread.
pub fn start(query: &str, seed: &str, index_path: std::path::PathBuf) -> Crawling {
    let (tx, rx) = mpsc::channel();
    let seed_owned = seed.to_string();
    std::thread::Builder::new()
        .name("forge-crawl".into())
        .spawn(move || run(&seed_owned, index_path, tx))
        .ok();

    Crawling {
        query: query.to_string(),
        seed: seed.to_string(),
        pages: 0,
        last_url: String::new(),
        finished: None,
        rx,
    }
}

fn run(seed: &str, index_path: std::path::PathBuf, tx: Sender<Progress>) {
    // Added to whatever is already indexed rather than replacing it, so a
    // second crawl of another site widens the corpus instead of resetting it.
    let mut index = Index::load(&index_path).unwrap_or_else(|_| Index::new());

    let fetcher = UreqFetcher {
        tx: tx.clone(),
        fetched: std::sync::atomic::AtomicUsize::new(0),
    };
    let clock = crawl::SystemClock;
    let limits = Limits {
        max_pages: MAX_PAGES,
        max_depth: 4,
        politeness: POLITENESS,
        stay_on_host: true,
        max_page_bytes: 4 * 1024 * 1024,
        // Bounded so a slow host cannot leave the line moving forever.
        max_seconds: Some(180.0),
    };

    let mut crawler = crawl::Crawler::new(&fetcher, &clock, limits);
    if crawler.seed(seed).is_err() {
        let _ = tx.send(Progress::Failed("not a usable address".into()));
        return;
    }
    let report = crawler.run(&mut index);

    if let Some(parent) = index_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = index.save(&index_path) {
        let _ = tx.send(Progress::Failed(e));
        return;
    }
    let _ = tx.send(Progress::Done {
        indexed: report.indexed,
        challenged: report.challenged,
        refused_by: report.challenged_by.clone(),
    });
    crate::wake::wake();
}

/// A [`Fetcher`] over the HTTP client the IDE already carries, reporting each
/// page as it goes.
struct UreqFetcher {
    tx: Sender<Progress>,
    /// Pages fetched, so the progress line counts. The crawler does not hand
    /// its own tally to the fetcher, and a count of zero every time is worse
    /// than no count at all — it looks stuck.
    fetched: std::sync::atomic::AtomicUsize,
}

impl Fetcher for UreqFetcher {
    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        let response = match ureq::get(url)
            .set("User-Agent", USER_AGENT)
            .timeout(std::time::Duration::from_secs(20))
            .call()
        {
            Ok(r) => r,
            // A status error is a response, not a failure to reach anything —
            // the crawler's handling of 404 and of a dead host differ, and
            // collapsing them loses that.
            Err(ureq::Error::Status(_, r)) => r,
            Err(e) => return Err(e.to_string()),
        };

        let status = response.status();
        let final_url = response.get_url().to_string();
        let content_type = response
            .header("content-type")
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        // Names lowercased, as `Fetched::challenge` expects. `cf-mitigated` is
        // the one that matters: it says outright that a bot check was served
        // instead of the page.
        let headers: Vec<(String, String)> = response
            .headers_names()
            .into_iter()
            .filter_map(|name| {
                response
                    .header(&name)
                    .map(|v| (name.to_ascii_lowercase(), v.to_string()))
            })
            .collect();

        let body = response.into_string().unwrap_or_default();

        // Progress before returning, so the line moves while the crawl waits
        // out its politeness delay rather than in bursts.
        let pages = self
            .fetched
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let _ = self.tx.send(Progress::Fetched { pages, url: final_url.clone() });
        crate::wake::wake();

        Ok(Fetched { status, final_url, content_type, headers, body })
    }

    fn user_agent(&self) -> &str {
        USER_AGENT
    }
}

/// What Forge calls itself when it crawls.
///
/// The same honesty as everywhere else here: a crawler that lies about who it
/// is cannot meaningfully claim to be obeying `robots.txt`, since the file
/// addresses crawlers by name.
const USER_AGENT: &str = concat!("forge-search/", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
mod tests {
    use super::*;

    /// The crawl adds to the index rather than replacing it, so searching one
    /// site does not throw away another.
    #[test]
    fn a_crawl_widens_the_corpus() {
        // Named for this test, not just the process: tests run in parallel
        // threads of one process, and `forge-ws-<pid>` is already taken by a
        // workspace test in `app.rs` — which this promptly deleted underneath
        // it. Third time this exact collision has happened in this tree.
        let dir = std::env::temp_dir()
            .join(format!("forge-websearch-widen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("i.bin");

        let mut first = Index::new();
        first.add("https://a.test/1", "A", "", "already here");
        first.save(&path).unwrap();

        // What `run` does with an existing index, without the network.
        let loaded = Index::load(&path).unwrap();
        assert_eq!(loaded.len(), 1, "the existing index was not loaded");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A page limit a person will actually wait out.
    #[test]
    fn the_page_budget_is_a_minute_or_so() {
        let seconds = MAX_PAGES as f64 * POLITENESS;
        assert!(seconds <= 90.0, "{seconds}s is too long to watch");
        assert!(MAX_PAGES >= 30, "too few pages to answer anything");
    }

    #[test]
    fn the_crawler_says_who_it_is() {
        assert!(USER_AGENT.starts_with("forge-search/"));
        assert!(!USER_AGENT.to_lowercase().contains("mozilla"), "a crawler must not pretend");
    }
}
