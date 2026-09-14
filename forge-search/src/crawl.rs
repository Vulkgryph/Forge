//! The crawler: deciding what to fetch next, and stopping.
//!
//! Every hard part of a crawler is a limit. Fetching a page and following its
//! links is a dozen lines; the work is in not fetching the same page twice, not
//! hammering one host, not wandering off the site you meant to index, and not
//! running until someone notices. A crawler without those is a program that
//! looks correct on three pages and is a denial-of-service attack on thirty
//! thousand.
//!
//! So this is organised around its bounds rather than its traversal:
//!
//! - **Breadth-first**, because a site's own structure puts its important pages
//!   near the front page. Depth-first on a paginated archive walks to page 900
//!   of the blog before it sees the documentation.
//! - **Per-host politeness**, respecting `Crawl-delay` where a site states one.
//!   A crawl that is fast enough to hurt is a crawl that gets blocked.
//! - **Every URL seen once**, using the normalised form — and recorded as seen
//!   when it is *queued*, not when it is fetched, or a page linked from five
//!   others is queued five times.
//! - **Explicit ceilings** on pages and depth, checked before fetching rather
//!   than after, since the point is to bound the work and not to report it.
//!
//! Time is injected rather than read, for the same reason the fetcher is: a
//! crawler that sleeps for real cannot be tested, and politeness is exactly the
//! behaviour most worth testing.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::fetch::Fetcher;
use crate::html;
use crate::index::Index;
use crate::robots::Robots;
use crate::url::Url;

/// What a crawl is permitted to do.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Most pages to fetch. The one bound that always applies.
    pub max_pages: usize,
    /// How far from a seed to follow links. Zero fetches only the seeds.
    pub max_depth: usize,
    /// Seconds to wait between requests to one host, unless its `robots.txt`
    /// asks for longer — a site's own figure is honoured over this, never
    /// undercut by it.
    pub politeness: f64,
    /// Whether to leave the seeds' hosts.
    ///
    /// Off by default, and that is the important default: following external
    /// links from an arbitrary page is how a crawl of one documentation site
    /// becomes a crawl of the web.
    pub stay_on_host: bool,
    /// Largest page body to parse, in bytes. A page beyond this is skipped
    /// rather than truncated, since half a document indexes as a document.
    pub max_page_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pages: 200,
            max_depth: 3,
            politeness: 1.0,
            stay_on_host: true,
            max_page_bytes: 2 * 1024 * 1024,
        }
    }
}

/// What a crawl did, including what it declined to do and why.
///
/// The refusals are the useful part. A crawl that indexed eleven of two
/// hundred pages has a reason, and without these the only way to find it is to
/// run the crawl again while watching.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub fetched: usize,
    pub indexed: usize,
    /// Refused by `robots.txt`.
    pub disallowed: usize,
    /// Fetched but not HTML.
    pub not_html: usize,
    /// Fetched and gone — 404 and the like.
    pub missing: usize,
    /// No response at all.
    pub unreachable: usize,
    /// Too large to parse.
    pub too_large: usize,
    /// Left in the queue when a limit was reached. Non-zero means the crawl
    /// stopped early and there is more to find.
    pub remaining: usize,
}

/// A clock a crawl can be tested against.
///
/// The only reason politeness is testable: a crawler that calls `sleep` takes
/// as long as its own delays, so either the tests are slow or the delays are
/// untested. With this, a test asserts that the crawler *waited* without
/// anything actually waiting.
pub trait Clock {
    /// Seconds since an arbitrary origin, monotonic.
    fn now(&self) -> f64;
    /// Wait until at least `until`.
    fn sleep_until(&self, until: f64);
}

/// The real clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    fn sleep_until(&self, until: f64) {
        let wait = until - self.now();
        if wait > 0.0 {
            std::thread::sleep(std::time::Duration::from_secs_f64(wait));
        }
    }
}

/// A clock that records what it was asked to wait for without waiting.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: std::cell::Cell<f64>,
    /// Every interval the crawler asked to wait, in order.
    pub waits: std::cell::RefCell<Vec<f64>>,
}

impl Clock for FakeClock {
    fn now(&self) -> f64 {
        self.now.get()
    }

    fn sleep_until(&self, until: f64) {
        let wait = until - self.now.get();
        if wait > 0.0 {
            self.waits.borrow_mut().push(wait);
            self.now.set(until);
        }
        // Time advances a little on every request regardless, so a crawl of
        // many pages does not appear to happen in one instant.
        self.now.set(self.now.get() + 0.001);
    }
}

/// A crawl in progress.
pub struct Crawler<'a, F: Fetcher, C: Clock> {
    fetcher: &'a F,
    clock: &'a C,
    limits: Limits,
    /// Normalised URLs already queued or visited. Recorded at queue time.
    seen: HashSet<String>,
    queue: VecDeque<(Url, usize)>,
    /// One `robots.txt` per host, fetched at most once.
    robots: HashMap<String, Robots>,
    /// When each host may next be contacted.
    next_allowed: HashMap<String, f64>,
    hosts: HashSet<String>,
}

impl<'a, F: Fetcher, C: Clock> Crawler<'a, F, C> {
    pub fn new(fetcher: &'a F, clock: &'a C, limits: Limits) -> Self {
        Self {
            fetcher,
            clock,
            limits,
            seen: HashSet::new(),
            queue: VecDeque::new(),
            robots: HashMap::new(),
            next_allowed: HashMap::new(),
            hosts: HashSet::new(),
        }
    }

    /// Add a starting point. Unparseable seeds are reported rather than
    /// ignored, since a mistyped seed otherwise looks like a site with nothing
    /// on it.
    pub fn seed(&mut self, url: &str) -> Result<(), String> {
        let parsed = Url::parse(url)?;
        self.hosts.insert(parsed.host.clone());
        self.enqueue(parsed, 0);
        Ok(())
    }

    fn enqueue(&mut self, url: Url, depth: usize) {
        let key = url.as_string();
        // Seen at queue time, not at fetch time. A page linked from five
        // others would otherwise be queued five times and fetched five times.
        if self.seen.insert(key) {
            self.queue.push_back((url, depth));
        }
    }

    /// Crawl until a limit is reached, adding what is found to `index`.
    pub fn run(&mut self, index: &mut Index) -> Report {
        let mut report = Report::default();

        while let Some((url, depth)) = self.queue.pop_front() {
            if report.fetched >= self.limits.max_pages {
                // Put it back so `remaining` counts honestly.
                self.queue.push_front((url, depth));
                break;
            }

            if !self.robots_allow(&url) {
                report.disallowed += 1;
                continue;
            }
            self.wait_for_host(&url);

            let fetched = match self.fetcher.fetch(&url.as_string()) {
                Ok(f) => f,
                Err(_) => {
                    report.unreachable += 1;
                    continue;
                }
            };
            report.fetched += 1;

            if !fetched.is_ok() {
                report.missing += 1;
                continue;
            }
            if !fetched.is_html() {
                report.not_html += 1;
                continue;
            }
            if fetched.body.len() > self.limits.max_page_bytes {
                report.too_large += 1;
                continue;
            }

            // Index under where the content actually came from. Recording the
            // requested URL when the server redirected would store an address
            // whose content lives elsewhere, and the next crawl would follow
            // the redirect and index the same page again under the other name.
            let actual = Url::parse(&fetched.final_url).unwrap_or_else(|_| url.clone());
            let page = html::parse(&fetched.body);
            index.add(&actual.as_string(), &page.title, &page.description, &page.text);
            report.indexed += 1;
            // So a redirect target is not fetched again on its own account.
            self.seen.insert(actual.as_string());

            if depth < self.limits.max_depth {
                for link in &page.links {
                    let Ok(target) = actual.join(link) else { continue };
                    if self.limits.stay_on_host && !self.hosts.contains(&target.host) {
                        continue;
                    }
                    self.enqueue(target, depth + 1);
                }
            }
        }

        report.remaining = self.queue.len();
        report
    }

    /// Whether `robots.txt` permits this URL, fetching the file once per host.
    fn robots_allow(&mut self, url: &Url) -> bool {
        let host = url.authority();
        if !self.robots.contains_key(&host) {
            let robots_url = format!("{}://{}/robots.txt", url.scheme, host);
            // The robots fetch is itself subject to politeness — it is a
            // request to the same host as everything else.
            self.wait_for_host(url);
            let rules = match self.fetcher.fetch(&robots_url) {
                Ok(r) if r.is_ok() => Robots::parse(&r.body, self.fetcher.user_agent()),
                // Absent is permissive; unreadable is not. A 404 means the
                // site said nothing, while a 500 means it said something that
                // could not be read, and those deserve different answers.
                Ok(r) if r.is_permanently_gone() => Robots::allow_all(),
                Ok(_) => Robots::deny_all(),
                // No response at all: treated as absent rather than as
                // refusal, or one unreachable robots.txt stops a whole crawl
                // over a file that may not exist.
                Err(_) => Robots::allow_all(),
            };
            self.robots.insert(host.clone(), rules);
        }
        self.robots[&host].allows(url)
    }

    /// Wait until this host may be contacted, then record the next time.
    fn wait_for_host(&mut self, url: &Url) {
        let host = url.authority();
        // A site's own figure is honoured over the configured one, never
        // undercut by it — the configured value is a floor, not a target.
        let delay = self
            .robots
            .get(&host)
            .and_then(|r| r.crawl_delay)
            .map_or(self.limits.politeness, |d| d.max(self.limits.politeness));

        if let Some(&next) = self.next_allowed.get(&host) {
            self.clock.sleep_until(next);
        }
        self.next_allowed.insert(host, self.clock.now() + delay);
    }
}

/// Crawl `seeds` into a new index, with the real clock.
///
/// The convenience form. Anything wanting to add to an existing index, or to
/// control time, uses [`Crawler`] directly.
pub fn crawl<F: Fetcher>(fetcher: &F, seeds: &[&str], limits: Limits) -> (Index, Report) {
    let clock = SystemClock;
    let mut crawler = Crawler::new(fetcher, &clock, limits);
    for seed in seeds {
        let _ = crawler.seed(seed);
    }
    let mut index = Index::new();
    let report = crawler.run(&mut index);
    (index, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{Fetched, StaticFetcher};

    /// A small site: front page linking to two pages, one of which links on.
    fn site() -> StaticFetcher {
        StaticFetcher::new()
            .with_page(
                "https://a.example/",
                r#"<title>Home</title><p>Welcome.</p>
                   <a href="/one">One</a><a href="/two">Two</a>"#,
            )
            .with_page(
                "https://a.example/one",
                r#"<title>One</title><p>The first page about allocators.</p>
                   <a href="/deep">Deeper</a><a href="/">Home</a>"#,
            )
            .with_page("https://a.example/two", r#"<title>Two</title><p>The second page.</p>"#)
            .with_page("https://a.example/deep", r#"<title>Deep</title><p>Down here.</p>"#)
    }

    fn run(fetcher: &StaticFetcher, limits: Limits) -> (Index, Report, FakeClock) {
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = {
            let mut c = Crawler::new(fetcher, &clock, limits);
            c.seed("https://a.example/").unwrap();
            c.run(&mut index)
        };
        (index, report, clock)
    }

    #[test]
    fn a_crawl_follows_links_and_indexes_what_it_finds() {
        let (index, report, _) = run(&site(), Limits::default());
        assert_eq!(report.indexed, 4, "{report:?}");
        assert_eq!(index.len(), 4);
        for url in [
            "https://a.example/",
            "https://a.example/one",
            "https://a.example/two",
            "https://a.example/deep",
        ] {
            assert!(index.contains_url(url), "{url} was not indexed");
        }
        // And the index works.
        assert_eq!(crate::rank::search(&index, "allocators", 10).len(), 1);
    }

    /// The bound that always applies, checked before fetching rather than
    /// after — the point is to bound the work, not to report it.
    #[test]
    fn the_page_limit_stops_the_crawl_and_says_so() {
        let limits = Limits { max_pages: 2, ..Default::default() };
        let (index, report, _) = run(&site(), limits);
        assert_eq!(report.fetched, 2);
        assert_eq!(index.len(), 2);
        assert!(report.remaining > 0, "stopped early but reported nothing left");
    }

    #[test]
    fn depth_zero_fetches_only_the_seeds() {
        let limits = Limits { max_depth: 0, ..Default::default() };
        let (index, report, _) = run(&site(), limits);
        assert_eq!(report.indexed, 1);
        assert!(index.contains_url("https://a.example/"));
        assert_eq!(report.remaining, 0, "links were queued despite depth 0");
    }

    #[test]
    fn depth_one_reaches_the_seeds_links_but_no_further() {
        let limits = Limits { max_depth: 1, ..Default::default() };
        let (index, _, _) = run(&site(), limits);
        assert!(index.contains_url("https://a.example/one"));
        assert!(!index.contains_url("https://a.example/deep"), "went too deep");
    }

    /// A page linked from several others is fetched once. Recording at fetch
    /// time instead of queue time is the bug this guards.
    #[test]
    fn a_page_linked_twice_is_fetched_once() {
        let fetcher = StaticFetcher::new()
            .with_page(
                "https://a.example/",
                r#"<a href="/x">a</a><a href="/x">b</a><a href="/x?">c</a>"#,
            )
            .with_page("https://a.example/x", "<p>once</p>");
        let (index, report, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 2, "the same page was indexed twice");
        assert_eq!(report.fetched, 2, "fetched {} times", report.fetched);
    }

    /// Following external links from an arbitrary page is how a crawl of one
    /// site becomes a crawl of the web.
    #[test]
    fn a_crawl_stays_on_the_seed_host_by_default() {
        let fetcher = StaticFetcher::new()
            .with_page(
                "https://a.example/",
                r#"<a href="https://elsewhere.example/big">off site</a><a href="/local">local</a>"#,
            )
            .with_page("https://a.example/local", "<p>here</p>")
            .with_page("https://elsewhere.example/big", "<p>should not be fetched</p>");
        let (index, _, _) = run(&fetcher, Limits::default());
        assert!(!index.contains_url("https://elsewhere.example/big"), "left the host");
        assert!(index.contains_url("https://a.example/local"));
    }

    #[test]
    fn leaving_the_host_can_be_allowed() {
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="https://b.example/x">off</a>"#)
            .with_page("https://b.example/x", "<p>reached</p>");
        let limits = Limits { stay_on_host: false, ..Default::default() };
        let (index, _, _) = run(&fetcher, limits);
        assert!(index.contains_url("https://b.example/x"));
    }

    #[test]
    fn robots_disallow_is_obeyed_and_counted() {
        let fetcher = site().with_response(
            "https://a.example/robots.txt",
            Fetched {
                status: 200,
                final_url: "https://a.example/robots.txt".into(),
                content_type: "text/plain".into(),
                body: "User-agent: *\nDisallow: /two\n".into(),
            },
        );
        let (index, report, _) = run(&fetcher, Limits::default());
        assert!(!index.contains_url("https://a.example/two"), "fetched a disallowed page");
        assert_eq!(report.disallowed, 1, "{report:?}");
        assert!(index.contains_url("https://a.example/one"));
    }

    /// robots.txt is fetched once per host however many pages are crawled.
    #[test]
    fn robots_is_fetched_once_per_host() {
        struct Counting {
            inner: StaticFetcher,
            robots_hits: std::cell::Cell<usize>,
        }
        impl Fetcher for Counting {
            fn fetch(&self, url: &str) -> Result<Fetched, String> {
                if url.ends_with("/robots.txt") {
                    self.robots_hits.set(self.robots_hits.get() + 1);
                }
                self.inner.fetch(url)
            }
        }
        let f = Counting { inner: site(), robots_hits: std::cell::Cell::new(0) };
        let clock = FakeClock::default();
        let mut index = Index::new();
        {
            let mut c = Crawler::new(&f, &clock, Limits::default());
            c.seed("https://a.example/").unwrap();
            c.run(&mut index);
        }
        assert_eq!(index.len(), 4);
        assert_eq!(f.robots_hits.get(), 1, "robots.txt was fetched {} times", f.robots_hits.get());
    }

    /// An absent robots.txt is permissive; one that exists and cannot be read
    /// is not. The site said something that was not understood.
    #[test]
    fn an_unreadable_robots_file_stops_the_crawl() {
        let fetcher = site().with_response(
            "https://a.example/robots.txt",
            Fetched {
                status: 500,
                final_url: "https://a.example/robots.txt".into(),
                content_type: "text/plain".into(),
                body: String::new(),
            },
        );
        let (index, report, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 0, "crawled a site whose rules could not be read");
        assert!(report.disallowed > 0);
    }

    /// But an unreachable one is treated as absent, or a single network blip
    /// on a file that may not exist stops everything.
    #[test]
    fn an_unreachable_robots_file_does_not_stop_the_crawl() {
        let fetcher = site().with_unreachable("https://a.example/robots.txt");
        let (index, _, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 4, "a missing robots.txt blocked the crawl");
    }

    /// The reason time is injected: politeness is the behaviour most worth
    /// testing and the least testable with a real clock.
    #[test]
    fn the_crawler_waits_between_requests_to_one_host() {
        let limits = Limits { politeness: 2.0, ..Default::default() };
        let (_, _, clock) = run(&site(), limits);
        let waits = clock.waits.borrow();
        assert!(!waits.is_empty(), "the crawler never waited");
        assert!(
            waits.iter().all(|&w| w >= 1.9),
            "waited less than asked: {waits:?}"
        );
    }

    /// A site's own figure is honoured over the configured one, and never
    /// undercut by it.
    #[test]
    fn a_sites_crawl_delay_overrides_a_shorter_configured_one() {
        let fetcher = site().with_response(
            "https://a.example/robots.txt",
            Fetched {
                status: 200,
                final_url: "https://a.example/robots.txt".into(),
                content_type: "text/plain".into(),
                body: "User-agent: *\nCrawl-delay: 5\n".into(),
            },
        );
        let limits = Limits { politeness: 0.5, ..Default::default() };
        let (_, _, clock) = run(&fetcher, limits);
        let waits = clock.waits.borrow();
        assert!(
            waits.iter().any(|&w| w >= 4.9),
            "the site asked for 5s and the crawler waited {waits:?}"
        );
    }

    /// Indexed under where the content came from, or the next crawl follows
    /// the redirect again and indexes a second copy.
    #[test]
    fn a_redirect_is_indexed_under_its_destination() {
        let fetcher = StaticFetcher::new()
            .with_redirect("https://a.example/old", "https://a.example/new", "<p>moved here</p>")
            .with_page("https://a.example/", r#"<a href="/old">old</a>"#);
        let (index, _, _) = run(&fetcher, Limits::default());
        assert!(index.contains_url("https://a.example/new"), "indexed the requested URL");
        assert!(!index.contains_url("https://a.example/old"));
    }

    #[test]
    fn non_html_and_missing_pages_are_counted_not_indexed() {
        let fetcher = StaticFetcher::new()
            .with_page(
                "https://a.example/",
                r#"<a href="/doc.pdf">pdf</a><a href="/gone">gone</a><a href="/ok">ok</a>"#,
            )
            .with_response(
                "https://a.example/doc.pdf",
                Fetched {
                    status: 200,
                    final_url: "https://a.example/doc.pdf".into(),
                    content_type: "application/pdf".into(),
                    body: "%PDF-1.4".into(),
                },
            )
            .with_page("https://a.example/ok", "<p>fine</p>");
        let (index, report, _) = run(&fetcher, Limits::default());
        assert_eq!(report.not_html, 1, "{report:?}");
        assert_eq!(report.missing, 1, "{report:?}");
        assert_eq!(index.len(), 2, "indexed something it should not have");
    }

    /// Half a document indexes as a document, so an oversized page is skipped
    /// rather than truncated.
    #[test]
    fn an_oversized_page_is_skipped() {
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/huge">huge</a>"#)
            .with_page("https://a.example/huge", &format!("<p>{}</p>", "x".repeat(5000)));
        let limits = Limits { max_page_bytes: 1000, ..Default::default() };
        let (index, report, _) = run(&fetcher, limits);
        assert_eq!(report.too_large, 1, "{report:?}");
        assert!(!index.contains_url("https://a.example/huge"));
    }

    /// A cycle must not crawl forever — the commonest way a crawler fails to
    /// terminate.
    #[test]
    fn a_link_cycle_terminates() {
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/b">b</a>"#)
            .with_page("https://a.example/b", r#"<a href="/">home</a><a href="/c">c</a>"#)
            .with_page("https://a.example/c", r#"<a href="/b">b</a><a href="/">home</a>"#);
        let limits = Limits { max_pages: 1000, max_depth: 50, ..Default::default() };
        let (index, report, _) = run(&fetcher, limits);
        assert_eq!(index.len(), 3, "{report:?}");
        assert_eq!(report.fetched, 3, "a cycle caused refetching: {report:?}");
    }

    /// A mistyped seed must be reported, or it looks like a site with nothing
    /// on it.
    #[test]
    fn a_bad_seed_is_an_error() {
        let fetcher = site();
        let clock = FakeClock::default();
        let mut c = Crawler::new(&fetcher, &clock, Limits::default());
        assert!(c.seed("not a url").is_err());
        assert!(c.seed("mailto:someone@example.com").is_err());
        assert!(c.seed("https://a.example/").is_ok());
    }

    #[test]
    fn a_crawl_with_no_seeds_does_nothing() {
        let fetcher = site();
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = Crawler::new(&fetcher, &clock, Limits::default()).run(&mut index);
        assert_eq!(report, Report::default());
        assert!(index.is_empty());
    }

    /// Several seeds, and every seed's host is crawlable even with
    /// `stay_on_host`.
    #[test]
    fn every_seed_host_is_allowed() {
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/x">x</a>"#)
            .with_page("https://a.example/x", "<p>a</p>")
            .with_page("https://b.example/", r#"<a href="/y">y</a>"#)
            .with_page("https://b.example/y", "<p>b</p>");
        let clock = FakeClock::default();
        let mut index = Index::new();
        {
            let mut c = Crawler::new(&fetcher, &clock, Limits::default());
            c.seed("https://a.example/").unwrap();
            c.seed("https://b.example/").unwrap();
            c.run(&mut index);
        }
        assert_eq!(index.len(), 4, "a seeded host was treated as off-site");
    }

    /// Breadth-first, so a site's own structure decides what is reached first.
    /// Depth-first on a paginated archive walks to page 900 of the blog before
    /// it sees the documentation.
    #[test]
    fn the_crawl_is_breadth_first() {
        // The shallow page is linked *first*, so a stack reaches it last and a
        // queue reaches it second. An earlier version of this fixture linked it
        // second, which meant both orders reached everything and the test
        // passed against a depth-first crawl — it proved nothing.
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/shallow">shallow</a><a href="/deep1">deep</a>"#)
            .with_page("https://a.example/deep1", r#"<a href="/deep2">deeper</a>"#)
            .with_page("https://a.example/deep2", r#"<a href="/deep3">deepest</a>"#)
            .with_page("https://a.example/deep3", "<p>bottom</p>")
            .with_page("https://a.example/shallow", "<p>near the top</p>");
        // Room for the seed and two more. Breadth-first spends them on the
        // seed's own links; depth-first walks down the chain instead.
        let limits = Limits { max_pages: 3, max_depth: 10, ..Default::default() };
        let (index, _, _) = run(&fetcher, limits);
        assert!(
            index.contains_url("https://a.example/shallow"),
            "a depth-first crawl went down instead of across"
        );
        assert!(!index.contains_url("https://a.example/deep3"));
    }
}
