//! URLs: parsing, resolving a link against the page it was found on, and
//! reducing two spellings of one address to the same string.
//!
//! The last of those is why this file exists. A crawler that cannot tell
//! `http://Example.COM/a/` from `http://example.com/a` fetches the same page
//! twice, indexes it twice, and returns it twice — and the effect compounds,
//! because every link on that page is then followed twice as well. Normalising
//! is not tidiness here; it is what stops a crawl multiplying.
//!
//! Enough of RFC 3986 to do that, and no more. There is no attempt at
//! userinfo, IPv6 literals, or internationalised hosts: a crawler that meets
//! one can decline it, and a partial implementation that is honest about its
//! limits is better than one that mangles what it does not understand.

/// A URL broken into the parts that matter for crawling.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Url {
    /// Lowercased, without the `:`. Only `http` and `https` are accepted.
    pub scheme: String,
    /// Lowercased, without a port.
    pub host: String,
    /// `None` when the scheme's default applies, so `example.com` and
    /// `example.com:443` compare equal under https.
    pub port: Option<u16>,
    /// Always begins with `/`, with `.` and `..` resolved away.
    pub path: String,
    /// Without the `?`. Empty when absent.
    pub query: String,
}

impl Url {
    /// Parse an absolute http(s) URL.
    ///
    /// Anything else — a `mailto:`, a `javascript:`, a bare word — is refused
    /// rather than guessed at. A crawler follows links written by strangers,
    /// and the cost of guessing is fetching something that was never a page.
    pub fn parse(input: &str) -> Result<Self, String> {
        let input = input.trim();
        let (scheme, rest) = input
            .split_once("://")
            .ok_or_else(|| format!("no scheme in {input:?}"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "http" && scheme != "https" {
            return Err(format!("scheme {scheme:?} is not http or https"));
        }
        // The fragment never reaches a server, so it is dropped here rather
        // than carried and ignored — two URLs differing only by fragment are
        // the same page, and keeping it would make them look different.
        let rest = rest.split('#').next().unwrap_or("");
        let (authority, tail) = match rest.find(['/', '?']) {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if authority.is_empty() {
            return Err(format!("no host in {input:?}"));
        }
        // Credentials in a URL are not something a crawler should carry around,
        // and a host containing `@` is more often an attempt to disguise one.
        if authority.contains('@') {
            return Err("credentials in a URL are not supported".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => {
                let port: u16 = p
                    .parse()
                    .map_err(|_| format!("port {p:?} is not a number"))?;
                (h, Some(port))
            }
            None => (authority, None),
        };
        let host = host.to_ascii_lowercase();
        if host.is_empty() || host.contains('/') {
            return Err(format!("bad host in {input:?}"));
        }
        // A default port is dropped, so the two spellings compare equal.
        let port = port.filter(|&p| !is_default_port(&scheme, p));

        let (path, query) = match tail.split_once('?') {
            Some((p, q)) => (p, q),
            None => (tail, ""),
        };
        let path = if path.is_empty() { "/" } else { path };

        Ok(Self {
            scheme,
            host,
            port,
            path: normalise_path(path),
            query: query.to_string(),
        })
    }

    /// Resolve `link` as it appeared on the page at `self`.
    ///
    /// Follows RFC 3986's reference resolution for the forms that occur in real
    /// pages: absolute, protocol-relative, root-relative, relative, and
    /// query-or-fragment-only.
    pub fn join(&self, link: &str) -> Result<Self, String> {
        let link = link.trim();
        if link.is_empty() {
            return Err("empty link".into());
        }
        // Absolute.
        if link.contains("://") {
            return Self::parse(link);
        }
        // Protocol-relative: `//host/path` inherits the scheme.
        if let Some(rest) = link.strip_prefix("//") {
            return Self::parse(&format!("{}://{rest}", self.scheme));
        }
        // A scheme we do not follow — `mailto:`, `tel:`, `javascript:`. Caught
        // before the relative cases, or `mailto:a@b` is read as a path.
        if let Some((maybe_scheme, _)) = link.split_once(':') {
            if !maybe_scheme.is_empty()
                && maybe_scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-')
                && !maybe_scheme.contains('/')
                && link[..maybe_scheme.len() + 1].len() < 12
            {
                return Err(format!("scheme {maybe_scheme:?} is not followed"));
            }
        }
        let base = |path: &str, query: &str| Self {
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            port: self.port,
            path: normalise_path(path),
            query: query.to_string(),
        };
        // Fragment only — the same page.
        if link.starts_with('#') {
            return Ok(base(&self.path, &self.query));
        }
        // Query only.
        if let Some(q) = link.strip_prefix('?') {
            return Ok(base(&self.path, q.split('#').next().unwrap_or("")));
        }
        let link = link.split('#').next().unwrap_or("");
        let (link_path, link_query) = match link.split_once('?') {
            Some((p, q)) => (p, q),
            None => (link, ""),
        };
        // Root-relative.
        if link_path.starts_with('/') {
            return Ok(base(link_path, link_query));
        }
        // Relative: against the base's directory, which is everything up to
        // and including its last `/`.
        let dir = match self.path.rfind('/') {
            Some(i) => &self.path[..=i],
            None => "/",
        };
        Ok(base(&format!("{dir}{link_path}"), link_query))
    }

    /// The host, with a port when it is not the scheme's default.
    pub fn authority(&self) -> String {
        match self.port {
            Some(p) => format!("{}:{}", self.host, p),
            None => self.host.clone(),
        }
    }

    /// The canonical string form — the key a crawler deduplicates on.
    pub fn as_string(&self) -> String {
        let mut s = format!("{}://{}{}", self.scheme, self.authority(), self.path);
        if !self.query.is_empty() {
            s.push('?');
            s.push_str(&self.query);
        }
        s
    }

    /// Whether `other` is on the same host, for a crawl that stays on a site.
    pub fn same_host(&self, other: &Url) -> bool {
        self.host == other.host
    }
}

impl std::fmt::Display for Url {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.as_string())
    }
}

fn is_default_port(scheme: &str, port: u16) -> bool {
    (scheme == "http" && port == 80) || (scheme == "https" && port == 443)
}

/// Resolve `.` and `..`, collapse `//`, and guarantee a leading `/`.
///
/// `..` above the root stays at the root rather than escaping, which is what
/// RFC 3986 requires and also what stops a crafted link reaching outside the
/// site a crawl is confined to.
fn normalise_path(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let trailing = path.ends_with('/') || path.ends_with("/.") || path.ends_with("/..");
    let mut s = String::with_capacity(path.len());
    for seg in &out {
        s.push('/');
        s.push_str(seg);
    }
    if s.is_empty() {
        return "/".to_string();
    }
    // A trailing slash is kept, because `/a/` and `/a` are genuinely different
    // addresses to many servers — normalising them together would merge two
    // pages, which is worse than crawling one twice.
    if trailing {
        s.push('/');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Url {
        Url::parse(s).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    #[test]
    fn a_plain_url_parses_into_its_parts() {
        let u = p("https://doc.rust-lang.org/std/vec/index.html?x=1");
        assert_eq!(u.scheme, "https");
        assert_eq!(u.host, "doc.rust-lang.org");
        assert_eq!(u.port, None);
        assert_eq!(u.path, "/std/vec/index.html");
        assert_eq!(u.query, "x=1");
    }

    /// The reason this file exists: these must all reduce to one string, or a
    /// crawl multiplies.
    #[test]
    fn equivalent_spellings_normalise_together() {
        let canonical = "https://example.com/a/b";
        for variant in [
            "https://example.com/a/b",
            "https://EXAMPLE.com/a/b",
            "https://example.com:443/a/b",
            "https://example.com/a/b#section",
            "https://example.com/./a/b",
            "https://example.com/a/c/../b",
            "https://example.com//a//b",
            "  https://example.com/a/b  ",
        ] {
            assert_eq!(p(variant).as_string(), canonical, "{variant:?}");
        }
    }

    /// And the http default port, which is a different number.
    #[test]
    fn the_http_default_port_is_also_dropped() {
        assert_eq!(p("http://example.com:80/x").as_string(), "http://example.com/x");
        // A non-default port is kept, because it is a different server.
        assert_eq!(p("http://example.com:8080/x").as_string(), "http://example.com:8080/x");
    }

    /// `/a/` and `/a` are genuinely different addresses to many servers, so
    /// merging them would hide a page rather than save a fetch.
    #[test]
    fn a_trailing_slash_is_significant() {
        assert_ne!(p("https://example.com/a/").as_string(), p("https://example.com/a").as_string());
        assert_eq!(p("https://example.com/a/").as_string(), "https://example.com/a/");
    }

    #[test]
    fn a_bare_host_gets_a_root_path() {
        assert_eq!(p("https://example.com").as_string(), "https://example.com/");
        assert_eq!(p("https://example.com?q=1").as_string(), "https://example.com/?q=1");
    }

    /// A crawler follows links written by strangers, so anything that is not
    /// an http(s) page is refused rather than guessed at.
    #[test]
    fn non_http_schemes_are_refused() {
        for bad in [
            "mailto:someone@example.com",
            "javascript:alert(1)",
            "ftp://example.com/x",
            "file:///etc/passwd",
            "data:text/html,hello",
            "not a url",
            "",
            "https://",
            "https://example.com:notaport/",
            "https://user:pass@example.com/",
        ] {
            assert!(Url::parse(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn an_absolute_link_ignores_the_base() {
        let base = p("https://a.example/dir/page.html");
        assert_eq!(base.join("https://b.example/x").unwrap().as_string(), "https://b.example/x");
    }

    #[test]
    fn a_root_relative_link_keeps_the_host() {
        let base = p("https://a.example/dir/page.html");
        assert_eq!(base.join("/other").unwrap().as_string(), "https://a.example/other");
    }

    /// A relative link resolves against the base's *directory*, not the base.
    #[test]
    fn a_relative_link_resolves_against_the_directory() {
        let base = p("https://a.example/dir/page.html");
        assert_eq!(base.join("next.html").unwrap().as_string(), "https://a.example/dir/next.html");
        assert_eq!(base.join("../up.html").unwrap().as_string(), "https://a.example/up.html");
        assert_eq!(base.join("./same.html").unwrap().as_string(), "https://a.example/dir/same.html");

        // From a directory URL, the directory is the whole path.
        let dir = p("https://a.example/dir/");
        assert_eq!(dir.join("next.html").unwrap().as_string(), "https://a.example/dir/next.html");
    }

    #[test]
    fn a_protocol_relative_link_inherits_the_scheme() {
        assert_eq!(
            p("https://a.example/x").join("//b.example/y").unwrap().as_string(),
            "https://b.example/y"
        );
        assert_eq!(
            p("http://a.example/x").join("//b.example/y").unwrap().as_string(),
            "http://b.example/y"
        );
    }

    #[test]
    fn fragment_and_query_only_links_stay_on_the_page() {
        let base = p("https://a.example/dir/page.html?a=1");
        assert_eq!(base.join("#section").unwrap().as_string(), "https://a.example/dir/page.html?a=1");
        assert_eq!(base.join("?b=2").unwrap().as_string(), "https://a.example/dir/page.html?b=2");
    }

    /// `..` must not climb above the root — RFC 3986 says so, and it is also
    /// what keeps a crafted link inside the site a crawl is confined to.
    #[test]
    fn dot_dot_cannot_escape_the_root() {
        let base = p("https://a.example/");
        assert_eq!(base.join("../../../etc/passwd").unwrap().as_string(), "https://a.example/etc/passwd");
        assert_eq!(p("https://a.example/../../x").as_string(), "https://a.example/x");
    }

    #[test]
    fn links_a_crawler_should_not_follow_are_refused() {
        let base = p("https://a.example/x");
        for bad in ["mailto:a@b.example", "javascript:void(0)", "tel:+15551234", ""] {
            assert!(base.join(bad).is_err(), "{bad:?} was followed");
        }
    }

    #[test]
    fn same_host_compares_hosts_only() {
        assert!(p("https://a.example/one").same_host(&p("https://a.example/two")));
        assert!(p("https://a.example/one").same_host(&p("http://A.EXAMPLE/three")));
        assert!(!p("https://a.example/one").same_host(&p("https://b.example/one")));
    }

    /// Resolving a link and re-parsing its string must give the same URL, or
    /// the deduplication key is unstable.
    #[test]
    fn the_string_form_round_trips() {
        let base = p("https://a.example/dir/page.html?x=1");
        for link in ["next.html", "/root", "../up", "?q=2", "https://b.example/z", "#frag"] {
            let resolved = base.join(link).unwrap();
            let reparsed = p(&resolved.as_string());
            assert_eq!(resolved, reparsed, "{link:?} did not round trip");
        }
    }
}
