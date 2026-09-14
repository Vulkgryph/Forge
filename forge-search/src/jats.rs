//! A journal article in JATS, reduced to the parts an index needs.
//!
//! JATS is the XML that PubMed Central, Europe PMC and most publishers use for
//! full text. Reading it needs three decisions that a generic tag-stripper
//! gets wrong, and each of them was observed on a real article before being
//! written down here.
//!
//! **The title is `<article-title>`, not `<title>`.** In JATS, `<title>` marks
//! every section heading. Running the HTML extractor over a real paper
//! produced the title `"ABSTRACTAimsMethodsResultsConclusionIntroduction
//! Materials and Methods…"` — every heading in the paper, concatenated —
//! because it took `<title>` to mean what it means in HTML.
//!
//! **The reference list is not body text.** A bibliography is hundreds of
//! titles by other people. Indexed as part of the article, a search for
//! "temperature" matches every paper that merely *cites* one with the word in
//! its title, which is a precision problem with no upside: the citation is
//! evidence about a different document. `<back>`, where JATS keeps
//! `<ref-list>`, is left out — on the article measured, that is the difference
//! between 63,081 characters of text and 46,882 characters of article.
//!
//! **The abstract is worth indexing twice.** It goes into the searchable text
//! *and* into the description, because a paper's abstract is both its densest
//! statement of what it found and the best thing to show a reader when the
//! query matches nothing quotable. The index does not tokenise descriptions,
//! so an abstract left only there would be readable and unfindable.

use crate::xml;

/// The readable parts of one article.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Article {
    pub title: String,
    /// The abstract, as prose. Empty for an article that has none.
    pub summary: String,
    /// The article body: everything except the front matter and the
    /// references.
    pub body: String,
}

impl Article {
    /// The text to index: the abstract followed by the body.
    ///
    /// See the note above on why the abstract appears here as well as in
    /// [`summary`](Self::summary).
    pub fn indexable(&self) -> String {
        if self.summary.is_empty() {
            return self.body.clone();
        }
        if self.body.is_empty() {
            return self.summary.clone();
        }
        format!("{}\n\n{}", self.summary, self.body)
    }

    /// Whether anything was recovered at all.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty() && self.summary.is_empty() && self.body.is_empty()
    }
}

/// Read an article. Never fails: a document that is not JATS, or is truncated,
/// yields whatever parts were recognisable and empty strings for the rest.
pub fn parse(source: &str) -> Article {
    // Front matter, where the title and abstract live. An article without a
    // `<front>` is not one this was written for, but the whole document is
    // then searched for the same elements rather than giving up — a fragment
    // of JATS is still worth reading.
    let front = xml::elements(source, "front").first().copied();
    let scope = front.unwrap_or(source);

    let title = xml::elements(scope, "title-group")
        .first()
        .and_then(|group| xml::child_text(group, "article-title"))
        // Some articles put `<article-title>` straight in the citation block
        // with no `<title-group>` around it.
        .or_else(|| xml::elements(scope, "article-title").first().map(|e| xml::text(e)))
        .unwrap_or_default();

    // Every abstract, since a structured article may carry more than one — a
    // plain-language summary beside the scientific one, for instance.
    let summary = xml::elements(scope, "abstract")
        .iter()
        .map(|a| xml::text(a))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let body = xml::elements(source, "body")
        .first()
        .map(|b| xml::text(b))
        .unwrap_or_default();

    Article { title, summary, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn article() -> &'static str {
        r#"<article>
             <front>
               <article-meta>
                 <title-group><article-title>Firing at 32 degrees</article-title></title-group>
                 <abstract><p>We recorded from neurons.</p></abstract>
               </article-meta>
             </front>
             <body>
               <sec><title>Methods</title><p>Slices were held at 32 degrees.</p></sec>
             </body>
             <back>
               <ref-list>
                 <ref><element-citation><article-title>Gardening in cold weather</article-title></element-citation></ref>
               </ref-list>
             </back>
           </article>"#
    }

    /// The mistake that motivated the module: `<title>` in JATS is a section
    /// heading, so an HTML-shaped reader returns every heading in the paper
    /// joined together instead of the title.
    #[test]
    fn the_title_is_the_article_title_not_a_section_heading() {
        let a = parse(article());
        assert_eq!(a.title, "Firing at 32 degrees");
        assert!(!a.title.contains("Methods"), "a section heading leaked into the title: {:?}", a.title);
    }

    #[test]
    fn the_abstract_and_body_are_both_read() {
        let a = parse(article());
        assert_eq!(a.summary, "We recorded from neurons.");
        assert!(a.body.contains("Slices were held at 32 degrees"));
        assert!(a.body.contains("Methods"), "section headings belong in the body text");
    }

    /// A bibliography is evidence about other documents. Indexing it makes
    /// every paper that cites a relevant one look relevant itself.
    #[test]
    fn the_reference_list_is_not_part_of_the_article() {
        let a = parse(article());
        let all = a.indexable();
        assert!(
            !all.contains("Gardening"),
            "the reference list was indexed as article text: {all:?}"
        );
    }

    /// The abstract has to be in the indexable text, not only in the summary:
    /// the index does not tokenise descriptions, so an abstract kept only
    /// there would be readable and unfindable.
    #[test]
    fn the_abstract_is_searchable_as_well_as_shown() {
        let a = parse(article());
        assert!(a.indexable().contains("We recorded from neurons"));
        assert_eq!(a.summary, "We recorded from neurons.");
    }

    #[test]
    fn a_missing_abstract_is_not_an_error() {
        let a = parse("<article><front><title-group><article-title>T</article-title></title-group></front><body><p>x</p></body></article>");
        assert_eq!(a.title, "T");
        assert!(a.summary.is_empty());
        assert_eq!(a.indexable(), "x");
    }

    #[test]
    fn an_article_title_without_a_title_group_is_still_found() {
        let a = parse("<article><front><article-title>Direct</article-title></front></article>");
        assert_eq!(a.title, "Direct");
    }

    /// Not JATS, or truncated mid-document: the parts that were recognisable
    /// come back and the rest is empty, rather than a panic.
    #[test]
    fn unrecognisable_input_yields_an_empty_article() {
        assert!(parse("").is_empty());
        assert!(parse("<html><body><p>Not a paper</p></body></html>").title.is_empty());
        // A truncated document still gives up its front matter.
        let cut = "<article><front><title-group><article-title>Half</article-title></title-group></front><body><p>begin";
        assert_eq!(parse(cut).title, "Half");
    }

    /// Inline markup inside a title is markup, not words.
    #[test]
    fn inline_markup_in_a_title_is_removed() {
        let a = parse(
            "<article><front><title-group><article-title>Na<sup>+</sup> channels in <italic>vitro</italic></article-title></title-group></front></article>",
        );
        assert_eq!(a.title, "Na + channels in vitro");
    }

    /// Nested sections must not truncate the body at the first `</sec>`.
    #[test]
    fn nested_sections_are_read_whole() {
        let a = parse(
            "<article><body><sec><title>Outer</title><p>one</p>\
             <sec><title>Inner</title><p>two</p></sec><p>three</p></sec></body></article>",
        );
        for word in ["one", "two", "three"] {
            assert!(a.body.contains(word), "{word} missing from {:?}", a.body);
        }
    }
}
