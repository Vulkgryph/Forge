// SPDX-License-Identifier: Apache-2.0
//! A terminal UI for Forge, rendered from scratch.
//!
//! Built to be free of the failure modes the previous ink-based TUI had, each of
//! which is prevented structurally rather than patched:
//!
//!  * **Messages cannot overlap.** [`width`] is the only thing that decides how
//!    wide text is, and both [`markdown`] (wrapping) and [`screen`] (advancing
//!    the cursor) use it. The old TUI had ink measuring with `string-width` and
//!    forge-ide's emulator counting one cell per character; when a line straddled
//!    the wrap boundary they computed different row counts, the cursor-up landed
//!    wrong, and text drew over text. Two implementations of one rule drift. One
//!    cannot.
//!  * **Scrollback cannot be damaged.** [`term`] draws on the alternate screen,
//!    so the user's history is never ours to erase. `ESC[3J` — the sequence that
//!    caused the visible jitter and the jumping scrollbar — is emitted nowhere,
//!    and tests assert that.
//!  * **Frames cannot tear.** [`screen`] diffs against what is on screen and
//!    writes the difference inside one synchronized update (`ESC[?2026h`/`l`).
//!  * **Idle costs nothing.** The loop blocks on input rather than repainting on
//!    a timer, so an untouched session uses no CPU.
//!
//! The layering is deliberately flat: [`app`] is a pure state machine over
//! [`Input`](app::Input), so scrolling and layout are testable without a
//! terminal, and [`input`] is the only module that knows crossterm exists.

pub mod app;
pub mod bridge;
pub mod clipboard;
pub mod commands;
pub mod dialog;
pub mod highlight;
pub mod inline;
pub mod input;
pub mod keys;
pub mod markdown;
pub mod model_display;
pub mod menu;
pub mod screen;
pub mod session;
pub mod sessions;
pub mod sys;
pub mod term;
pub mod widgets;
pub mod width;
