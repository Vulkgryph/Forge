//! A small search engine: fetch, extract, index, rank.
//!
//! Built in stages, each usable on its own:
//!
//! - [`html`] — a page's title, readable text and links. No DOM: a search
//!   engine needs the words and the hyperlinks, never the tree.
//! - [`tokenize`] — text into terms. Decides what can be found at all.
//! - [`index`] — the inverted index: which documents hold a term, and where.
//! - [`rank`] — which matching document to show first.
//!
//! Deliberately free of dependencies, which is the crate's purpose rather
//! than a side effect. The engine should be liftable whole into something
//! that is not Forge — including somewhere without cargo — so every stage is
//! written out.

pub mod html;
pub mod index;
pub mod rank;
pub mod tokenize;
