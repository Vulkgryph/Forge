//! What the engine needs from the network, expressed as a trait it does not
//! implement.
//!
//! HTTP over TLS is the one part of this engine that should not be written
//! here. Not because it is hard to parse — HTTP/1.1 is a few hundred lines —
//! but because the TLS underneath it is constant-time cryptography and
//! certificate chain validation, where a subtle mistake is not a crash but a
//! silent, exploitable hole. Writing that would make this engine less
//! trustworthy, not more independent.
//!
//! So fetching is a hole in the shape of [`Fetcher`], and the caller fills it:
//! Forge with the HTTP client it already carries, another host with whatever it
//! has. The crate keeps its property of having no dependencies, and the reason
//! is a judgement about what is safe to own rather than a convenience.
//!
//! It also makes the crawler testable. A crawl driven by a real network is
//! slow, needs the internet, and gives different answers on different days; one
//! driven by [`StaticFetcher`] is none of those, which is why the crawl tests
//! can assert what they assert.

use std::collections::HashMap;

/// One fetched page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fetched {
    /// The HTTP status. The crawler decides what to do with it — 404 means
    /// forget the URL, 503 means try later, and only the crawler knows which.
    pub status: u16,
    /// Where the content actually came from, after redirects. Indexing the
    /// requested URL when the server redirected would record an address whose
    /// content lives elsewhere, and the next crawl would follow the redirect
    /// again and index a duplicate.
    pub final_url: String,
    /// `Content-Type`, lowercased, without parameters — `text/html`, not
    /// `text/html; charset=utf-8`.
    pub content_type: String,
    /// Response headers, names lowercased.
    ///
    /// Carried because some things a crawler needs to know are only in the
    /// headers and cannot be inferred from the body. The one this was added
    /// for is `cf-mitigated`, which says outright that a bot-management
    /// challenge was served instead of the page — a fact worth having as a
    /// stated header rather than guessed from HTML.
    ///
    /// A list rather than a map: responses have few headers, lookup is by a
    /// handful of known names, and a `Vec` keeps `Fetched` cheap to build for
    /// the fetchers that have nothing to put here.
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Fetched {
    pub fn is_html(&self) -> bool {
        self.content_type.is_empty()
            || self.content_type.contains("html")
            || self.content_type.contains("xhtml")
    }

    pub fn is_ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Whether the status says the URL will not become available by waiting —
    /// so the crawler can forget it rather than retrying forever.
    pub fn is_permanently_gone(&self) -> bool {
        matches!(self.status, 400..=499 if self.status != 408 && self.status != 429)
    }

    /// The first header with this name, lowercased comparison.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Whether a bot-management interstitial was served instead of the page,
    /// and which system served it.
    ///
    /// This is a distinct outcome from the page being missing, and conflating
    /// the two tells a caller the wrong thing: a 404 means the URL is wrong,
    /// while a challenge means the URL is fine and the *access* is wrong. The
    /// responses are different — correct the address, or open it in a browser.
    ///
    /// Worse, some of these arrive with status 200 and get indexed as content.
    /// A crawl of a challenged site then holds a page whose text is "Just a
    /// moment... Enable JavaScript and cookies to continue", which will match
    /// queries and answer nothing.
    ///
    /// Detection is by vendor, not by site, and in order of how much the
    /// server is actually telling us:
    ///
    /// 1. `cf-mitigated`, which Cloudflare sets to say a challenge was served.
    ///    A stated fact, not an inference.
    /// 2. Headers particular to other bot-management systems.
    /// 3. A body signature, for the ones that announce themselves only in
    ///    HTML.
    ///
    /// The last of those is bounded by size, and that bound matters: plenty of
    /// ordinary pages carry "please enable JavaScript" in a `<noscript>`, so
    /// the phrase alone would misread them. A challenge page is nearly empty —
    /// the one measured was 5.5 kB — while a real page carrying such a notice
    /// is almost never under sixteen.
    pub fn challenge(&self) -> Option<&'static str> {
        // Cloudflare says so outright.
        if self.header("cf-mitigated").is_some() {
            return Some("Cloudflare");
        }
        if self.header("x-datadome").is_some() {
            return Some("DataDome");
        }
        if self.header("x-iinfo").is_some() {
            return Some("Imperva");
        }
        if self
            .header("server")
            .is_some_and(|v| v.eq_ignore_ascii_case("AkamaiGHost"))
            && !self.is_ok()
        {
            return Some("Akamai");
        }

        if !self.is_html() {
            return None;
        }
        let body = self.body.to_lowercase();
        // Signatures that name the system that produced them.
        for (marker, vendor) in [
            ("cf-browser-verification", "Cloudflare"),
            ("__cf_chl", "Cloudflare"),
            ("/cdn-cgi/challenge-platform", "Cloudflare"),
            ("px-captcha", "PerimeterX"),
            ("_px_captcha", "PerimeterX"),
            ("datadome", "DataDome"),
        ] {
            if body.contains(marker) {
                return Some(vendor);
            }
        }

        // And the generic interstitial, size-bounded as above.
        const INTERSTITIAL_MAX: usize = 16 * 1024;
        if self.body.len() < INTERSTITIAL_MAX
            && (body.contains("just a moment")
                || (body.contains("enable javascript") && body.contains("cookie")))
        {
            return Some("an unidentified bot check");
        }
        None
    }
}

/// Where pages come from.
///
/// Implementations are expected to follow redirects and report the final URL,
/// to apply their own timeout, and to return `Err` only when there is no
/// response at all. An HTTP error is a `Fetched` with that status, not an
/// `Err`: the crawler's response to 404 and to "the network is down" are
/// different, and collapsing them loses that.
pub trait Fetcher {
    fn fetch(&self, url: &str) -> Result<Fetched, String>;

    /// How this fetcher identifies itself, for `robots.txt` matching.
    ///
    /// A crawler that reads `robots.txt` and then cannot say which rules apply
    /// to it is only pretending to obey the file.
    fn user_agent(&self) -> &str {
        "forge-search"
    }
}

/// A fetcher backed by a fixed map, for tests and for indexing a corpus that
/// is already in hand.
///
/// This is not a mock in the sense of something that asserts how it was
/// called. It is a real implementation of the trait over a different source,
/// which is what makes a crawl test a test of the crawler rather than of a
/// test double.
#[derive(Clone, Debug, Default)]
pub struct StaticFetcher {
    pages: HashMap<String, Fetched>,
    /// URLs that fail at the transport level, to exercise the difference
    /// between "no response" and "a response saying no".
    unreachable: Vec<String>,
}

impl StaticFetcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an HTML page at `url`.
    pub fn with_page(mut self, url: &str, body: &str) -> Self {
        self.pages.insert(
            url.to_string(),
            Fetched {
                status: 200,
                final_url: url.to_string(),
                content_type: "text/html".into(),
                body: body.to_string(),
                    headers: Vec::new(),
                },
        );
        self
    }

    /// Add a response with a chosen status and content type.
    pub fn with_response(mut self, url: &str, response: Fetched) -> Self {
        self.pages.insert(url.to_string(), response);
        self
    }

    /// Add a redirect: fetching `from` returns `to`'s content with
    /// `final_url` set to `to`.
    pub fn with_redirect(mut self, from: &str, to: &str, body: &str) -> Self {
        self.pages.insert(
            from.to_string(),
            Fetched {
                status: 200,
                final_url: to.to_string(),
                content_type: "text/html".into(),
                body: body.to_string(),
                    headers: Vec::new(),
                },
        );
        self
    }

    /// Make `url` fail with no response at all.
    pub fn with_unreachable(mut self, url: &str) -> Self {
        self.unreachable.push(url.to_string());
        self
    }

    /// How many distinct URLs this fetcher can serve — a crawl test's upper
    /// bound on what it could possibly have visited.
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }
}

impl Fetcher for StaticFetcher {
    fn fetch(&self, url: &str) -> Result<Fetched, String> {
        if self.unreachable.iter().any(|u| u == url) {
            return Err(format!("could not reach {url}"));
        }
        match self.pages.get(url) {
            Some(f) => Ok(f.clone()),
            // A URL the fetcher has never heard of is a 404 rather than an
            // error, which is what a real server says about a page that is not
            // there.
            None => Ok(Fetched {
                status: 404,
                final_url: url.to_string(),
                content_type: "text/plain".into(),
                body: String::new(),
                    headers: Vec::new(),
                }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, headers: &[(&str, &str)], body: &str) -> Fetched {
        Fetched {
            status,
            final_url: "https://a.example/".into(),
            content_type: "text/html".into(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_string(),
        }
    }

    /// The real response, as measured: a 403 with `cf-mitigated: challenge`
    /// and 5.5 kB of interstitial where the page should be.
    #[test]
    fn a_cloudflare_challenge_is_recognised() {
        let got = response(
            403,
            &[("cf-mitigated", "challenge"), ("server", "cloudflare")],
            "<html><head><title>Just a moment...</title></head><body>\
             Enable JavaScript and cookies to continue</body></html>",
        );
        assert_eq!(got.challenge(), Some("Cloudflare"));
    }

    /// The header alone is enough, because it is the server stating the fact
    /// rather than us inferring it from HTML that may change.
    #[test]
    fn the_header_alone_identifies_a_challenge() {
        let got = response(403, &[("cf-mitigated", "challenge")], "");
        assert_eq!(got.challenge(), Some("Cloudflare"));
    }

    /// The dangerous case: a challenge served with status 200. Nothing about
    /// the status says anything is wrong, so without this it is indexed as
    /// content and answers later queries with "Just a moment".
    #[test]
    fn a_challenge_served_as_200_is_still_a_challenge() {
        let got = response(
            200,
            &[],
            "<html><body><h1>Just a moment...</h1>\
             <p>Enable JavaScript and cookies to continue</p></body></html>",
        );
        assert!(got.is_ok(), "the fixture should look successful");
        assert_eq!(got.challenge(), Some("an unidentified bot check"));
    }

    /// Other systems, by their own headers.
    #[test]
    fn other_bot_managers_are_recognised_too() {
        assert_eq!(response(403, &[("x-datadome", "protected")], "").challenge(), Some("DataDome"));
        assert_eq!(response(403, &[("x-iinfo", "1-2-3")], "").challenge(), Some("Imperva"));
        assert_eq!(
            response(403, &[("server", "AkamaiGHost")], "").challenge(),
            Some("Akamai"),
        );
        // Akamai serves plenty of ordinary pages; a 200 from it is not a
        // challenge.
        assert_eq!(response(200, &[("server", "AkamaiGHost")], "<p>fine</p>").challenge(), None);
    }

    /// The signature check must not fire on an ordinary page.
    ///
    /// This is the false positive that matters: a great many real pages carry
    /// "please enable JavaScript" in a `<noscript>`, and reading those as
    /// challenges would silently drop them from every crawl. The size bound is
    /// what separates them — a challenge page is nearly empty, a real page
    /// carrying the notice is not.
    #[test]
    fn a_real_page_mentioning_javascript_is_not_a_challenge() {
        let real = format!(
            "<html><body><noscript>Please enable JavaScript and cookies.</noscript>{}</body></html>",
            "<p>Actual article content that goes on for a while. </p>".repeat(400),
        );
        assert!(real.len() > 16 * 1024, "fixture is not big enough to be a real page");
        assert_eq!(response(200, &[], &real).challenge(), None);
    }

    /// A short page that simply says nothing about JavaScript is fine too.
    #[test]
    fn an_ordinary_short_page_is_not_a_challenge() {
        assert_eq!(response(200, &[], "<p>A brief but genuine page.</p>").challenge(), None);
    }

    /// Non-HTML cannot be an interstitial, and JSON that happens to contain
    /// the words must not be read as one.
    #[test]
    fn non_html_is_never_a_challenge_by_signature() {
        let mut json = response(200, &[], "{\"error\":\"just a moment, enable javascript cookie\"}");
        json.content_type = "application/json".into();
        assert_eq!(json.challenge(), None);
    }

    /// A 404 is a missing page, not a refused one — the distinction the whole
    /// method exists to preserve.
    #[test]
    fn a_plain_404_is_not_a_challenge() {
        let got = response(404, &[("server", "nginx")], "<h1>Not Found</h1>");
        assert_eq!(got.challenge(), None);
        assert!(got.is_permanently_gone());
    }

    #[test]
    fn headers_are_read_case_insensitively() {
        let got = response(200, &[("Content-Language", "en")], "");
        assert_eq!(got.header("content-language"), Some("en"));
        assert_eq!(got.header("CONTENT-LANGUAGE"), Some("en"));
        assert_eq!(got.header("absent"), None);
    }

    #[test]
    fn a_static_fetcher_serves_what_it_was_given() {
        let f = StaticFetcher::new().with_page("https://a.example/", "<p>hello</p>");
        let got = f.fetch("https://a.example/").unwrap();
        assert!(got.is_ok());
        assert!(got.is_html());
        assert_eq!(got.body, "<p>hello</p>");
        assert_eq!(got.final_url, "https://a.example/");
    }

    /// An unknown URL is a 404, not an error. A real server says the same, and
    /// the crawler's handling of the two differs.
    #[test]
    fn an_unknown_url_is_a_404_not_an_error() {
        let f = StaticFetcher::new();
        let got = f.fetch("https://a.example/missing").unwrap();
        assert_eq!(got.status, 404);
        assert!(!got.is_ok());
        assert!(got.is_permanently_gone());
    }

    /// And a transport failure is an error, not a status. Collapsing the two
    /// would lose the difference between "there is no such page" and "the
    /// network is down", which deserve different responses.
    #[test]
    fn an_unreachable_url_is_an_error() {
        let f = StaticFetcher::new().with_unreachable("https://down.example/");
        assert!(f.fetch("https://down.example/").is_err());
    }

    #[test]
    fn a_redirect_reports_where_the_content_came_from() {
        let f = StaticFetcher::new()
            .with_redirect("https://a.example/old", "https://a.example/new", "<p>moved</p>");
        let got = f.fetch("https://a.example/old").unwrap();
        assert_eq!(got.final_url, "https://a.example/new");
        assert_eq!(got.body, "<p>moved</p>");
    }

    /// Which statuses are worth retrying is the crawler's decision, so the
    /// classification has to be right: 429 and 408 are temporary even though
    /// they are 4xx.
    #[test]
    fn temporary_failures_are_not_treated_as_gone() {
        let gone = |status| Fetched {
            status,
            final_url: "https://a.example/".into(),
            content_type: String::new(),
            body: String::new(),
                headers: Vec::new(),
            };
        assert!(gone(404).is_permanently_gone());
        assert!(gone(410).is_permanently_gone());
        assert!(gone(403).is_permanently_gone());
        // Rate limited and request timeout — try again later.
        assert!(!gone(429).is_permanently_gone());
        assert!(!gone(408).is_permanently_gone());
        // Server errors are the server's problem, and temporary.
        assert!(!gone(500).is_permanently_gone());
        assert!(!gone(503).is_permanently_gone());
    }

    /// A response with no content type is assumed to be HTML rather than
    /// skipped: plenty of servers omit it, and refusing them silently loses
    /// pages.
    #[test]
    fn a_missing_content_type_is_assumed_html() {
        let f = Fetched {
            status: 200,
            final_url: "https://a.example/".into(),
            content_type: String::new(),
            body: "<p>x</p>".into(),
                headers: Vec::new(),
            };
        assert!(f.is_html());
    }

    #[test]
    fn non_html_content_is_recognised() {
        for ct in ["application/pdf", "image/png", "application/json", "text/css"] {
            let f = Fetched {
                status: 200,
                final_url: "https://a.example/x".into(),
                content_type: ct.into(),
                body: String::new(),
                    headers: Vec::new(),
                };
            assert!(!f.is_html(), "{ct} was treated as html");
        }
    }
}
