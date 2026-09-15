// SPDX-License-Identifier: Apache-2.0
//! Pages a bot check refused, waiting for a person who might open one.
//!
//! A queue rather than a call, for the same reason the Dock menu in the IDE
//! uses one: the refusal is noticed deep inside a tool, which has no handle on
//! the agent loop that owns the event channel and no business acquiring one.
//! Tools record; the loop drains after each tool call and raises the requests.
//!
//! What this is *not* is a way to get past a challenge. Nothing here retries,
//! and nothing here carries a cookie or a credential. It records that a
//! particular page was refused and lets the agent loop ask whether a person
//! wants to open it — and if one does, the page arrives from a real browser
//! that really did the asking. The distinction is the whole design: a request
//! made by a browser is a browser's request, while a cookie taken from a
//! browser and replayed through an HTTP client is a claim to be something it
//! is not.
//!
//! Requests are also short-lived by intent. The agent carries on with other
//! sources and the loop withdraws anything still outstanding when the turn
//! ends, because a page that turned out not to matter should stop asking to be
//! opened. A prompt that outlives its purpose is one people learn to dismiss.

use std::sync::Mutex;

/// One page that was refused, and by what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub url: String,
    /// The bot-management system that refused it, for a client that wants to
    /// say why — "Cloudflare", "DataDome", or an honest admission that it
    /// could not tell.
    pub refused_by: String,
}

/// Refusals recorded since the last drain.
///
/// Process-wide because the tools that record are called from wherever the
/// runtime puts them, and the one consumer is the agent loop. Bounded, so a
/// crawl of a wholly walled site cannot grow it without limit — see [`record`].
static PENDING: Mutex<Vec<Refusal>> = Mutex::new(Vec::new());

/// The most refusals kept between drains.
///
/// A crawl of a site that refuses everything would otherwise record a page at
/// a time until the page budget ran out, and nobody is going to open ninety
/// pages by hand. Past this the rest are dropped: the first few name the site,
/// which is the actionable part, and the tool's own text already says how many
/// pages were refused.
const MAX_PENDING: usize = 8;

/// Note that `url` was refused. Duplicates and floods are dropped.
pub fn record(url: &str, refused_by: &str) {
    let Ok(mut pending) = PENDING.lock() else {
        // A poisoned lock means another thread panicked holding it. Losing a
        // browser prompt is not worth propagating that.
        return;
    };
    if pending.len() >= MAX_PENDING || pending.iter().any(|r| r.url == url) {
        return;
    }
    pending.push(Refusal {
        url: url.to_string(),
        refused_by: refused_by.to_string(),
    });
}

/// Take everything recorded, leaving the queue empty.
pub fn drain() -> Vec<Refusal> {
    PENDING
        .lock()
        .map(|mut pending| std::mem::take(&mut *pending))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialised, because the queue is process-wide and these tests would
    /// otherwise see each other's entries — the mistake made once already in
    /// this session with a temp directory keyed on the process id.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        drain();
        g
    }

    #[test]
    fn a_refusal_is_recorded_and_drained_once() {
        let _g = guard();
        record("https://walled.test/a", "Cloudflare");
        let first = drain();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].url, "https://walled.test/a");
        assert_eq!(first[0].refused_by, "Cloudflare");
        // Draining empties it, so the agent loop cannot raise the same
        // request twice.
        assert!(drain().is_empty());
    }

    #[test]
    fn the_same_url_is_not_queued_twice() {
        let _g = guard();
        record("https://walled.test/a", "Cloudflare");
        record("https://walled.test/a", "Cloudflare");
        assert_eq!(drain().len(), 1);
    }

    /// A crawl of a wholly walled site must not queue a page at a time until
    /// the budget runs out. Nobody is opening ninety pages by hand, and the
    /// tool's own text already reports the count.
    #[test]
    fn a_flood_is_bounded() {
        let _g = guard();
        for i in 0..200 {
            record(&format!("https://walled.test/{i}"), "Cloudflare");
        }
        assert_eq!(drain().len(), MAX_PENDING);
    }

    #[test]
    fn draining_an_empty_queue_is_fine() {
        let _g = guard();
        assert!(drain().is_empty());
    }
}
