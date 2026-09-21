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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// Whether the client can put a page in front of a person and hand the result
/// back.
///
/// Declared once at startup by the host, which is the only thing that knows —
/// see `--host-can-browse`. Forge IDE can; a terminal cannot, since it has no
/// way to show a page and no way to read one back.
///
/// Process-global for the same reason the queue below is: the code that needs
/// this is the message text inside a tool, and a tool has no handle on the
/// agent and no business acquiring one.
///
/// Defaults to false, which is the safe direction. A client that forgets to
/// declare the capability gets told a page cannot be opened, which is merely
/// pessimistic; the other default would have the agent promise a browser
/// handoff that never comes, and then wait for it.
static CAN_BROWSE: AtomicBool = AtomicBool::new(false);

/// Record what the host can do. Called once, before any tool runs.
pub fn set_host_can_browse(can: bool) {
    CAN_BROWSE.store(can, Ordering::Relaxed);
}

/// Whether asking somebody to open a page is a real option here.
pub fn host_can_browse() -> bool {
    CAN_BROWSE.load(Ordering::Relaxed)
}

/// Take exclusive use of the queue and the capability flag, for a test.
///
/// Both are process-global and Rust runs tests as threads of one process, so
/// two tests touching them race — and they are in different modules, which is
/// how this nearly went wrong: each had its own lock, which serialises a
/// module against itself and not against the other. One lock, owned by the
/// module that owns the state.
///
/// Leaves the queue empty and the capability on, since a test about the queue
/// needs recording to be possible. A test about the off case says so itself.
#[cfg(test)]
pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    drain();
    set_host_can_browse(true);
    guard
}

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
///
/// Nothing is queued when the host cannot open a page. A request no client can
/// satisfy is worse than no request: the agent would raise it, nothing would
/// answer, and the turn would end with a withdrawal for something nobody ever
/// saw — while the user was told to do something their client cannot do.
pub fn record(url: &str, refused_by: &str) {
    if !host_can_browse() {
        return;
    }
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
    use super::test_guard as guard;

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

    /// A client that cannot open a page queues nothing.
    ///
    /// A request no client can satisfy is worse than no request. The agent
    /// would raise it, nothing would answer, the turn would end with a
    /// withdrawal for something nobody saw — and the person would have been
    /// told to do a thing their client cannot do. In a terminal the honest
    /// answer is that the page is out of reach, said once, and the agent
    /// carries on with what it has.
    #[test]
    fn a_host_with_no_browser_is_offered_nothing() {
        let _g = guard();
        set_host_can_browse(false);
        record("https://walled.test/a", "Cloudflare");
        record("https://walled.test/b", "Cloudflare");
        assert!(
            drain().is_empty(),
            "a page was queued for a client that cannot open one",
        );

        // And the capability is what decides it, not anything about the page.
        set_host_can_browse(true);
        record("https://walled.test/a", "Cloudflare");
        assert_eq!(drain().len(), 1);
    }

    /// Off by default, which is the safe direction: a host that forgets to
    /// declare the capability gets told a page cannot be opened, which is
    /// merely pessimistic. The other default promises a handoff that never
    /// comes.
    #[test]
    fn the_capability_is_off_until_declared() {
        let _g = guard();
        set_host_can_browse(false);
        assert!(!host_can_browse());
    }
}
