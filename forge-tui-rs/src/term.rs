// SPDX-License-Identifier: Apache-2.0
//! Owning the terminal, and giving it back.
//!
//! A TUI puts the terminal into a state no shell can use: raw mode (no echo, no
//! line buffering, no Ctrl-C signal), the alternate screen, hidden cursor. If
//! the process leaves without undoing that, the user's shell is left mute and
//! invisible and they have to type `reset` blind. So restoration has to survive
//! every exit path, not just the happy one:
//!
//!  * normal return — [`Guard`]'s `Drop`
//!  * panic — a hook that restores before unwinding prints, so the message is
//!    readable on the main screen instead of being wiped with the alternate one
//!  * `SIGTERM`/`SIGHUP` — not delivered as Rust unwinding, so `Drop` never
//!    runs; handled by restoring from the handler
//!
//! Ctrl-C needs no signal handler: raw mode turns off ISIG, so it arrives as an
//! ordinary key event and the event loop decides what it means.
//!
//! The alternate screen is what keeps this TUI out of the class of bugs that
//! plagued the last one. Drawing on a separate buffer means repaints cannot
//! touch the scrollback the user built up, so there is no reason to ever erase
//! it — and `ESC[3J`, the sequence that erased it and caused the visible jitter,
//! is never emitted at all.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::sys::{self, Termios};

/// Set once the terminal has been modified, so restoration is idempotent and
/// safe to attempt from a signal handler that may race with `Drop`.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// The terminal settings from before we took over, to be put back exactly.
///
/// Kept rather than reconstructed: the user's shell may have set anything, and
/// guessing at a "normal" configuration would silently change their setup.
static ORIGINAL: Mutex<Option<Termios>> = Mutex::new(None);

/// Set by the `SIGWINCH` handler; the input thread turns it into a resize.
static RESIZED: AtomicBool = AtomicBool::new(false);

/// Take and clear the pending-resize flag.
pub fn take_resize() -> bool {
    RESIZED.swap(false, Ordering::SeqCst)
}

/// Enter/leave sequences, written directly rather than through crossterm's
/// command queue so the exact bytes are visible and testable.
const ENTER_ALT: &str = "\x1b[?1049h";
const LEAVE_ALT: &str = "\x1b[?1049l";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
/// Report key presses as unambiguous sequences where the terminal supports it.
const PUSH_KEYBOARD: &str = "\x1b[>1u";
const POP_KEYBOARD: &str = "\x1b[<u";
const ENABLE_BRACKETED_PASTE: &str = "\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &str = "\x1b[?2004l";
/// Autowrap (DECAWM). Off while we own the screen.
///
/// With it on, writing the bottom-right cell wraps and scrolls: the frame just
/// drawn shifts up a row while the next one is written at absolute positions,
/// leaving two frames on screen at once. A full-screen renderer that addresses
/// cells directly never wants the terminal moving them, so this is switched off
/// for the duration and restored on the way out.
const DISABLE_AUTOWRAP: &str = "\x1b[?7l";
const ENABLE_AUTOWRAP: &str = "\x1b[?7h";

/// Holds the terminal in TUI state and gives it back when dropped.
pub struct Guard {
    _private: (),
}

impl Guard {
    /// Take over the terminal.
    ///
    /// Installs the panic hook and signal handlers as a side effect; both are
    /// process-global and safe to install more than once.
    pub fn new() -> io::Result<Self> {
        let original = sys::enable_raw_mode(sys::STDIN).ok_or_else(|| {
            io::Error::other(
                "could not put the terminal into raw mode — is this a terminal?",
            )
        })?;
        if let Ok(mut slot) = ORIGINAL.lock() {
            *slot = Some(original);
        }

        let mut out = io::stdout();
        // Bracketed paste before the alternate screen, so a paste arriving
        // during startup is already delimited.
        write!(
            out,
            "{ENABLE_BRACKETED_PASTE}{ENTER_ALT}{DISABLE_AUTOWRAP}{HIDE_CURSOR}{PUSH_KEYBOARD}",
        )?;
        out.flush()?;

        ACTIVE.store(true, Ordering::SeqCst);
        install_panic_hook();
        install_signal_handlers();

        Ok(Self { _private: () })
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        restore();
    }
}

/// Put the terminal back the way it was. Safe to call repeatedly and from a
/// signal handler.
pub fn restore() {
    // `swap` rather than load-then-store: two exit paths can race (a SIGTERM
    // arriving while Drop runs), and the loser must not emit a second set of
    // sequences.
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let mut out = io::stdout();
    // Order mirrors setup in reverse. Note there is no `ESC[3J` here and none
    // anywhere else: the alternate screen means the user's scrollback was never
    // ours to erase.
    let _ = write!(
        out,
        "{POP_KEYBOARD}{SHOW_CURSOR}{ENABLE_AUTOWRAP}{LEAVE_ALT}{DISABLE_BRACKETED_PASTE}",
    );
    let _ = out.flush();

    // Put back exactly what was there, rather than an approximation of it.
    let original = ORIGINAL.lock().ok().and_then(|mut slot| slot.take());
    if let Some(settings) = original {
        sys::set_attributes(sys::STDIN, &settings);
    }
}

/// Restore before the default hook prints, so the backtrace lands on the main
/// screen where it can be read and scrolled.
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            default(info);
        }));
    });
}

/// `SIGTERM` and `SIGHUP` terminate the process without unwinding, so `Drop`
/// never runs and the terminal would be left raw. Restore, then re-raise with
/// the handler cleared so the default disposition and exit status apply.
fn install_signal_handlers() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        sys::on_signal(sys::SIGTERM, handle_fatal);
        sys::on_signal(sys::SIGHUP, handle_fatal);
        sys::on_signal(sys::SIGWINCH, handle_winch);
    });
}

/// `SIGTERM`/`SIGHUP`: restore, then die the way the caller expects.
///
/// `restore` writes to stdout, which is more than a strictly async-signal-safe
/// handler should do. The alternative is a terminal left raw and invisible, and
/// the process is ending either way.
extern "C" fn handle_fatal(sig: std::os::raw::c_int) {
    restore();
    sys::reraise_default(sig);
}

/// `SIGWINCH`: note it and return. Only an atomic store, which *is* safe here —
/// the actual re-layout happens on the input thread.
extern "C" fn handle_winch(_sig: std::os::raw::c_int) {
    RESIZED.store(true, Ordering::SeqCst);
}

/// Current terminal size, falling back to a usable default when it is unknown
/// (a pipe, or a terminal that will not answer).
pub fn size() -> (usize, usize) {
    sys::window_size(sys::STDIN).unwrap_or((80, 24))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequences we emit are part of the contract with forge-ide's emulator,
    /// which has matching tests. Pin them so a refactor cannot quietly change
    /// what the emulator has to understand.
    #[test]
    fn uses_1049_for_the_alternate_screen() {
        assert_eq!(ENTER_ALT, "\x1b[?1049h");
        assert_eq!(LEAVE_ALT, "\x1b[?1049l");
    }

    /// The specific sequence that caused the original jitter. It must appear
    /// nowhere in this module.
    #[test]
    fn no_setup_or_teardown_sequence_erases_scrollback() {
        for seq in [
            ENTER_ALT, LEAVE_ALT, HIDE_CURSOR, SHOW_CURSOR,
            PUSH_KEYBOARD, POP_KEYBOARD,
            ENABLE_BRACKETED_PASTE, DISABLE_BRACKETED_PASTE,
            DISABLE_AUTOWRAP, ENABLE_AUTOWRAP,
        ] {
            assert!(!seq.contains("3J"), "{seq:?} must not erase scrollback");
            assert!(!seq.contains("2J"), "{seq:?} must not clear the screen");
        }
    }

    /// Restoration must be idempotent: `Drop` and a signal handler can both run.
    /// Autowrap must be off while we own the screen and back on afterwards.
    /// Leaving it on lets a write in the last cell scroll the display, which
    /// puts two frames on screen at once; leaving it off afterwards would break
    /// the user's shell.
    #[test]
    fn autowrap_is_disabled_during_and_restored_after() {
        assert_eq!(DISABLE_AUTOWRAP, "\x1b[?7l");
        assert_eq!(ENABLE_AUTOWRAP, "\x1b[?7h");
    }

    #[test]
    fn restore_is_idempotent_when_inactive() {
        ACTIVE.store(false, Ordering::SeqCst);
        restore();
        restore();
        assert!(!ACTIVE.load(Ordering::SeqCst));
    }

    #[test]
    fn size_has_a_usable_fallback() {
        let (cols, rows) = size();
        assert!(cols > 0 && rows > 0, "never zero, even without a tty");
    }
}
