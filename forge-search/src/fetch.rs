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
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            };
            assert!(!f.is_html(), "{ct} was treated as html");
        }
    }
}
