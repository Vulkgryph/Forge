//! Europe PMC: searching the biomedical literature through the channel built
//! for it.
//!
//! This exists because the obvious approach is forbidden and the right one is
//! not obvious. PubMed Central's website answers `robots.txt` with
//! `User-agent: * / Disallow: /` — the primary literature cannot be crawled.
//! A crawler that only knows what it crawled therefore cannot answer a
//! question whose answer is in a paper, which is most questions worth asking
//! about a measured value.
//!
//! The channel that is built for this is a REST API on a different host, and
//! using it is not a way round the refusal — it is the front door. The
//! distinction matters enough to write down:
//!
//! - `robots.txt` is a protocol for crawlers (RFC 9309). It governs spidering
//!   a website. Publishers who want programmatic access to happen publish an
//!   API and state its terms there instead.
//! - Europe PMC's *website*, `europepmc.org`, carries
//!   `Content-Signal: search=yes,ai-train=no,use=reference` and names
//!   ClaudeBot, GPTBot, CCBot and others with `Disallow: /`. Building an index
//!   and returning links and short excerpts is expressly permitted; training
//!   is expressly refused; `ai-input` is not mentioned, which by the file's own
//!   terms is neither granted nor restricted.
//! - The API is on `www.ebi.ac.uk`, whose `robots.txt` restricts two named
//!   bots and two paths and is otherwise open.
//!
//! So this module never reads the website. It asks the API, and it keeps only
//! what the article's own licence allows it to keep — which is the part of
//! this that is enforced in code rather than in a comment:
//!
//! **Only open-access articles are indexed.** For anything else the citation
//! comes back and the text does not. That is deliberately more conservative
//! than it has to be: a title and a journal name are facts, and Europe PMC
//! publishes abstracts for far more than it opens. Keeping the rule simple —
//! full text for articles licensed for reuse, a citation and a link for
//! everything else — makes it one sentence to state and one condition to
//! audit, and no part of it depends on a judgement about how much of a
//! copyrighted abstract is a "short excerpt".
//!
//! Every indexed document records its licence in
//! [`Document::attribution`](crate::index::Document::attribution), because the
//! obligation outlives the request that fetched it.

use crate::fetch::Fetcher;
use crate::index::Index;
use crate::{jats, url, xml};

/// The API host. Not `europepmc.org`, which is the website.
pub const HOST: &str = "www.ebi.ac.uk";

const BASE: &str = "https://www.ebi.ac.uk/europepmc/webservices/rest";

/// Europe PMC's own cap on a page of results.
const MAX_PAGE: usize = 1000;

/// One article as Europe PMC describes it.
///
/// Metadata comes from the search response rather than from the article's own
/// XML: the licence, the identifiers and the open-access flag are curated
/// fields, and reading them from one place means the decision about what may
/// be kept is made from one source.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    /// Which database the record came from — `MED`, `PMC`, `PPR` for
    /// preprints.
    pub source: String,
    pub id: String,
    pub pmid: String,
    pub pmcid: String,
    pub doi: String,
    pub title: String,
    /// The abstract, when the record has one.
    pub summary: String,
    pub journal: String,
    pub year: String,
    /// Whether the article is open access. The one field that decides whether
    /// full text may be kept.
    pub open_access: bool,
    /// The licence as Europe PMC states it — `cc by`, `cc by-nc`, `cc0`, or
    /// empty when unstated.
    pub license: String,
}

impl Found {
    /// Whether full text can be fetched *and* kept.
    ///
    /// Both halves are required: a PMCID says the full text exists, and the
    /// open-access flag says it may be indexed.
    pub fn may_index_full_text(&self) -> bool {
        self.open_access && !self.pmcid.is_empty()
    }

    /// Where a reader can open this article.
    ///
    /// Europe PMC rather than the DOI: for an open-access article this is
    /// where the full text is readable, whereas the DOI resolves to the
    /// publisher, which may be a paywall. The DOI is kept in the record for
    /// anyone who needs the durable identifier.
    pub fn article_url(&self) -> String {
        if !self.pmcid.is_empty() {
            return format!("https://europepmc.org/article/PMC/{}", self.pmcid);
        }
        if !self.source.is_empty() && !self.id.is_empty() {
            return format!("https://europepmc.org/article/{}/{}", self.source, self.id);
        }
        if !self.doi.is_empty() {
            return format!("https://doi.org/{}", self.doi);
        }
        String::new()
    }

    /// The terms this article's text is held under, as one line.
    pub fn attribution(&self) -> String {
        let licence = if self.license.is_empty() {
            "licence not stated".to_string()
        } else {
            self.license.clone()
        };
        let mut out = format!("{licence} — Europe PMC");
        if !self.pmcid.is_empty() {
            out.push(' ');
            out.push_str(&self.pmcid);
        }
        if !self.doi.is_empty() {
            out.push_str(&format!(" doi:{}", self.doi));
        }
        out
    }

    /// A one-line citation, for showing an article whose text may not be kept.
    pub fn citation(&self) -> String {
        let mut out = self.title.clone();
        if !self.journal.is_empty() {
            out.push_str(&format!(" — {}", self.journal));
        }
        if !self.year.is_empty() {
            out.push_str(&format!(" ({})", self.year));
        }
        out
    }
}

/// A parsed search response.
#[derive(Clone, Debug, Default)]
pub struct Search {
    /// How many articles match in total, which is almost always far more than
    /// were returned. Reported so a caller can tell "nothing matches" from
    /// "this is the first page of thousands".
    pub hit_count: usize,
    pub results: Vec<Found>,
}

/// The URL for a search.
///
/// `resultType=core` is what carries the abstract, the licence and the
/// open-access flag; the lighter result types omit exactly the fields the
/// indexing decision depends on.
pub fn search_url(query: &str, page_size: usize) -> String {
    let size = page_size.clamp(1, MAX_PAGE);
    format!(
        "{BASE}/search?query={}&format=xml&resultType=core&pageSize={size}",
        url::encode_query(query)
    )
}

/// The URL for an article's full text in JATS.
pub fn full_text_url(pmcid: &str) -> String {
    format!("{BASE}/{}/fullTextXML", url::encode_query(pmcid))
}

/// Read a search response. Never fails; a body that is not a search response
/// yields no results.
pub fn parse_search(body: &str) -> Search {
    let hit_count = xml::elements(body, "responseWrapper")
        .first()
        .and_then(|w| xml::child_text(w, "hitCount"))
        .or_else(|| xml::elements(body, "hitCount").first().map(|e| xml::text(e)))
        .and_then(|t| t.trim().parse().ok())
        .unwrap_or(0);

    let results = xml::elements(body, "result")
        .iter()
        .map(|r| {
            let field = |name: &str| xml::child_text(r, name).unwrap_or_default();
            Found {
                source: field("source"),
                id: field("id"),
                pmid: field("pmid"),
                pmcid: field("pmcid"),
                doi: field("doi"),
                title: field("title"),
                summary: field("abstractText"),
                // `journalInfo > journal > title`, which is why the reader
                // needs to walk children rather than search by name: a
                // descendant search for "title" would find the article's.
                journal: xml::child(r, "journalInfo")
                    .and_then(|ji| xml::child(ji, "journal"))
                    .and_then(|j| xml::child_text(j, "title"))
                    .unwrap_or_default(),
                year: field("pubYear"),
                open_access: field("isOpenAccess").eq_ignore_ascii_case("y"),
                license: field("license"),
            }
        })
        .collect();

    Search { hit_count, results }
}

/// What a run did.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// Total matches Europe PMC reports, not the number examined.
    pub hit_count: usize,
    /// Records returned by the search.
    pub examined: usize,
    /// Of those, how many were open access with full text available.
    pub open_access: usize,
    /// How many articles were indexed in full.
    pub indexed: usize,
    /// How many came back as a citation only, because their licence does not
    /// permit keeping the text. Reported rather than silently dropped: a
    /// caller that does not know coverage was limited will read the results as
    /// complete.
    pub citation_only: usize,
    /// Requests that got no response.
    pub unreachable: usize,
    /// Full texts fetched that turned out to hold nothing readable.
    pub empty: usize,
    /// Every record seen, indexed or not, so a caller can cite what it could
    /// not keep.
    pub articles: Vec<Found>,
}

/// Searches Europe PMC and indexes what may be kept.
pub struct Connector<'a, F: Fetcher, C: crate::crawl::Clock> {
    fetcher: &'a F,
    clock: &'a C,
    /// Seconds between requests to the API.
    politeness: f64,
    next_allowed: f64,
}

impl<'a, F: Fetcher, C: crate::crawl::Clock> Connector<'a, F, C> {
    pub fn new(fetcher: &'a F, clock: &'a C, politeness: f64) -> Self {
        Self { fetcher, clock, politeness, next_allowed: 0.0 }
    }

    /// Search for `query` and index up to `want` open-access articles.
    ///
    /// One search request, then one request per article whose full text may be
    /// kept. Articles are considered in the order Europe PMC ranked them,
    /// which is its relevance order rather than ours — this module's job is to
    /// get the text; ranking it is [`crate::rank`]'s.
    pub fn run(&mut self, query: &str, want: usize, index: &mut Index) -> Report {
        let mut report = Report::default();
        if want == 0 {
            return report;
        }

        // Ask for more records than wanted, because some of them will be
        // closed access and contribute no text.
        let page = (want * 3).clamp(1, MAX_PAGE);
        self.wait();
        let body = match self.fetcher.fetch(&search_url(query, page)) {
            Ok(response) if response.is_ok() => response.body,
            Ok(_) | Err(_) => {
                report.unreachable += 1;
                return report;
            }
        };

        let search = parse_search(&body);
        report.hit_count = search.hit_count;
        report.examined = search.results.len();

        for found in search.results {
            if report.indexed >= want {
                // Not examined further, so not counted as refused either.
                report.articles.push(found);
                continue;
            }
            if !found.may_index_full_text() {
                report.citation_only += 1;
                report.articles.push(found);
                continue;
            }
            report.open_access += 1;

            self.wait();
            let full = match self.fetcher.fetch(&full_text_url(&found.pmcid)) {
                Ok(response) if response.is_ok() => response.body,
                Ok(_) | Err(_) => {
                    report.unreachable += 1;
                    report.articles.push(found);
                    continue;
                }
            };

            let article = jats::parse(&full);
            let text = article.indexable();
            if text.is_empty() {
                report.empty += 1;
                report.articles.push(found);
                continue;
            }
            // The curated title is preferred over the article's own, since it
            // is the one Europe PMC indexes under; the article's is a fallback
            // for a record whose title field is empty.
            let title = if found.title.is_empty() { article.title.clone() } else { found.title.clone() };
            // The abstract from the search record is preferred for the same
            // reason, and is present even when the full text's own front
            // matter is not.
            let summary = if found.summary.is_empty() { article.summary.clone() } else { found.summary.clone() };

            index.add_attributed(
                &found.article_url(),
                &title,
                &summary,
                &text,
                &found.attribution(),
            );
            report.indexed += 1;
            report.articles.push(found);
        }

        report
    }

    /// Hold off until the API may be asked again.
    fn wait(&mut self) {
        if self.next_allowed > 0.0 {
            self.clock.sleep_until(self.next_allowed);
        }
        self.next_allowed = self.clock.now() + self.politeness;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crawl::{Clock, FakeClock};
    use crate::fetch::{Fetched, StaticFetcher};

    fn response(source: &str, open: bool, licence: &str, pmcid: &str) -> String {
        format!(
            r#"<responseWrapper><hitCount>4210</hitCount><resultList><result>
                 <id>1</id><source>{source}</source><pmid>111</pmid><pmcid>{pmcid}</pmcid>
                 <doi>10.1000/x</doi>
                 <title>Firing rate at 32 degrees</title>
                 <abstractText>We measured the rate.</abstractText>
                 <authorList><author><fullName>A B</fullName></author></authorList>
                 <journalInfo><journal><title>Journal of Things</title></journal></journalInfo>
                 <pubYear>2026</pubYear>
                 <isOpenAccess>{}</isOpenAccess><license>{licence}</license>
               </result></resultList></responseWrapper>"#,
            if open { "Y" } else { "N" }
        )
    }

    fn full_text() -> &'static str {
        r#"<article><front><article-meta>
             <title-group><article-title>Firing rate at 32 degrees</article-title></title-group>
             <abstract><p>We measured the rate.</p></abstract>
           </article-meta></front>
           <body><sec><title>Methods</title><p>Slices were held at 32 degrees in ACSF.</p></sec></body>
           <back><ref-list><ref><article-title>Gardening</article-title></ref></ref-list></back>
         </article>"#
    }

    /// Only the API host is contacted. The website carries content signals and
    /// named-bot refusals; the API is the channel published for this.
    #[test]
    fn urls_point_at_the_api_not_the_website() {
        let s = search_url("pyramidal neuron", 10);
        assert!(s.starts_with("https://www.ebi.ac.uk/europepmc/webservices/rest/search?"), "{s}");
        assert!(!s.contains("europepmc.org"), "the website must not be queried: {s}");
        assert!(full_text_url("PMC1").starts_with("https://www.ebi.ac.uk/"), "{}", full_text_url("PMC1"));
    }

    /// A query is one parameter however it is spelled, so its own `&` and `=`
    /// cannot split it into two.
    #[test]
    fn a_query_is_encoded() {
        let s = search_url(r#""pyramidal neuron" AND temperature"#, 5);
        assert!(s.contains("%22pyramidal%20neuron%22%20AND%20temperature"), "{s}");
        let injected = search_url("x&pageSize=999", 5);
        assert!(injected.contains("x%26pageSize%3D999"), "{injected}");
        assert!(injected.ends_with("pageSize=5"), "{injected}");
    }

    /// `resultType=core` carries the licence and the open-access flag, which
    /// are the fields the keep-or-cite decision is made from.
    #[test]
    fn the_search_asks_for_the_fields_the_licence_check_needs() {
        let s = search_url("x", 10);
        assert!(s.contains("resultType=core"), "{s}");
    }

    #[test]
    fn a_page_size_is_clamped_to_what_the_api_allows() {
        assert!(search_url("x", 0).ends_with("pageSize=1"));
        assert!(search_url("x", 10_000).ends_with(&format!("pageSize={MAX_PAGE}")));
    }

    #[test]
    fn a_search_response_is_read() {
        let s = parse_search(&response("MED", true, "cc by", "PMC9"));
        assert_eq!(s.hit_count, 4210);
        assert_eq!(s.results.len(), 1);
        let r = &s.results[0];
        assert_eq!(r.title, "Firing rate at 32 degrees");
        assert_eq!(r.journal, "Journal of Things", "the journal title, not the article's");
        assert_eq!(r.pmcid, "PMC9");
        assert_eq!(r.year, "2026");
        assert!(r.open_access);
        assert_eq!(r.license, "cc by");
        assert!(r.summary.starts_with("We measured"));
    }

    /// The rule the module is built around: text is kept only for articles
    /// licensed for reuse.
    #[test]
    fn only_open_access_full_text_is_indexed() {
        let fetcher = StaticFetcher::new()
            .with_response(
                &search_url("firing rate", 3),
                Fetched {
                    status: 200,
                    final_url: search_url("firing rate", 3),
                    content_type: "application/xml".into(),
                    body: response("MED", false, "", "PMC9"),
                },
            )
            .with_page(&full_text_url("PMC9"), full_text());
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = Connector::new(&fetcher, &clock, 0.0).run("firing rate", 1, &mut index);

        assert_eq!(report.indexed, 0, "a closed article must not be indexed");
        assert_eq!(report.citation_only, 1);
        assert_eq!(index.len(), 0);
        // And it still comes back, so the caller can cite and link it.
        assert_eq!(report.articles.len(), 1);
        assert_eq!(report.articles[0].citation(), "Firing rate at 32 degrees — Journal of Things (2026)");
    }

    #[test]
    fn an_open_access_article_is_indexed_with_its_licence() {
        let fetcher = StaticFetcher::new()
            .with_response(
                &search_url("firing rate", 3),
                Fetched {
                    status: 200,
                    final_url: search_url("firing rate", 3),
                    content_type: "application/xml".into(),
                    body: response("MED", true, "cc by", "PMC9"),
                },
            )
            .with_page(&full_text_url("PMC9"), full_text());
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = Connector::new(&fetcher, &clock, 0.0).run("firing rate", 1, &mut index);

        assert_eq!(report.indexed, 1);
        assert_eq!(report.hit_count, 4210, "the total should be reported, not just what was taken");
        let doc = index.document(0).unwrap();
        assert_eq!(doc.url, "https://europepmc.org/article/PMC/PMC9");
        assert_eq!(doc.attribution, "cc by — Europe PMC PMC9 doi:10.1000/x");
        assert!(doc.text.contains("Slices were held at 32 degrees"), "{:?}", doc.text);
        assert!(doc.text.contains("We measured the rate"), "the abstract should be searchable");
        assert!(!doc.text.contains("Gardening"), "the reference list was indexed");
        // And it is findable by something only the Methods section says.
        let hits = crate::query::search(&index, "slices acsf", 3);
        assert_eq!(hits.len(), 1, "the body text did not make it into the postings");
    }

    /// An article with no licence stated is still recorded as such, rather
    /// than as though no question arises.
    #[test]
    fn a_missing_licence_is_recorded_not_assumed() {
        let found = Found { open_access: true, pmcid: "PMC1".into(), ..Found::default() };
        assert_eq!(found.attribution(), "licence not stated — Europe PMC PMC1");
    }

    /// Full text needs both a PMCID to fetch and a licence to keep.
    #[test]
    fn full_text_needs_both_availability_and_permission() {
        let open_no_id = Found { open_access: true, ..Found::default() };
        let id_not_open = Found { pmcid: "PMC1".into(), ..Found::default() };
        let both = Found { open_access: true, pmcid: "PMC1".into(), ..Found::default() };
        assert!(!open_no_id.may_index_full_text());
        assert!(!id_not_open.may_index_full_text());
        assert!(both.may_index_full_text());
    }

    /// The API being unreachable is reported, not silently an empty result.
    #[test]
    fn an_unreachable_api_is_reported() {
        let fetcher = StaticFetcher::new().with_unreachable(&search_url("x", 3));
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = Connector::new(&fetcher, &clock, 0.0).run("x", 1, &mut index);
        assert_eq!(report.unreachable, 1);
        assert_eq!(report.indexed, 0);
    }

    /// Politeness is one request per interval, counted across the search and
    /// every full text it leads to.
    #[test]
    fn requests_are_spaced() {
        let fetcher = StaticFetcher::new()
            .with_response(
                &search_url("q", 3),
                Fetched {
                    status: 200,
                    final_url: search_url("q", 3),
                    content_type: "application/xml".into(),
                    body: response("MED", true, "cc by", "PMC9"),
                },
            )
            .with_page(&full_text_url("PMC9"), full_text());
        let clock = FakeClock::default();
        let mut index = Index::new();
        Connector::new(&fetcher, &clock, 1.0).run("q", 1, &mut index);
        // Two requests, so one interval must have passed between them.
        assert!(clock.now() >= 1.0, "the clock only reached {}", clock.now());
    }

    #[test]
    fn nothing_wanted_asks_for_nothing() {
        let fetcher = StaticFetcher::new();
        let clock = FakeClock::default();
        let mut index = Index::new();
        let report = Connector::new(&fetcher, &clock, 0.0).run("x", 0, &mut index);
        assert_eq!(report.examined, 0);
        assert_eq!(report.unreachable, 0, "no request should have been made at all");
    }

    #[test]
    fn a_body_that_is_not_a_search_response_yields_nothing() {
        let s = parse_search("<html><body>down for maintenance</body></html>");
        assert_eq!(s.hit_count, 0);
        assert!(s.results.is_empty());
        assert!(parse_search("").results.is_empty());
    }
}
