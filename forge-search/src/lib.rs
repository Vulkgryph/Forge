//! A small search engine: fetch, extract, index, rank.
//!
//! Built in stages, each usable on its own:
//!
//! - [`crawl`] — what to fetch next, and when to stop. Every hard part of a
//!   crawler is a limit.
//! - [`document`] — readable text out of a file on disk, which is the corpus
//!   most people actually have. The crawler's counterpart: nothing about an
//!   inverted index cares whether the words arrived over a socket.
//! - [`epmc`] — Europe PMC, for the literature a crawler is forbidden to
//!   reach. Indexes only what an article's licence permits keeping.
//! - [`fetch`] — what the engine needs from the network, as a trait it does
//!   not implement. TLS is not something this crate should own.
//! - [`html`] — a page's title, readable text and links. No DOM: a search
//!   engine needs the words and the hyperlinks, never the tree.
//! - [`tokenize`] — text into terms. Decides what can be found at all.
//! - [`index`] — the inverted index: which documents hold a term, and where.
//! - [`library`] — what has been read, grouped by site. The honest face of
//!   the whole crate: it cannot find a site nobody pointed it at, so what it
//!   offers a person is a view of the shelves it does have.
//! - [`jats`] — a journal article, where the title is not in the `<title>`
//!   element and the bibliography is not part of the paper.
//! - [`query`] — the operators a person typed, and the passage of each
//!   document worth showing them back.
//! - [`rank`] — which matching document to show first.
//! - [`robots`] — what a site has asked crawlers not to fetch.
//! - [`url`] — parsing, link resolution, and reducing two spellings of one
//!   address to the same string, without which a crawl multiplies.
//! - [`xml`] — just enough XML for the documents an API returns, where the
//!   structure is the point rather than something to flatten away.
//!
//! Deliberately free of dependencies, which is the crate's purpose rather
//! than a side effect. The engine should be liftable whole into something
//! that is not Forge — including somewhere without cargo — so every stage is
//! written out.

pub mod crawl;
pub mod document;
pub mod epmc;
pub mod fetch;
pub mod html;
pub mod index;
pub mod jats;
pub mod library;
pub mod query;
pub mod rank;
pub mod robots;
pub mod tokenize;
pub mod url;
pub mod xml;
