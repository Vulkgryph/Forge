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

/// Google's documented `robots.txt` size limit, and the conventional one.
/// A body past it is not a rules file anyone wrote by hand.
const MAX_ROBOTS_BYTES: usize = 500 * 1024;

/// How long to stand off a host that said 429 or 503 without saying when.
pub(crate) const DEFAULT_BACKOFF: f64 = 60.0;

/// The longest stand-off a crawl will take from a `Retry-After`. Past this the
/// wait is longer than most crawls live, and the host is simply left alone for
/// the rest of this one.
pub(crate) const MAX_BACKOFF: f64 = 300.0;

/// `Retry-After` in either of its two forms.
///
/// Delta-seconds is the common one. The HTTP-date form is accepted as far as
/// recognising that it *is* a date — without a clock to compare against, the
/// honest reading is "a while", not a number parsed out of thin air.
pub(crate) fn parse_retry_after(value: &str) -> Option<f64> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<f64>() {
        return if secs.is_finite() && secs >= 0.0 { Some(secs) } else { None };
    }
    // Looks like an HTTP-date (RFC 9110 prefixes it with a weekday).
    if value.len() > 4 && value[..4].contains(',') {
        return Some(DEFAULT_BACKOFF);
    }
    None
}

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
    ///
    /// One second is the conventional figure and the default here. It is not a
    /// performance setting: at this rate a crawl spends almost all of its wall
    /// clock deliberately waiting, because fetching and indexing a page costs
    /// about sixteen milliseconds. Lowering it makes a crawl faster and makes
    /// Forge's user agent — which identifies itself honestly — more likely to
    /// be blocked, and a blocked reputation attaches to Forge rather than to
    /// an anonymous scraper.
    pub politeness: f64,
    /// Longest `Crawl-delay` this crawler will agree to.
    ///
    /// A site asking for longer is not refused because the number is wrong —
    /// it is refused because a crawl that must finish inside a time budget
    /// cannot honour it, and the alternative was silently going 288 times
    /// faster than asked while reporting itself as obedient.
    pub max_crawl_delay: f64,
    /// Whether to leave the seeds' hosts.
    ///
    /// Off by default, and that is the important default: following external
    /// links from an arbitrary page is how a crawl of one documentation site
    /// becomes a crawl of the web.
    pub stay_on_host: bool,
    /// Largest page body to parse, in bytes. A page beyond this is skipped
    /// rather than truncated, since half a document indexes as a document.
    pub max_page_bytes: usize,
    /// Seconds the crawl may run, or `None` for no limit.
    ///
    /// The page limit bounds the work but not the time: one slow host can hold
    /// a twenty-page crawl for minutes, and a crawl driven by something with a
    /// caller waiting on it — a tool call, a request — needs a bound on the
    /// clock rather than on the count. Checked between pages, so it stops
    /// promptly rather than mid-fetch.
    pub max_seconds: Option<f64>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_pages: 200,
            max_depth: 3,
            politeness: 1.0,
            // Five minutes. Long enough that almost every site asking for a
            // gap is honoured, short enough that a crawl with a budget can
            // keep the appointment.
            max_crawl_delay: 300.0,
            stay_on_host: true,
            max_page_bytes: 2 * 1024 * 1024,
            max_seconds: None,
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
    /// The site asked, in real time, for less — a 429 or a 503. Counted
    /// apart from `missing` because it is the one refusal that says "later"
    /// rather than "no", and the crawler backs off the host rather than
    /// carrying on at the same rate.
    pub rate_limited: usize,
    /// Asked for and found unchanged — a 304. The copy already indexed is
    /// still current, so nothing was transferred and nothing was rewritten.
    pub unchanged: usize,
    /// Refused because the site asked for a `Crawl-delay` longer than this
    /// crawl can honour. Not a failure and not a ban: an appointment that
    /// cannot be kept inside the budget.
    pub crawl_delay_too_long: usize,
    /// Refused because the address is not on the public internet — loopback,
    /// a private range, or link-local. Counted apart from `disallowed`
    /// because a site did not ask for this; it is a limit on where a crawler
    /// may be pointed at all.
    pub not_public: usize,
    /// Redirected off every host the caller named, while `stay_on_host` was
    /// set. The requested host was in scope; the destination was not.
    pub off_host: usize,
    /// Fetched but not HTML.
    pub not_html: usize,
    /// Fetched and gone — 404 and the like.
    pub missing: usize,
    /// Refused by a bot-management challenge rather than served.
    ///
    /// Counted apart from `missing` because the two mean opposite things about
    /// what to do next: a missing page means the address is wrong, while a
    /// challenge means the address is right and the access is not. Told the
    /// first when it is really the second, a caller corrects the URL forever
    /// and never opens a browser.
    pub challenged: usize,
    /// Hosts that served a challenge, deduplicated, so a caller can say which
    /// site needs a browser rather than only how many pages did.
    pub challenged_hosts: Vec<String>,
    /// One refused URL per challenged host.
    ///
    /// A host is what you name in a message; a URL is what a browser can
    /// actually be pointed at. One per host rather than all of them because a
    /// wholly walled site refuses every page in the crawl and nobody is going
    /// to open them one at a time — the first names the problem.
    pub challenged_urls: Vec<String>,
    /// The system that did the refusing, as last seen. One name is enough: a
    /// crawl is nearly always walled by one thing, and the alternative is a
    /// list nobody reads.
    pub challenged_by: String,
    /// No response at all.
    pub unreachable: usize,
    /// Too large to parse.
    pub too_large: usize,
    /// Left in the queue when a limit was reached. Non-zero means the crawl
    /// stopped early and there is more to find.
    pub remaining: usize,
    /// Whether the clock, rather than the page count, ended the crawl. The
    /// caller's response differs: more pages is a setting, more time may not
    /// be available.
    pub timed_out: bool,
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

/// How many queue entries are looked at when choosing the next page.
///
/// Bounded so the choice costs the same whether the frontier holds a hundred
/// URLs or a hundred thousand — a real crawl of six seed pages left 7,409
/// queued, and scanning all of them on every pop would make the crawler
/// quadratic in its own frontier.
const FRONTIER_WINDOW: usize = 512;

/// How many pages must have been seen before link frequency means anything.
///
/// With one page crawled, every link on it was linked by 100% of pages.
const TEMPLATE_EVIDENCE: usize = 4;

/// The share of crawled pages that must link a URL for it to look like part of
/// the site's template rather than its content.
const TEMPLATE_SHARE: f64 = 0.8;

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
    /// How many crawled pages linked each URL, counted once per page.
    ///
    /// This is how the site's furniture gives itself away. A template is, by
    /// definition, what every page has in common: the sidebar, the footer, the
    /// "About" and "Contact" links. Content links are on a few pages; nav
    /// links are on all of them.
    link_sources: HashMap<String, usize>,
    /// How many crawled pages have contributed link evidence — the denominator
    /// for the above.
    pages_linked: usize,
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
            link_sources: HashMap::new(),
            pages_linked: 0,
        }
    }

    /// Add a starting point. Unparseable seeds are reported rather than
    /// ignored, since a mistyped seed otherwise looks like a site with nothing
    /// on it.
    pub fn seed(&mut self, url: &str) -> Result<(), String> {
        let parsed = Url::parse(url)?;
        // A crawl is pointed at the public internet. Refused here rather than
        // at fetch time so the caller is told why, instead of watching a crawl
        // return nothing.
        if crate::net::is_obviously_local(&parsed.host) {
            return Err(format!(
                "{}: not a public address — a crawl cannot be pointed at loopback, \
                 a private network, or link-local addresses",
                parsed.host
            ));
        }
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
        let started = self.clock.now();

        while let Some((url, depth)) = self.next_page() {
            if report.fetched >= self.limits.max_pages {
                // Put it back so `remaining` counts honestly.
                self.queue.push_front((url, depth));
                break;
            }
            // Between pages rather than mid-fetch, so the crawl stops promptly
            // without abandoning a request already in flight.
            if let Some(budget) = self.limits.max_seconds {
                if self.clock.now() - started >= budget {
                    self.queue.push_front((url, depth));
                    report.timed_out = true;
                    break;
                }
            }

            if !self.robots_allow(&url) {
                report.disallowed += 1;
                continue;
            }
            self.wait_for_host(&url);

            // What the server told us about this page last time, if it is
            // already indexed. Sending it back is the difference between
            // "send me the page" and "send it only if it changed" — on a
            // site asked about repeatedly, almost every page is unchanged
            // and the second question costs a header instead of a body.
            let (etag, last_modified) = index
                .by_url_id(&url.as_string())
                .and_then(|id| index.document(id))
                .map(|d| (d.etag.clone(), d.last_modified.clone()))
                .unwrap_or_default();

            let fetched = match self.fetcher.fetch_conditional(
                &url.as_string(),
                &etag,
                &last_modified,
            ) {
                Ok(f) => f,
                Err(_) => {
                    report.unreachable += 1;
                    continue;
                }
            };
            report.fetched += 1;

            // The server agreeing that our copy is current. Nothing to parse,
            // nothing to rewrite, and the copy stays exactly as it is.
            if fetched.status == 304 {
                report.unchanged += 1;
                continue;
            }

            // Before the status check, because a challenge can arrive as a
            // 200 and would otherwise be indexed as content — a page whose
            // text is "Just a moment... Enable JavaScript and cookies to
            // continue", matching later queries and answering nothing.
            if let Some(vendor) = fetched.challenge() {
                report.challenged += 1;
                let host = url.host.clone();
                if !report.challenged_hosts.contains(&host) {
                    report.challenged_hosts.push(host);
                    report.challenged_urls.push(url.as_string());
                    report.challenged_by = vendor.to_string();
                }
                continue;
            }

            // A site asking for less, right now. This used to be counted as
            // "missing" and the crawl carried on at the same rate, which is
            // worse than ignoring a static Crawl-delay: that is a standing
            // preference, this is the server saying it is struggling, while
            // it is struggling. Under a user agent that names us, ignoring it
            // is the fastest route onto a blocklist by name.
            //
            // `is_permanently_gone` already classified these correctly and
            // was already tested — it simply had no caller outside its own
            // tests.
            if matches!(fetched.status, 429 | 503) {
                let backoff = fetched
                    .header("retry-after")
                    .and_then(parse_retry_after)
                    .unwrap_or(DEFAULT_BACKOFF)
                    .clamp(1.0, MAX_BACKOFF);
                let next = self.clock.now() + backoff;
                self.next_allowed.insert(url.authority(), next);
                report.rate_limited += 1;
                continue;
            }

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

            // Everything above was decided about the URL that was *requested*.
            // The fetcher follows redirects internally, so what came back may
            // be a different host entirely — one whose robots.txt was never
            // read, which the caller never named, and which may not be on the
            // public internet at all. A hostile page redirecting a legitimate
            // crawl at 127.0.0.1 put the response in the index and then in
            // front of the model, with no cooperation from the model needed.
            if actual.host != url.host {
                if crate::net::is_obviously_local(&actual.host) {
                    report.not_public += 1;
                    continue;
                }
                if self.limits.stay_on_host && !self.hosts.contains(&actual.host) {
                    report.off_host += 1;
                    continue;
                }
                if !self.robots_allow(&actual) {
                    report.disallowed += 1;
                    continue;
                }
                // The destination host has now been fetched from, so it owes
                // the same politeness gap as any other.
                let next = self.clock.now() + self.limits.politeness;
                self.next_allowed.insert(actual.authority(), next);
            }

            let page = html::parse(&fetched.body);
            index.add(&actual.as_string(), &page.title, &page.description, &page.text);
            // Kept for the next crawl to ask with. Only what this response
            // actually carried — a server that sends neither gets an
            // unconditional fetch next time, as it must.
            index.set_validators(
                &actual.as_string(),
                fetched.header("etag").unwrap_or_default(),
                fetched.header("last-modified").unwrap_or_default(),
            );
            report.indexed += 1;
            // So a redirect target is not fetched again on its own account.
            self.seen.insert(actual.as_string());

            // Link evidence is recorded whatever the depth: a page at the
            // depth limit still shows which links every page carries, and
            // that is what tells the template apart from the content.
            let mut on_this_page = HashSet::new();
            for link in &page.links {
                let Ok(target) = actual.join(link) else { continue };
                if self.limits.stay_on_host && !self.hosts.contains(&target.host) {
                    continue;
                }
                let key = target.as_string();
                // Once per page. A sidebar link repeated in the footer is one
                // page's worth of evidence, not two.
                if !on_this_page.insert(key.clone()) {
                    continue;
                }
                *self.link_sources.entry(key).or_insert(0) += 1;
                if depth < self.limits.max_depth {
                    self.enqueue(target, depth + 1);
                }
            }
            if !on_this_page.is_empty() {
                self.pages_linked += 1;
            }
        }

        report.remaining = self.queue.len();
        report
    }

    /// The next page to fetch: the first candidate near the front of the
    /// frontier that does not look like part of the site's template.
    ///
    /// Breadth-first order is kept as the base, and this only reorders within
    /// a bounded window of it. The reason is measured: crawling thirty pages
    /// from six neuroscience articles on Wikipedia spent ten of them on
    /// `Main_Page`, `Help:Contents`, `Wikipedia:About`, `Portal:Current_events`
    /// and the file-upload wizard. A third of the budget went to the sidebar,
    /// because navigation links come before body links in the document and
    /// breadth-first takes them in the order it finds them.
    ///
    /// Nothing is excluded — a deferred URL stays in the queue and is taken
    /// once the better candidates run out. That matters because the signal has
    /// a known false positive: on a tightly topical seed set, a genuinely
    /// central page can be linked from every seed and look like furniture.
    /// Deferring such a page costs its position; dropping it would cost the
    /// page, so this defers.
    fn next_page(&mut self) -> Option<(Url, usize)> {
        let window = self.queue.len().min(FRONTIER_WINDOW);
        let mut pick = 0;
        for i in 0..window {
            if !self.looks_like_template(&self.queue[i].0.as_string()) {
                pick = i;
                break;
            }
        }
        // If everything in the window looks like template, the front of the
        // queue is taken anyway rather than stalling.
        self.queue.remove(pick)
    }

    /// Whether a URL is linked by a large enough share of the pages crawled so
    /// far to be the site's own furniture.
    fn looks_like_template(&self, url: &str) -> bool {
        if self.pages_linked < TEMPLATE_EVIDENCE {
            return false;
        }
        let sources = self.link_sources.get(url).copied().unwrap_or(0);
        sources as f64 / self.pages_linked as f64 >= TEMPLATE_SHARE
    }

        /// Whether `robots.txt` permits this URL, fetching the file once per host.
    fn robots_allow(&mut self, url: &Url) -> bool {
        // Keyed by scheme *and* authority, per RFC 9309, which scopes a
        // robots.txt to (scheme, host, port). Keying on the authority alone
        // meant whichever scheme was seen first decided the rules for both,
        // so an http page could be governed by the https file or the reverse.
        // Politeness stays keyed on the bare authority, because a server is
        // one server however you reach it.
        let key = format!("{}://{}", url.scheme, url.authority());
        let host = url.authority();
        if !self.robots.contains_key(&key) {
            let robots_url = format!("{}/robots.txt", key);
            // The robots fetch is itself subject to politeness — it is a
            // request to the same host as everything else.
            self.wait_for_host(url);
            let fetched = self.fetch_robots(&robots_url);
            let rules = match fetched {
                // A 200 is not enough on its own. A robots.txt that redirects
                // to a login page, or an SPA serving its shell for unknown
                // paths, arrives as a perfectly good 200 whose body parses to
                // zero directives — which reads as "no restrictions" and is
                // the most permissive possible answer to a question that was
                // never answered. `looks_like_robots` was already written for
                // the challenge branch; the clean path needed it just as much.
                Ok(r) if r.is_ok() => {
                    if looks_like_robots(&r.body) {
                        Robots::parse(&r.body, self.fetcher.user_agent())
                    } else {
                        Robots::deny_all()
                    }
                }

                // A bot check on `robots.txt` is the site answering, not the
                // site staying silent — so it is never read as permission.
                //
                // This was letting Forge crawl a site that forbids it.
                // Cloudflare answers stackoverflow.com's `robots.txt` with
                // status 418 and the real file in the body; 418 falls inside
                // "permanently gone", so the rules were discarded as absent
                // and a file saying `User-agent: * / Disallow: /` was treated
                // as no restrictions at all. RFC 9309 does permit reading 4xx
                // as unrestricted, and for a genuine 404 that is right, but a
                // refusal is not a 404.
                //
                // The body is parsed anyway when it carries rules, because it
                // often does: honouring what the site actually said beats
                // assuming the worst about it, and here it says the same thing
                // either way.
                Ok(r) if r.challenge().is_some() => {
                    if looks_like_robots(&r.body) {
                        Robots::parse(&r.body, self.fetcher.user_agent())
                    } else {
                        Robots::deny_all()
                    }
                }

                // Absent is permissive; unreadable is not. A 404 or a 410 means
                // the site said nothing, while a 500 means it said something
                // that could not be read, and those deserve different answers.
                Ok(r) if matches!(r.status, 404 | 410) => Robots::allow_all(),

                // Any other refusal — 401, 403, 451 — is a site declining to
                // show its rules rather than having none. Unreadable, so
                // treated as unreadable.
                Ok(_) => Robots::deny_all(),

                // A transport failure is not silence either. `deny_all`'s own
                // documentation already said it was for "a 500, or a timeout"
                // — the doc was right and the code was not, returning
                // allow_all and no retry. A host that selectively drops this
                // crawler's connections is a cheap and standard anti-bot
                // measure, and reading that as "no restrictions" rewarded it.
                // Retried once by `fetch_robots` before arriving here.
                Err(_) => Robots::deny_all(),
            };

            // The site's own gap applies from its first page, not from its
            // second. The politeness stamp was written before these rules
            // existed, so a site asking for thirty seconds still got its
            // first page one second after robots.txt.
            if let Some(delay) = rules.crawl_delay {
                let next = self.clock.now() + delay.max(self.limits.politeness);
                self.next_allowed.insert(host.clone(), next);
            }
            self.robots.insert(key.clone(), rules);
        }

        // A gap longer than this crawl can keep is refused rather than
        // quietly shortened.
        if let Some(delay) = self.robots[&key].crawl_delay {
            if delay > self.limits.max_crawl_delay {
                return false;
            }
        }
        self.robots[&key].allows(url)
    }

    /// Fetch `robots.txt`, once retried, and bounded in size.
    ///
    /// The retry is because the failure this distinguishes — a host dropping
    /// this crawler's connections — looks exactly like a transient network
    /// blip, and the consequence of getting it wrong is now refusing the
    /// whole host. One retry is cheap and makes the refusal mean something.
    fn fetch_robots(&self, robots_url: &str) -> Result<crate::fetch::Fetched, String> {
        let mut last = self.fetcher.fetch(robots_url);
        if last.is_err() {
            last = self.fetcher.fetch(robots_url);
        }
        // Google's documented cap. A body past it is not a rules file anyone
        // wrote, and the truncated remainder would parse to something the
        // site never said.
        if let Ok(r) = &last {
            if r.body.len() > MAX_ROBOTS_BYTES {
                return Ok(crate::fetch::Fetched {
                    status: r.status,
                    final_url: r.final_url.clone(),
                    content_type: r.content_type.clone(),
                    body: String::new(),
                    headers: r.headers.clone(),
                });
            }
        }
        last
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
/// Whether a body looks like `robots.txt` rather than an error page.
///
/// One directive is enough, and `user-agent` is the one every group must
/// begin with. Deliberately not a parse: this only decides whether parsing is
/// worth attempting on a response that was not a clean 200.
fn looks_like_robots(body: &str) -> bool {
    body.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim().to_ascii_lowercase())
        .any(|l| l.starts_with("user-agent:"))
}

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

    /// A bot check on `robots.txt` is never read as permission to crawl.
    ///
    /// The case this comes from: Cloudflare answers stackoverflow.com's
    /// `robots.txt` with status 418 and the real file in the body. 418 falls
    /// inside "permanently gone", so the rules were discarded as absent — and
    /// a file reading `User-agent: * / Disallow: /` was treated as no
    /// restrictions at all. Forge crawled a site that forbids it, and then
    /// reported the refusal it got as a bot check rather than as the site's
    /// own stated wish.
    #[test]
    fn a_challenged_robots_txt_does_not_become_permission() {
        let real_rules = "User-agent: *\nContent-signal: search=no, ai-train=no\nDisallow: /\n";
        // 418 with the real rules in the body and a Cloudflare header, which
        // is what stackoverflow.com actually serves.
        let fetcher = StaticFetcher::new()
            .with_response(
                "https://walled.example/robots.txt",
                Fetched {
                    status: 418,
                    final_url: "https://walled.example/robots.txt".into(),
                    content_type: "text/plain".into(),
                    body: real_rules.into(),
                    headers: vec![("cf-mitigated".into(), "challenge".into())],
                },
            )
            .with_page("https://walled.example/", "<html><body><p>Should never be read.</p></body></html>");
        let clock = FakeClock::default();
        let mut index = Index::new();
        let mut crawler = Crawler::new(&fetcher, &clock, Limits { politeness: 0.0, ..Default::default() });
        crawler.seed("https://walled.example/").unwrap();
        let report = crawler.run(&mut index);

        assert_eq!(index.len(), 0, "a site that disallows all crawling was indexed");
        assert_eq!(report.disallowed, 1, "the refusal was not attributed to robots.txt: {report:?}");
        assert_eq!(report.challenged, 0, "reported as a bot check rather than as the site's own rules");
    }

    /// And a challenged `robots.txt` whose body is not rules at all — an
    /// interstitial page — denies rather than guessing.
    #[test]
    fn a_challenged_robots_txt_with_no_rules_in_it_denies() {
        let fetcher = StaticFetcher::new().with_response(
            "https://walled.example/robots.txt",
            Fetched {
                status: 403,
                final_url: "https://walled.example/robots.txt".into(),
                content_type: "text/html".into(),
                body: "<html><body>Just a moment...</body></html>".into(),
                headers: Vec::new(),
            },
        );
        let clock = FakeClock::default();
        let mut index = Index::new();
        let mut crawler = Crawler::new(&fetcher, &clock, Limits { politeness: 0.0, ..Default::default() });
        crawler.seed("https://walled.example/").unwrap();
        let report = crawler.run(&mut index);
        assert_eq!(index.len(), 0);
        assert_eq!(report.disallowed, 1, "{report:?}");
    }

    /// A genuine 404 still means the site said nothing, which RFC 9309 reads
    /// as no restrictions — and a site with no `robots.txt` is the common case
    /// rather than an edge one.
    #[test]
    fn an_absent_robots_txt_is_still_permissive() {
        let fetcher = StaticFetcher::new()
            .with_response(
                "https://open.example/robots.txt",
                Fetched {
                    status: 404,
                    final_url: "https://open.example/robots.txt".into(),
                    content_type: "text/plain".into(),
                    body: "not found".into(),
                    headers: Vec::new(),
                },
            )
            .with_page(
                "https://open.example/",
                "<html><head><title>Open</title></head><body><p>Readable page here, with \
                 enough words in it to be worth indexing at all.</p></body></html>",
            );
        let clock = FakeClock::default();
        let mut index = Index::new();
        let mut crawler = Crawler::new(&fetcher, &clock, Limits { politeness: 0.0, ..Default::default() });
        crawler.seed("https://open.example/").unwrap();
        let report = crawler.run(&mut index);
        assert_eq!(report.disallowed, 0, "an absent robots.txt blocked the crawl: {report:?}");
        assert_eq!(index.len(), 1, "{report:?}");
    }

    /// The shape check that decides whether a non-200 body is worth parsing.
    #[test]
    fn a_body_is_recognised_as_rules_or_not() {
        assert!(looks_like_robots("User-agent: *\nDisallow: /\n"));
        assert!(looks_like_robots("# a comment\n\nuser-agent: Googlebot\n"));
        assert!(!looks_like_robots("<html><body>Just a moment...</body></html>"));
        assert!(!looks_like_robots(""));
        // A commented-out directive is not a directive.
        assert!(!looks_like_robots("# User-agent: *\n"));
    }

    /// A challenged page is refused, not missing, and never indexed.
    ///
    /// The 200 case is the one that matters. Nothing about the status says
    /// anything is wrong, so before this the interstitial was indexed as a
    /// page — a document whose entire text is "Just a moment... Enable
    /// JavaScript and cookies to continue", which then matches queries and
    /// answers nothing.
    #[test]
    fn a_challenge_is_neither_indexed_nor_counted_as_missing() {
        let challenge = Fetched {
            status: 200,
            final_url: "https://walled.example/".into(),
            content_type: "text/html".into(),
            headers: vec![("cf-mitigated".into(), "challenge".into())],
            body: "<html><title>Just a moment...</title><body>\
                   Enable JavaScript and cookies to continue</body></html>"
                .into(),
        };
        let fetcher = StaticFetcher::new().with_response("https://walled.example/", challenge);
        let clock = FakeClock::default();
        let limits = Limits { max_pages: 4, politeness: 0.0, ..Limits::default() };
        let mut index = Index::new();
        let mut crawler = Crawler::new(&fetcher, &clock, limits);
        crawler.seed("https://walled.example/").unwrap();
        let report = crawler.run(&mut index);

        assert_eq!(report.challenged, 1, "{report:?}");
        assert_eq!(report.missing, 0, "a refusal is not a missing page: {report:?}");
        assert_eq!(report.indexed, 0, "the interstitial was indexed as content");
        assert_eq!(index.len(), 0);
        // And the host is named, so a caller can say which site needs a
        // browser rather than only that something did.
        assert_eq!(report.challenged_hosts, vec!["walled.example".to_string()]);
    }

    /// A genuinely missing page is still missing — the distinction has to hold
    /// in both directions or it is not a distinction.
    #[test]
    fn a_missing_page_is_not_reported_as_challenged() {
        let fetcher = StaticFetcher::new();
        let clock = FakeClock::default();
        let limits = Limits { max_pages: 4, politeness: 0.0, ..Limits::default() };
        let mut index = Index::new();
        let mut crawler = Crawler::new(&fetcher, &clock, limits);
        crawler.seed("https://nowhere.example/gone").unwrap();
        let report = crawler.run(&mut index);
        assert_eq!(report.missing, 1, "{report:?}");
        assert_eq!(report.challenged, 0, "{report:?}");
        assert!(report.challenged_hosts.is_empty());
    }

    /// A site with a sidebar, which is the shape that exposed the frontier
    /// problem: every article carries the same navigation links, and they come
    /// before the body links in the document.
    fn site_with_a_sidebar() -> StaticFetcher {
        let mut f = StaticFetcher::new();
        for i in 1..=6 {
            f = f.with_page(
                &format!("https://a.example/article{i}"),
                &format!(
                    r#"<title>Article {i}</title>
                       <nav><a href="/about">About</a><a href="/help">Help</a>
                            <a href="/upload">Upload</a></nav>
                       <p>The subject of article {i}.</p>
                       <a href="/content{i}">Related {i}</a>"#
                ),
            );
        }
        for name in ["about", "help", "upload"] {
            f = f.with_page(
                &format!("https://a.example/{name}"),
                &format!("<title>{name}</title><p>Site furniture, not content.</p>"),
            );
        }
        for i in 1..=6 {
            f = f.with_page(
                &format!("https://a.example/content{i}"),
                &format!("<title>Content {i}</title><p>More about subject {i}.</p>"),
            );
        }
        f
    }

    /// The page budget should go to content, not to the sidebar.
    ///
    /// Measured against the real thing first: thirty pages crawled from six
    /// Wikipedia articles spent ten on `Main_Page`, `Help:Contents`,
    /// `Wikipedia:About`, `Portal:Current_events` and the upload wizard. Here
    /// the same shape in miniature — three nav links on all six articles, one
    /// content link on each. Breadth-first takes the nav links first because
    /// they come first in the document; there are exactly three pages of
    /// budget left after the seeds, and plain FIFO spends all three on them.
    #[test]
    fn the_site_template_does_not_eat_the_page_budget() {
        let fetcher = site_with_a_sidebar();
        let clock = FakeClock::default();
        let limits = Limits {
            max_pages: 9,
            max_depth: 3,
            politeness: 0.0,
            ..Limits::default()
        };
        let mut index = Index::new();
        let mut crawler = Crawler::new(&fetcher, &clock, limits);
        for i in 1..=6 {
            crawler.seed(&format!("https://a.example/article{i}")).unwrap();
        }
        crawler.run(&mut index);

        let urls: Vec<String> = index.urls().map(str::to_string).collect();
        let furniture: Vec<&String> = urls
            .iter()
            .filter(|u| ["/about", "/help", "/upload"].iter().any(|f| u.ends_with(f)))
            .collect();
        assert!(
            furniture.is_empty(),
            "budget went to the template: {furniture:?}"
        );
        let content = urls.iter().filter(|u| u.contains("/content")).count();
        assert_eq!(content, 3, "expected the three spare pages to be content: {urls:?}");
    }

    /// Deferring is not excluding. Given the budget to reach them, the
    /// template pages are still crawled — the false positive this heuristic
    /// can have on a tightly topical seed set costs a position, not a page.
    #[test]
    fn deferred_pages_are_still_reached_eventually() {
        let fetcher = site_with_a_sidebar();
        let clock = FakeClock::default();
        let limits = Limits {
            max_pages: 100,
            max_depth: 3,
            politeness: 0.0,
            ..Limits::default()
        };
        let mut index = Index::new();
        let mut crawler = Crawler::new(&fetcher, &clock, limits);
        for i in 1..=6 {
            crawler.seed(&format!("https://a.example/article{i}")).unwrap();
        }
        let report = crawler.run(&mut index);
        assert_eq!(report.remaining, 0, "crawl did not finish");
        let urls: Vec<String> = index.urls().map(str::to_string).collect();
        for name in ["/about", "/help", "/upload"] {
            assert!(
                urls.iter().any(|u| u.ends_with(name)),
                "{name} was dropped rather than deferred"
            );
        }
        assert_eq!(urls.len(), 15, "{urls:?}");
    }

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

    /// The attack this exists to stop, written as the attacker would run it.
    ///
    /// No cooperation from the model is needed: one page in an otherwise
    /// ordinary crawl redirects to loopback, and without the check the
    /// response is indexed and then read back to the model as though it were
    /// a page from the web.
    #[test]
    fn a_page_cannot_redirect_the_crawler_onto_the_local_machine() {
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/hop">next</a>"#)
            .with_redirect(
                "https://a.example/hop",
                "http://127.0.0.1:2375/containers/json",
                "<p>docker socket contents</p>",
            );

        let (index, report, _) = run(&fetcher, Limits::default());

        assert!(
            !index.contains_url("http://127.0.0.1:2375/containers/json"),
            "the local machine was indexed through a redirect"
        );
        assert_eq!(index.len(), 1, "the local response reached the index under some other name");
        assert_eq!(report.not_public, 1, "the refusal was not reported");
    }

    #[test]
    fn cloud_instance_metadata_is_refused_the_same_way() {
        // The other address worth naming: on a cloud host this one serves
        // credentials to anything that asks.
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/hop">next</a>"#)
            .with_redirect(
                "https://a.example/hop",
                "http://169.254.169.254/latest/meta-data/iam/security-credentials/",
                "<p>role credentials</p>",
            );
        let (index, report, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 1, "metadata reached the index");
        assert_eq!(report.not_public, 1);
    }

    #[test]
    fn a_crawl_cannot_be_seeded_at_a_private_address() {
        for address in [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://169.254.169.254/",
            "http://10.0.0.1/",
            "http://[::1]/",
        ] {
            let fetcher = StaticFetcher::new();
            let clock = FakeClock::default();
            let mut crawler = Crawler::new(&fetcher, &clock, Limits::default());
            assert!(
                crawler.seed(address).is_err(),
                "{address} was accepted as a crawl seed"
            );
        }
    }

    /// A redirect that crosses hosts must satisfy the destination's rules, not
    /// the rules of the host that was asked.
    #[test]
    fn a_redirect_to_another_host_consults_that_host_s_robots() {
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/hop">next</a>"#)
            .with_response(
                "https://b.example/robots.txt",
                Fetched {
                    status: 200,
                    final_url: "https://b.example/robots.txt".into(),
                    content_type: "text/plain".into(),
                    body: "User-agent: *\nDisallow: /\n".into(),
                    headers: Vec::new(),
                },
            )
            .with_redirect("https://a.example/hop", "https://b.example/secret", "<p>kept out</p>");

        let limits = Limits { stay_on_host: false, ..Default::default() };
        let (index, report, _) = run(&fetcher, limits);

        assert!(
            !index.contains_url("https://b.example/secret"),
            "indexed a page the destination host disallows"
        );
        assert!(report.disallowed >= 1, "the refusal was not reported");
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
                    headers: Vec::new(),
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
                    headers: Vec::new(),
                },
        );
        let (index, report, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 0, "crawled a site whose rules could not be read");
        assert!(report.disallowed > 0);
    }

    /// An unreachable `robots.txt` stops the host, and that is a reversal.
    ///
    /// It used to be read as absent, on the reasoning that a network blip
    /// should not stop a crawl over a file that may not exist. But a file
    /// that does not exist arrives as a 404, which is still treated as
    /// permission — `Err` means the server could not be talked to at all.
    /// A host that selectively drops this crawler's connections is a cheap
    /// and standard anti-bot measure, and reading that as "no restrictions"
    /// rewarded it. `deny_all`'s own documentation already said it covered
    /// "a 500, or a timeout"; the code disagreed with the doc, and the doc
    /// was right.
    ///
    /// The fetch is retried once first, so a genuine blip does not cost the
    /// host.
    #[test]
    fn an_unreachable_robots_file_stops_the_host_rather_than_opening_it() {
        let fetcher = site().with_unreachable("https://a.example/robots.txt");
        let (index, report, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 0, "crawled a host whose rules could not be read at all");
        assert!(report.disallowed > 0, "the refusal was not reported");
    }

    /// The distinction that makes the reversal above affordable: a site with
    /// no robots.txt answers 404, and 404 is still permission.
    #[test]
    fn a_site_with_no_robots_file_is_still_crawled() {
        let fetcher = site().with_response(
            "https://a.example/robots.txt",
            Fetched {
                status: 404,
                final_url: "https://a.example/robots.txt".into(),
                content_type: "text/plain".into(),
                body: "not found".into(),
                headers: Vec::new(),
            },
        );
        let (index, _, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 4, "an absent robots.txt blocked the crawl");
    }

    /// A 200 whose body is not a rules file is not permission either.
    ///
    /// A robots.txt that redirects to a login page, or an SPA serving its
    /// shell for unknown paths, arrives as a clean 200 that parses to zero
    /// directives — the most permissive possible answer to a question nobody
    /// answered.
    #[test]
    fn a_200_that_is_not_a_rules_file_is_not_read_as_permission() {
        let fetcher = site().with_response(
            "https://a.example/robots.txt",
            Fetched {
                status: 200,
                final_url: "https://a.example/login".into(),
                content_type: "text/html".into(),
                body: "<html><body><h1>Sign in to continue</h1></body></html>".into(),
                headers: Vec::new(),
            },
        );
        let (index, report, _) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 0, "a login page was read as robots.txt permission");
        assert!(report.disallowed > 0);
    }

    /// A 429 is the server asking for less while it is struggling. It used
    /// to be counted as "missing" and the crawl carried straight on.
    #[test]
    fn a_rate_limited_host_is_backed_off_rather_than_hammered() {
        let fetcher = site().with_response(
            "https://a.example/two",
            Fetched {
                status: 429,
                final_url: "https://a.example/two".into(),
                content_type: "text/html".into(),
                body: String::new(),
                headers: vec![("retry-after".into(), "120".into())],
            },
        );
        let (_, report, clock) = run(&fetcher, Limits::default());

        assert_eq!(report.rate_limited, 1, "the 429 was not recognised as one");
        assert_eq!(report.missing, 0, "a 429 was filed as a missing page");
        assert!(
            clock.waits.borrow().iter().any(|&w| w >= 119.0),
            "asked to wait 120s and did not: {:?}",
            clock.waits.borrow()
        );
    }

    #[test]
    fn a_503_without_a_retry_after_still_backs_off() {
        let fetcher = site().with_response(
            "https://a.example/two",
            Fetched {
                status: 503,
                final_url: "https://a.example/two".into(),
                content_type: "text/html".into(),
                body: String::new(),
                headers: Vec::new(),
            },
        );
        let (_, report, clock) = run(&fetcher, Limits::default());
        assert_eq!(report.rate_limited, 1);
        assert!(
            clock.waits.borrow().iter().any(|&w| w >= 59.0),
            "no default stand-off was taken: {:?}",
            clock.waits.borrow()
        );
    }

    #[test]
    fn retry_after_is_read_in_both_of_its_forms() {
        assert_eq!(parse_retry_after("120"), Some(120.0));
        assert_eq!(parse_retry_after("  30  "), Some(30.0));
        assert_eq!(parse_retry_after("-5"), None);
        assert_eq!(parse_retry_after("soon"), None);
        // An HTTP-date is recognised as a date rather than parsed into a
        // number nobody sent.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), Some(DEFAULT_BACKOFF));
    }

    /// A re-crawl asks whether the page changed, rather than asking for it.
    #[test]
    fn a_second_crawl_sends_the_validators_the_first_one_was_given() {
        // A fetcher that records what it was asked, and answers 304 to any
        // conditional request — which is what a server does for a page that
        // has not changed.
        struct Conditional {
            asked: std::cell::RefCell<Vec<(String, String)>>,
        }
        impl Fetcher for Conditional {
            fn fetch(&self, url: &str) -> Result<Fetched, String> {
                self.fetch_conditional(url, "", "")
            }
            fn fetch_conditional(
                &self,
                url: &str,
                etag: &str,
                _last_modified: &str,
            ) -> Result<Fetched, String> {
                self.asked.borrow_mut().push((url.to_string(), etag.to_string()));
                if url.ends_with("robots.txt") {
                    return Ok(Fetched {
                        status: 404,
                        final_url: url.into(),
                        content_type: "text/plain".into(),
                        body: "none".into(),
                        headers: Vec::new(),
                    });
                }
                if !etag.is_empty() {
                    return Ok(Fetched {
                        status: 304,
                        final_url: url.into(),
                        content_type: String::new(),
                        body: String::new(),
                        headers: Vec::new(),
                    });
                }
                Ok(Fetched {
                    status: 200,
                    final_url: url.into(),
                    content_type: "text/html".into(),
                    body: "<title>P</title><p>Some words about allocators.</p>".into(),
                    headers: vec![("etag".into(), "\"abc123\"".into())],
                })
            }
        }

        let fetcher = Conditional { asked: std::cell::RefCell::new(Vec::new()) };
        let clock = FakeClock::default();
        let limits = Limits { politeness: 0.0, max_depth: 0, ..Default::default() };
        let mut index = Index::new();

        let mut first = Crawler::new(&fetcher, &clock, limits.clone());
        first.seed("https://a.example/page").unwrap();
        let r1 = first.run(&mut index);
        assert_eq!(r1.indexed, 1, "the first crawl did not index the page");

        // The validator has to survive a save, or the next process asks
        // unconditionally and the whole thing is decorative.
        let dir = std::env::temp_dir().join(format!("forge-cond-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("idx");
        index.save(&path).unwrap();
        let mut reloaded = Index::load(&path).unwrap();

        let mut second = Crawler::new(&fetcher, &clock, limits);
        second.seed("https://a.example/page").unwrap();
        let r2 = second.run(&mut reloaded);

        assert_eq!(r2.unchanged, 1, "the 304 was not recognised as unchanged");
        assert_eq!(r2.indexed, 0, "an unchanged page was reindexed");
        assert_eq!(reloaded.len(), 1, "the copy was lost");

        let asked = fetcher.asked.borrow();
        let page_asks: Vec<_> = asked.iter().filter(|(u, _)| !u.ends_with("robots.txt")).collect();
        assert_eq!(page_asks.len(), 2, "expected two page requests: {page_asks:?}");
        assert_eq!(page_asks[0].1, "", "the first crawl should have no validator");
        assert_eq!(
            page_asks[1].1, "\"abc123\"",
            "the second crawl did not send the ETag the first was given"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A gap longer than the crawl can keep is refused, not shortened.
    #[test]
    fn a_crawl_delay_beyond_what_we_can_honour_is_refused() {
        let fetcher = site().with_response(
            "https://a.example/robots.txt",
            Fetched {
                status: 200,
                final_url: "https://a.example/robots.txt".into(),
                content_type: "text/plain".into(),
                // One visit a day. Used to be clamped to five minutes.
                body: "User-agent: *\nCrawl-delay: 86400\n".into(),
                headers: Vec::new(),
            },
        );
        let (index, _, clock) = run(&fetcher, Limits::default());
        assert_eq!(index.len(), 0, "crawled a site 288x faster than it asked");
        assert!(
            clock.waits.borrow().iter().all(|&w| w < 86400.0),
            "waited the full day instead of declining"
        );
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
                    headers: Vec::new(),
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
                        headers: Vec::new(),
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

#[cfg(test)]
mod budget_tests {
    use super::*;
    use crate::fetch::StaticFetcher;

    /// A page limit bounds the work; it does not bound the time. One slow host
    /// can hold a small crawl for minutes, which is why anything with a caller
    /// waiting needs a limit on the clock too.
    #[test]
    fn the_clock_can_end_a_crawl_the_page_limit_would_not() {
        // Twenty pages all linked from the seed, so the page limit is nowhere
        // near reached.
        let mut fetcher = StaticFetcher::new().with_page(
            "https://a.example/",
            &(0..20)
                .map(|i| format!(r#"<a href="/p{i}">{i}</a>"#))
                .collect::<String>(),
        );
        for i in 0..20 {
            fetcher = fetcher.with_page(&format!("https://a.example/p{i}"), "<p>page</p>");
        }

        // A second of politeness per page, and four seconds to spend.
        let limits = Limits {
            max_pages: 500,
            politeness: 1.0,
            max_seconds: Some(4.0),
            ..Default::default()
        };
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = {
            let mut c = Crawler::new(&fetcher, &clock, limits);
            c.seed("https://a.example/").unwrap();
            c.run(&mut index)
        };

        assert!(report.timed_out, "the clock did not stop the crawl: {report:?}");
        assert!(report.fetched < 21, "fetched everything despite the budget: {report:?}");
        assert!(report.remaining > 0, "stopped but reported nothing left");
        // And it did index what it managed to reach, rather than nothing.
        assert!(index.len() > 0, "a timed-out crawl indexed nothing");
    }

    /// Without a budget the crawl runs to its page limit, so the default
    /// behaviour is unchanged.
    #[test]
    fn no_budget_means_no_time_limit() {
        let fetcher = StaticFetcher::new()
            .with_page("https://a.example/", r#"<a href="/x">x</a>"#)
            .with_page("https://a.example/x", "<p>x</p>");
        let limits = Limits { politeness: 100.0, max_seconds: None, ..Default::default() };
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = {
            let mut c = Crawler::new(&fetcher, &clock, limits);
            c.seed("https://a.example/").unwrap();
            c.run(&mut index)
        };
        assert!(!report.timed_out);
        assert_eq!(index.len(), 2, "a long politeness delay stopped the crawl");
    }
}
