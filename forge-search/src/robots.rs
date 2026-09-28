//! `robots.txt`: what a site has asked crawlers not to fetch.
//!
//! There is no law here and no enforcement. A crawler that ignores this file
//! works perfectly well, right up to the point where it is blocked, or it
//! costs someone money, or it indexes something that was excluded for a reason
//! the crawler could not see. Obeying it is the difference between a crawler
//! and a nuisance, and it costs one request per host.
//!
//! The format has no specification, only thirty years of convention and one
//! recent RFC (9309) describing what everyone already did. The rules below are
//! the conventions that matter, and each is a decision about being conservative
//! when the file is ambiguous — because the site is the one making the request
//! and the crawler is the one that should yield.

use crate::url::Url;

/// One host's rules, as they apply to one crawler.
#[derive(Clone, Debug, Default)]
pub struct Robots {
    /// Path prefixes that may not be fetched.
    disallowed: Vec<String>,
    /// Prefixes explicitly permitted, which override a longer `Disallow`.
    allowed: Vec<String>,
    /// Seconds the site asked crawlers to wait between requests.
    pub crawl_delay: Option<f64>,
    /// Sitemaps the file advertised — a better place to start a crawl than
    /// following links from the front page, since it is the site's own list of
    /// what it considers worth indexing.
    pub sitemaps: Vec<String>,
}

impl Robots {
    /// A host with no `robots.txt`, or one that could not be fetched.
    ///
    /// Permissive. The convention is that an absent file means no restrictions
    /// — and the alternative, refusing to crawl a host whose file 404s, would
    /// exclude most of the web for the sake of a file that was never written.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// A host that may not be crawled at all.
    ///
    /// Used when `robots.txt` exists but could not be read — a 500, or a
    /// timeout. The site said something and it was not understood, which is
    /// different from the site saying nothing, and the conservative reading of
    /// "I could not hear you" is to wait.
    pub fn deny_all() -> Self {
        Self {
            disallowed: vec!["/".to_string()],
            ..Self::default()
        }
    }

    /// Parse `robots.txt` for the crawler calling itself `user_agent`.
    ///
    /// Groups are matched by the most specific applicable `User-agent`: an
    /// exact name beats `*`, which is the whole point of naming yourself.
    pub fn parse(text: &str, user_agent: &str) -> Self {
        // RFC 9309 matches the *product token* — the name before any `/` —
        // not the whole User-Agent string. `starts_with` on the full string
        // meant a group written for `forge` also captured `forge-search`,
        // and a site aiming a rule at some other crawler whose name happens
        // to be a prefix of ours would have caught us too.
        let wanted = product_token(user_agent);

        // Two passes rather than one: a file may put the `*` group before or
        // after the specific one, and the specific group wins regardless of
        // order. Deciding as the file is read would make the result depend on
        // the order in which the site happened to write it.
        let (specific, matched) = Self::parse_group(text, |agent| agent == wanted && agent != "*");

        // Matched, not "matched and had rules". The difference is the standard
        // way a site lets one crawler in:
        //
        //     User-agent: *
        //     Disallow: /
        //     User-agent: forge-search
        //     Disallow:
        //
        // An empty `Disallow:` permits everything, so that group has no rules
        // to speak of — and falling through to the wildcard on that basis
        // turned "everyone out except you" into the blanket ban it was
        // written to make an exception to. Which is exactly the file a site
        // writes after someone emails to ask for access.
        if matched {
            return specific;
        }
        Self::parse_group(text, |agent| agent == "*").0
    }

    /// Returns the group's rules and whether any `User-agent` line matched at
    /// all — which is not the same question as whether the group had rules.
    fn parse_group(text: &str, matches: impl Fn(&str) -> bool) -> (Self, bool) {
        let mut out = Self::default();
        let mut matched_any = false;
        // Consecutive `User-agent` lines share one group of rules, so the flag
        // stays set across them and is only cleared by a rule line.
        let mut in_group = false;
        let mut naming_agents = false;

        for line in text.lines() {
            // A `#` starts a comment anywhere on the line.
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();

            match key.as_str() {
                "user-agent" => {
                    let agent = value.to_ascii_lowercase();
                    if !naming_agents {
                        // A new group begins.
                        in_group = false;
                    }
                    naming_agents = true;
                    if matches(&agent) {
                        in_group = true;
                        matched_any = true;
                    }
                }
                // Sitemaps are global rather than per-group, so they are
                // collected whichever group is being read.
                "sitemap" => {
                    if !value.is_empty() {
                        out.sitemaps.push(value.to_string());
                    }
                }
                "disallow" => {
                    naming_agents = false;
                    if in_group {
                        // An empty `Disallow:` means "nothing is disallowed",
                        // which is the documented way to permit everything.
                        // Treating it as a prefix would match every path and
                        // block the site.
                        if !value.is_empty() {
                            out.disallowed.push(normalise_rule(value));
                        }
                    }
                }
                "allow" => {
                    naming_agents = false;
                    if in_group && !value.is_empty() {
                        out.allowed.push(normalise_rule(value));
                    }
                }
                "crawl-delay" => {
                    naming_agents = false;
                    if in_group {
                        if let Ok(secs) = value.parse::<f64>() {
                            // Recorded as written. A large value used to be
                            // clamped to 300s and called malformed, so a site
                            // asking for one visit a day got one every five
                            // minutes — 288 times what it asked for, while
                            // the code described itself as obeying. A number
                            // a site meant is not malformed because it is
                            // inconvenient. Negative and non-finite are still
                            // rejected, because those are not numbers a site
                            // can have meant.
                            if secs.is_finite() && secs >= 0.0 {
                                out.crawl_delay = Some(secs);
                            }
                        }
                    }
                }
                _ => {
                    naming_agents = false;
                }
            }
        }
        (out, matched_any)
    }

    /// Whether `url`'s path may be fetched.
    pub fn allows(&self, url: &Url) -> bool {
        let path = if url.query.is_empty() {
            url.path.clone()
        } else {
            format!("{}?{}", url.path, url.query)
        };
        self.allows_path(&path)
    }

    /// The rule-matching itself, on a path string.
    ///
    /// The longest matching rule wins, and `Allow` wins a tie. That is the
    /// convention every major crawler follows, and it is what makes the common
    /// "block a directory, permit one file inside it" pattern work — the
    /// alternative, first-match-wins, would make that pattern depend on line
    /// order.
    pub fn allows_path(&self, path: &str) -> bool {
        let longest = |rules: &[String]| -> Option<usize> {
            rules
                .iter()
                .filter(|r| rule_matches(r, path))
                .map(|r| r.len())
                .max()
        };
        match (longest(&self.disallowed), longest(&self.allowed)) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(deny), Some(allow)) => allow >= deny,
        }
    }
}

/// The product token from a User-Agent string: the name before any `/`.
///
/// RFC 9309 matches `robots.txt` groups against this, not against the whole
/// header. `forge-search/0.5.2 (+https://…)` has the token `forge-search`.
fn product_token(user_agent: &str) -> String {
    user_agent
        .split('/')
        .next()
        .unwrap_or(user_agent)
        .trim()
        .to_ascii_lowercase()
}

/// Trim a rule to its comparable form.
fn normalise_rule(rule: &str) -> String {
    // A rule not beginning with `/` or `*` is malformed; treating it as
    // rooted is what crawlers do, and is the conservative reading.
    if rule.starts_with('/') || rule.starts_with('*') {
        rule.to_string()
    } else {
        format!("/{rule}")
    }
}

/// Whether `rule` matches `path`, with the two wildcards the format has.
///
/// `*` matches any run of characters and `$` anchors the end. Neither is in the
/// original convention, but both are so widely used that a parser without them
/// misreads real files — and misreading a `Disallow` means fetching something
/// the site asked you not to.
fn rule_matches(rule: &str, path: &str) -> bool {
    let (pattern, anchored) = match rule.strip_suffix('$') {
        Some(p) => (p, true),
        None => (rule, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();

    // No wildcard: a plain prefix, or an exact match when anchored.
    if parts.len() == 1 {
        return if anchored { path == pattern } else { path.starts_with(pattern) };
    }

    // The first part must be a prefix, then each subsequent part must appear
    // in order, and an anchored rule's last part must reach the end.
    let Some(mut at) = path.strip_prefix(parts[0]).map(|_| parts[0].len()) else {
        return false;
    };
    for (i, part) in parts.iter().enumerate().skip(1) {
        if part.is_empty() {
            continue;
        }
        let last = i == parts.len() - 1;
        if last && anchored {
            return path[at..].ends_with(part);
        }
        match path[at..].find(part) {
            Some(found) => at += found + part.len(),
            None => return false,
        }
    }
    !anchored || at == path.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn an_absent_file_allows_everything() {
        let r = Robots::allow_all();
        assert!(r.allows(&u("https://a.example/anything")));
        assert!(r.allows(&u("https://a.example/admin/secret")));
    }

    /// A file that exists but could not be read is different from no file:
    /// the site said something and it was not understood.
    #[test]
    fn an_unreadable_file_denies_everything() {
        let r = Robots::deny_all();
        assert!(!r.allows(&u("https://a.example/")));
        assert!(!r.allows(&u("https://a.example/anything")));
    }

    #[test]
    fn a_disallowed_prefix_is_refused_and_the_rest_allowed() {
        let r = Robots::parse("User-agent: *\nDisallow: /private/\n", "forge-search");
        assert!(!r.allows(&u("https://a.example/private/x")));
        assert!(r.allows(&u("https://a.example/public/x")));
        assert!(r.allows(&u("https://a.example/")));
    }

    /// The documented way to permit everything, and a parser that treats it as
    /// a prefix blocks the whole site instead.
    #[test]
    fn an_empty_disallow_permits_everything() {
        let r = Robots::parse("User-agent: *\nDisallow:\n", "forge-search");
        assert!(r.allows(&u("https://a.example/anything")));
    }

    #[test]
    fn disallow_slash_blocks_the_site() {
        let r = Robots::parse("User-agent: *\nDisallow: /\n", "forge-search");
        assert!(!r.allows(&u("https://a.example/")));
        assert!(!r.allows(&u("https://a.example/x")));
    }

    /// The common "block a directory, permit one thing inside it" pattern.
    /// First-match-wins would make this depend on line order.
    #[test]
    fn the_longest_rule_wins() {
        let r = Robots::parse(
            "User-agent: *\nDisallow: /docs/\nAllow: /docs/public/\n",
            "forge-search",
        );
        assert!(!r.allows(&u("https://a.example/docs/private")));
        assert!(r.allows(&u("https://a.example/docs/public/page")));
    }

    /// And it must not depend on the order the site wrote them in.
    #[test]
    fn rule_order_does_not_matter() {
        let a = Robots::parse("User-agent: *\nDisallow: /d/\nAllow: /d/ok/\n", "forge-search");
        let b = Robots::parse("User-agent: *\nAllow: /d/ok/\nDisallow: /d/\n", "forge-search");
        for path in ["/d/ok/x", "/d/no/x", "/other"] {
            assert_eq!(a.allows_path(path), b.allows_path(path), "{path} differed by order");
        }
    }

    /// Naming yourself is pointless if the specific group does not win.
    #[test]
    fn a_named_group_beats_the_wildcard() {
        let text = "User-agent: *\nDisallow: /\n\nUser-agent: forge-search\nDisallow: /private/\n";
        let r = Robots::parse(text, "forge-search");
        assert!(r.allows(&u("https://a.example/public")), "the wildcard group was applied");
        assert!(!r.allows(&u("https://a.example/private/x")));

        // And another crawler gets the restrictive group.
        let other = Robots::parse(text, "someone-else");
        assert!(!other.allows(&u("https://a.example/public")));
    }

    /// Including when the site writes the groups the other way round.
    #[test]
    fn a_named_group_wins_whichever_order_it_appears_in() {
        let text = "User-agent: forge-search\nDisallow: /private/\n\nUser-agent: *\nDisallow: /\n";
        let r = Robots::parse(text, "forge-search");
        assert!(r.allows(&u("https://a.example/public")));
    }

    /// Consecutive `User-agent` lines share one group of rules.
    #[test]
    fn consecutive_agent_lines_share_a_group() {
        let text = "User-agent: alpha\nUser-agent: forge-search\nDisallow: /x/\n";
        let r = Robots::parse(text, "forge-search");
        assert!(!r.allows(&u("https://a.example/x/y")));
        assert!(r.allows(&u("https://a.example/y")));
    }

    /// Both wildcards, because real files use them and misreading a `Disallow`
    /// means fetching what a site asked you not to.
    #[test]
    fn wildcards_are_honoured() {
        let r = Robots::parse(
            "User-agent: *\nDisallow: /*.pdf$\nDisallow: /tmp/*/cache\n",
            "forge-search",
        );
        assert!(!r.allows(&u("https://a.example/manual.pdf")));
        assert!(!r.allows(&u("https://a.example/deep/path/manual.pdf")));
        // `$` anchors: a path that merely contains `.pdf` is fine.
        assert!(r.allows(&u("https://a.example/manual.pdf.html")));
        assert!(!r.allows(&u("https://a.example/tmp/anything/cache")));
        assert!(r.allows(&u("https://a.example/tmp/anything/else")));
    }

    #[test]
    fn a_crawl_delay_is_recorded_as_the_site_wrote_it() {
        let r = Robots::parse("User-agent: *\nCrawl-delay: 2.5\n", "forge-search");
        assert_eq!(r.crawl_delay, Some(2.5));

        // Not a number the site can have meant.
        assert_eq!(Robots::parse("User-agent: *\nCrawl-delay: -5\n", "x").crawl_delay, None);
        assert_eq!(Robots::parse("User-agent: *\nCrawl-delay: abc\n", "x").crawl_delay, None);

        // A large one is. This used to be clamped to 300 and the file called
        // malformed, so a site asking for one visit a day got one every five
        // minutes — 288 times what it asked for, from code describing itself
        // as obeying. What can actually be honoured is the crawler's
        // decision, and it refuses rather than quietly going faster.
        assert_eq!(
            Robots::parse("User-agent: *\nCrawl-delay: 86400\n", "x").crawl_delay,
            Some(86400.0),
        );
    }

    /// The standard way a site lets one crawler past a blanket ban.
    #[test]
    fn an_empty_disallow_in_our_own_group_is_permission_not_silence() {
        let text = "User-agent: *\nDisallow: /\n\nUser-agent: forge-search\nDisallow:\n";
        let r = Robots::parse(text, "forge-search/0.5.2 (+https://vulkgryph.com/)");
        assert!(
            r.allows(&u("https://a.example/anything")),
            "the exception written for us was read as the ban it excepts"
        );

        // And a crawler the exception was not written for still gets the ban.
        let other = Robots::parse(text, "some-other-bot/1.0");
        assert!(!other.allows(&u("https://a.example/anything")));
    }

    /// RFC 9309 matches the product token, not the whole header.
    #[test]
    fn a_group_named_for_another_crawler_does_not_capture_this_one() {
        // `forge` is a prefix of `forge-search`, and prefix matching on the
        // full User-Agent string meant this group caught us.
        let text = "User-agent: forge\nDisallow: /\n";
        let r = Robots::parse(text, "forge-search/0.5.2 (+https://vulkgryph.com/)");
        assert!(
            r.allows(&u("https://a.example/page")),
            "a rule aimed at a differently-named crawler was applied to this one"
        );

        // Our own name still matches, with or without the version suffix.
        for ua in ["forge-search", "forge-search/0.5.2 (+https://vulkgryph.com/)"] {
            let mine = Robots::parse("User-agent: forge-search\nDisallow: /\n", ua);
            assert!(!mine.allows(&u("https://a.example/page")), "{ua}");
        }
    }

    /// A site's own list of what it considers worth indexing is a better place
    /// to start than its front page.
    #[test]
    fn sitemaps_are_collected() {
        let r = Robots::parse(
            "Sitemap: https://a.example/sitemap.xml\nUser-agent: *\nDisallow: /x/\n\
             Sitemap: https://a.example/news.xml\n",
            "forge-search",
        );
        assert_eq!(
            r.sitemaps,
            vec!["https://a.example/sitemap.xml", "https://a.example/news.xml"]
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let r = Robots::parse(
            "# a comment\n\nUser-agent: *   # trailing comment\nDisallow: /x/ # here too\n\n",
            "forge-search",
        );
        assert!(!r.allows(&u("https://a.example/x/y")));
        assert!(r.allows(&u("https://a.example/y")));
    }

    #[test]
    fn keys_are_case_insensitive() {
        let r = Robots::parse("USER-AGENT: *\nDISALLOW: /x/\n", "forge-search");
        assert!(!r.allows(&u("https://a.example/x/y")));
    }

    /// The query string is part of what a rule matches against, since a rule
    /// may target a parameter.
    #[test]
    fn a_rule_can_match_the_query_string() {
        let r = Robots::parse("User-agent: *\nDisallow: /*?sort=\n", "forge-search");
        assert!(!r.allows(&u("https://a.example/list?sort=price")));
        assert!(r.allows(&u("https://a.example/list")));
    }

    /// Real files are malformed in these ways, and a parser that panics or
    /// blocks the site over one is worse than a permissive one.
    #[test]
    fn malformed_files_do_not_panic_or_block_everything() {
        for text in [
            "",
            "garbage",
            "Disallow: /x/",            // rules with no group
            "User-agent:",              // no name
            "User-agent: *\nDisallow",  // no colon
            ":::::",
            "User-agent: *\nDisallow: x/no/leading/slash",
            &"User-agent: *\n".repeat(1000),
        ] {
            let r = Robots::parse(text, "forge-search");
            // Whatever it decided, it must answer without panicking.
            let _ = r.allows(&u("https://a.example/some/path"));
        }
        // Rules outside any group belong to nobody, so the site stays open.
        assert!(Robots::parse("Disallow: /", "x").allows(&u("https://a.example/y")));
    }
}
