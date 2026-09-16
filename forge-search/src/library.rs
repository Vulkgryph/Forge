// SPDX-License-Identifier: Apache-2.0
//! What the index has read, described so a person can look at it.
//!
//! This exists because of what the thing actually is. It is not a search
//! engine: it cannot be asked "microsoft" and produce Microsoft, because it
//! has no way to discover a site it was never pointed at. What it is, is a
//! crawler that keeps what it read — and the useful form of that is a library
//! you can see the shelves of.
//!
//! The distinction is not cosmetic. A search box over a corpus of whatever
//! happened to get crawled last week sets an expectation the corpus cannot
//! meet, and a person who types a question and gets nothing concludes the
//! feature is broken. The same corpus, presented as "these are the eleven
//! sites Forge has read, and here is when", is honest and immediately useful:
//! you can see whether the answer could possibly be in there before you ask.
//!
//! So the unit here is the host. A host is the closest thing to a corpus that
//! needs no naming and no decision from anybody — you read `doc.rust-lang.org`
//! or you have not — and it is the grouping a person already thinks in.

use crate::index::{DocId, Index};

/// One site the index holds pages from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shelf {
    /// The host, as it appears in the pages' URLs.
    pub host: String,
    /// How many live pages are held from it.
    pub pages: usize,
    /// Roughly how much text, in bytes. What was kept for snippets rather
    /// than what the pages originally weighed, since that is what is there.
    pub text_bytes: u64,
    /// When the most recent of those pages was read, as Unix seconds. Zero if
    /// the index could not say — an in-memory index that was never saved.
    pub last_read: u64,
    /// When the oldest was read. The pair is what makes staleness legible: a
    /// shelf read once two months ago is a different thing from one that has
    /// been topped up since.
    pub first_read: u64,
    /// A page from the host, so a reader has somewhere to click.
    pub example: String,
}

/// Every site the index holds, most pages first.
///
/// Sorted by size rather than alphabetically because the question a person
/// opens this to answer is "what does Forge actually know about", and the
/// answer is the top of the list. Ties broken by host so the order is stable
/// between calls — a list that reshuffles on every redraw is a list nobody
/// trusts.
pub fn shelves(index: &Index) -> Vec<Shelf> {
    let mut by_host: std::collections::HashMap<String, Shelf> = std::collections::HashMap::new();

    for id in 0..index.len_including_dead() as DocId {
        let Some(doc) = index.document(id) else { continue };
        // The same spelling a scoped query uses, so a shelf a person can see
        // is a shelf they can search. Two spellings of one host would make the
        // library list a site that narrowing to it then found nothing from.
        let host = crate::query::normalise_host(&doc.url).unwrap_or_else(|| doc.url.clone());
        let read = index.read_time(id);
        let shelf = by_host.entry(host.clone()).or_insert_with(|| Shelf {
            host,
            pages: 0,
            text_bytes: 0,
            last_read: 0,
            first_read: u64::MAX,
            example: doc.url.clone(),
        });
        shelf.pages += 1;
        shelf.text_bytes += doc.text_len() as u64;
        shelf.last_read = shelf.last_read.max(read);
        shelf.first_read = shelf.first_read.min(read);
        // The shallowest URL, which on most sites is the front page or a
        // section index — a better place to send somebody than page 47 of a
        // forum thread.
        if depth_of(&doc.url) < depth_of(&shelf.example) {
            shelf.example = doc.url.clone();
        }
    }

    let mut out: Vec<Shelf> = by_host.into_values().collect();
    for shelf in &mut out {
        if shelf.first_read == u64::MAX {
            shelf.first_read = 0;
        }
    }
    out.sort_by(|a, b| b.pages.cmp(&a.pages).then_with(|| a.host.cmp(&b.host)));
    out
}

/// How many path segments a URL has, for picking the shallowest.
fn depth_of(url: &str) -> usize {
    crate::url::Url::parse(url)
        .map(|u| u.path.split('/').filter(|s| !s.is_empty()).count())
        .unwrap_or(usize::MAX)
}

/// Whether the index holds anything from `host`.
///
/// The question `web_search` asks before deciding to crawl: naming a site is
/// an instruction to read it, and an instruction already carried out should be
/// answered from the index rather than fetched again.
pub fn holds_host(index: &Index, host: &str) -> bool {
    shelves(index).iter().any(|s| s.host == host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_pages(pages: &[(&str, &str)]) -> Index {
        let mut ix = Index::new();
        for (url, body) in pages {
            ix.add(url, "Title", "", body);
        }
        ix
    }

    #[test]
    fn pages_are_grouped_by_host_largest_first() {
        let ix = with_pages(&[
            ("https://docs.test/a", "alpha"),
            ("https://docs.test/b", "beta"),
            ("https://docs.test/c", "gamma"),
            ("https://forum.test/t/1", "delta"),
            ("https://forum.test/t/2", "epsilon"),
            ("https://single.test/", "zeta"),
        ]);
        let shelves = shelves(&ix);
        assert_eq!(
            shelves.iter().map(|s| (s.host.as_str(), s.pages)).collect::<Vec<_>>(),
            vec![("docs.test", 3), ("forum.test", 2), ("single.test", 1)]
        );
    }

    /// The link offered for a shelf should be somewhere worth landing, not
    /// whichever page happened to be crawled first.
    #[test]
    fn the_example_page_is_the_shallowest_one() {
        let ix = with_pages(&[
            ("https://docs.test/guide/ch/3/deep/page", "alpha"),
            ("https://docs.test/guide/", "beta"),
            ("https://docs.test/guide/ch/1", "gamma"),
        ]);
        assert_eq!(shelves(&ix)[0].example, "https://docs.test/guide/");
    }

    /// A removed page leaves its shelf, and a shelf with nothing left leaves
    /// the library. Otherwise the view reports pages that no longer answer.
    #[test]
    fn removed_pages_do_not_appear() {
        let mut ix = with_pages(&[
            ("https://docs.test/a", "alpha"),
            ("https://docs.test/b", "beta"),
            ("https://gone.test/x", "gamma"),
        ]);
        let id = ix.by_url_id("https://gone.test/x").unwrap();
        assert!(ix.remove(id));
        let id = ix.by_url_id("https://docs.test/b").unwrap();
        assert!(ix.remove(id));

        let shelves = shelves(&ix);
        assert_eq!(shelves.len(), 1, "an emptied shelf is still listed: {shelves:?}");
        assert_eq!(shelves[0].pages, 1);
        assert!(!holds_host(&ix, "gone.test"));
        assert!(holds_host(&ix, "docs.test"));
    }

    /// The read time comes from the segment, so it only exists after a save —
    /// and then it has to be a real time, not a zero that renders as 1970.
    #[test]
    fn a_saved_shelf_knows_when_it_was_read() {
        let dir = std::env::temp_dir().join("forge-library-times");
        let _ = std::fs::remove_dir_all(&dir);
        let mut ix = with_pages(&[("https://docs.test/a", "alpha")]);

        // Before a save there is no segment to have a time.
        assert_eq!(shelves(&ix)[0].last_read, 0);

        ix.save(&dir).unwrap();
        let back = Index::load(&dir).unwrap();
        let shelf = &shelves(&back)[0];
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            shelf.last_read > now - 120 && shelf.last_read <= now,
            "read time {} is not near now ({now})",
            shelf.last_read
        );
        assert_eq!(shelf.first_read, shelf.last_read, "one segment, one time");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Pages added in a later crawl are newer than the ones already there, and
    /// the shelf has to show both ends — that span is what makes "this needs
    /// refreshing" a thing a person can see.
    #[test]
    fn a_topped_up_shelf_spans_two_reads() {
        let dir = std::env::temp_dir().join("forge-library-span");
        let _ = std::fs::remove_dir_all(&dir);
        let mut ix = with_pages(&[("https://docs.test/a", "alpha")]);
        ix.save(&dir).unwrap();

        // The clock has a one-second resolution here, so the two saves are
        // forced apart rather than assumed to differ.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        ix.add("https://docs.test/b", "Title", "", "beta");
        ix.save(&dir).unwrap();

        let back = Index::load(&dir).unwrap();
        let shelf = &shelves(&back)[0];
        assert_eq!(shelf.pages, 2);
        assert!(
            shelf.last_read > shelf.first_read,
            "both pages read at {} — the later segment's time was not used",
            shelf.last_read
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A shelf a person can see has to be a shelf they can search, so the two
    /// must agree on how a host is spelled — including the `www.` half the web
    /// uses and half does not.
    #[test]
    fn a_listed_shelf_can_be_searched_by_its_own_name() {
        let ix = with_pages(&[
            ("https://www.forum.test/t/1", "the crankcase holds five quarts"),
            ("https://forum.test/t/2", "the crankcase is drained from below"),
            ("https://docs.test/x", "the crankcase of an engine"),
        ]);
        let shelves = shelves(&ix);
        assert_eq!(shelves[0].host, "forum.test", "www. was not folded away");
        assert_eq!(shelves[0].pages, 2, "one site was listed as two");

        let scoped = crate::query::search_within(&ix, "crankcase", 5, &[shelves[0].host.clone()]);
        assert_eq!(scoped.len(), 2, "the shelf's own name did not match it");
        assert!(scoped.iter().all(|r| !r.url.contains("docs.test")));
    }

    #[test]
    fn an_empty_index_has_an_empty_library() {
        assert!(shelves(&Index::new()).is_empty());
        assert!(!holds_host(&Index::new(), "anything.test"));
    }
}
