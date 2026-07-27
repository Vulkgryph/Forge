//! Waking the event loop from background threads.
//!
//! The event loop sleeps in `ControlFlow::Wait`/`WaitUntil` between frames.
//! Real input wakes it automatically — winit delivers that as an OS event — but
//! an `mpsc::Sender::send` from a worker thread does not: the channel has no
//! connection to the run loop, so the message just sits in the queue until
//! something else causes a frame.
//!
//! That gap is why `IdeApp::draw` used to end with an unconditional
//! `request_repaint_after(300ms)`: a blanket 3.3 Hz poll so anything arriving on
//! any channel would be noticed "soon enough". It worked, but it meant the app
//! never actually idled — measured at ~4% of a core doing nothing, with a full
//! egui layout plus GPU submit (~8.7ms) on every tick.
//!
//! The fix is to let the producers say something arrived. A worker calls
//! [`wake`] after sending, which pokes the event loop via winit's
//! `EventLoopProxy`; the loop marks itself dirty and renders one frame. Idle
//! then costs nothing at all, because nothing is scheduled when nothing is
//! pending.
//!
//! Calls before [`set_waker`] (or after the loop exits) are no-ops, so worker
//! threads never need to care whether the UI is up.

use std::sync::OnceLock;

type Waker = Box<dyn Fn() + Send + Sync + 'static>;

static WAKER: OnceLock<Waker> = OnceLock::new();

/// Install the process-wide waker. Called once from `main` with a closure that
/// sends a user event through the event-loop proxy. Later calls are ignored.
pub fn set_waker(f: impl Fn() + Send + Sync + 'static) {
    let _ = WAKER.set(Box::new(f));
}

/// Ask the event loop to render a frame soon. Cheap and safe to call from any
/// thread, including before the loop exists.
pub fn wake() {
    if let Some(w) = WAKER.get() { w(); }
}
