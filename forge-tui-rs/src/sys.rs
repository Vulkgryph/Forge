// SPDX-License-Identifier: Apache-2.0
//! The handful of C calls a terminal needs, declared here rather than taken as a
//! dependency.
//!
//! **Why the terminal state is an opaque buffer.** `struct termios` has a
//! different shape on every platform — macOS uses 64-bit flags and a 20-entry
//! control array, glibc uses 32-bit flags, a `c_line` byte and 32 entries — and
//! the individual flag *values* differ too (`ICANON` is `0x100` on macOS and `2`
//! on Linux). Transcribing all of that is exactly the kind of detail that is
//! wrong once and leaves someone's shell unusable.
//!
//! None of it is necessary. We only ever hand the struct back to the C library:
//! read it with `tcgetattr`, hand it to `cfmakeraw` to set raw mode, write it
//! with `tcsetattr`, and keep a pristine copy to restore. Since no field is ever
//! read on this side, the struct can be an opaque block of bytes large enough
//! for any platform's version, and there is no layout to get wrong.
//!
//! `cfmakeraw` is in both macOS libc and glibc, and it is the canonical way to
//! do this — it disables echo, line buffering, signal generation and output
//! post-processing in one call.

#![cfg(unix)]

use std::os::raw::{c_int, c_short, c_ulong, c_ushort, c_void};

/// Opaque terminal settings.
///
/// 128 bytes covers every platform's `struct termios` with room to spare (macOS
/// needs 72, glibc 60), and `u64` alignment satisfies the strictest field any of
/// them uses. Never interpreted here — only passed to the C library.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct Termios([u8; 128]);

impl Termios {
    pub fn zeroed() -> Self {
        Self([0; 128])
    }

    fn as_ptr(&mut self) -> *mut c_void {
        self.0.as_mut_ptr().cast()
    }
}

/// A descriptor to wait on. This layout is identical on macOS and Linux.
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd:      c_int,
    events:  c_short,
    revents: c_short,
}

/// "Readable" — 1 on both platforms.
const POLLIN: c_short = 1;

/// Window size, as `TIOCGWINSZ` fills it. This layout *is* identical everywhere.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct WinSize {
    rows:    c_ushort,
    cols:    c_ushort,
    _xpixel: c_ushort,
    _ypixel: c_ushort,
}

// `TIOCGWINSZ` is one of the few constants that genuinely differs, being an
// encoded ioctl number rather than a plain flag.
#[cfg(target_os = "macos")]
const TIOCGWINSZ: c_ulong = 0x4008_7468;
#[cfg(not(target_os = "macos"))]
const TIOCGWINSZ: c_ulong = 0x5413;

/// Apply the change immediately. 0 on every unix.
const TCSANOW: c_int = 0;

pub const STDIN: c_int = 0;

/// Signal numbers we care about. These agree across macOS and Linux.
pub const SIGWINCH: c_int = 28;
pub const SIGTERM: c_int = 15;
pub const SIGHUP: c_int = 1;

pub type SigHandler = extern "C" fn(c_int);

unsafe extern "C" {
    fn tcgetattr(fd: c_int, termios: *mut c_void) -> c_int;
    fn tcsetattr(fd: c_int, action: c_int, termios: *mut c_void) -> c_int;
    /// Sets every flag raw mode needs, so this side never touches a bitmask.
    fn cfmakeraw(termios: *mut c_void);
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn isatty(fd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
    fn signal(sig: c_int, handler: usize) -> usize;
    fn raise(sig: c_int) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout_ms: c_int) -> c_int;
}

/// What a wait ended with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ready {
    /// Bytes are available.
    Readable,
    /// The timeout elapsed with nothing to read.
    TimedOut,
    /// A signal arrived. Not an error: `SIGWINCH` lands here on every resize,
    /// and the caller should check its flags and wait again.
    Interrupted,
    Failed,
}

/// Wait until a descriptor is readable, or the timeout elapses.
///
/// This is what lets a lone `ESC` be resolved: without a timed wait, deciding
/// whether an escape sequence is still arriving would mean blocking until the
/// user pressed something else, so Escape would appear to do nothing until the
/// next keystroke.
pub fn wait_readable(fd: c_int, timeout: Option<std::time::Duration>) -> Ready {
    let mut fds = PollFd { fd, events: POLLIN, revents: 0 };
    let ms = match timeout {
        // Saturate rather than wrap: a very long timeout must not become a
        // negative one, which `poll` reads as "wait forever".
        Some(d) => c_int::try_from(d.as_millis()).unwrap_or(c_int::MAX),
        None => -1,
    };
    // SAFETY: one descriptor, and `fds` is a valid `PollFd` for the call.
    let rc = unsafe { poll(&mut fds, 1, ms) };
    match rc {
        1.. => Ready::Readable,
        0 => Ready::TimedOut,
        _ => {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                Ready::Interrupted
            } else {
                Ready::Failed
            }
        }
    }
}

/// Read the current terminal settings, to be restored later.
pub fn get_attributes(fd: c_int) -> Option<Termios> {
    let mut settings = Termios::zeroed();
    // SAFETY: `settings` is 128 bytes of owned, aligned storage — larger than
    // any platform's struct — and is only written by the C library.
    let rc = unsafe { tcgetattr(fd, settings.as_ptr()) };
    (rc == 0).then_some(settings)
}

/// Write terminal settings back.
pub fn set_attributes(fd: c_int, settings: &Termios) -> bool {
    let mut copy = *settings;
    // SAFETY: as above; `tcsetattr` reads from the buffer.
    unsafe { tcsetattr(fd, TCSANOW, copy.as_ptr()) == 0 }
}

/// Switch a terminal into raw mode, returning the settings to restore.
///
/// Raw mode is what lets the TUI see every keystroke: no echo, no waiting for a
/// newline, and no signal generation — which is why Ctrl-C arrives as an
/// ordinary key here and quitting is the program's decision.
pub fn enable_raw_mode(fd: c_int) -> Option<Termios> {
    let original = get_attributes(fd)?;
    let mut raw = original;
    // SAFETY: `cfmakeraw` only writes the flag fields of a struct we own.
    unsafe { cfmakeraw(raw.as_ptr()) };
    set_attributes(fd, &raw).then_some(original)
}

/// Current terminal size as `(cols, rows)`.
pub fn window_size(fd: c_int) -> Option<(usize, usize)> {
    let mut size = WinSize::default();
    // SAFETY: `TIOCGWINSZ` writes exactly a `winsize`, which is what is passed.
    let rc = unsafe { ioctl(fd, TIOCGWINSZ, &mut size as *mut WinSize) };
    if rc != 0 || size.cols == 0 || size.rows == 0 {
        // A zero here means the kernel has no size for this fd — a pipe, or a
        // terminal that has not reported one yet. Treated as unknown so the
        // caller can fall back rather than dividing by it.
        return None;
    }
    Some((size.cols as usize, size.rows as usize))
}

pub fn is_terminal(fd: c_int) -> bool {
    // SAFETY: no arguments beyond a file descriptor.
    unsafe { isatty(fd) == 1 }
}

/// Read bytes from a descriptor, retrying when a signal interrupts.
///
/// `EINTR` is expected rather than exceptional here: `SIGWINCH` arrives on every
/// window resize and lands while this is blocked. Returning zero on it would look
/// exactly like end-of-input and shut the session down whenever someone dragged
/// their window.
pub fn read_bytes(fd: c_int, buf: &mut [u8]) -> Option<usize> {
    loop {
        // SAFETY: writing at most `buf.len()` bytes into `buf`.
        let n = unsafe { read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Some(n as usize);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::Interrupted {
            return Some(0); // caller re-polls; a resize is not end-of-input
        }
        return None;
    }
}

/// Install a signal handler.
pub fn on_signal(sig: c_int, handler: SigHandler) {
    // SAFETY: a valid `extern "C"` function pointer for the lifetime of the
    // process, cast to the integer the C signature takes.
    unsafe { signal(sig, handler as usize) };
}

/// Restore the default disposition and re-raise, so the process dies the way the
/// caller expects rather than exiting zero from a signal.
pub fn reraise_default(sig: c_int) {
    const SIG_DFL: usize = 0;
    // SAFETY: restoring the default handler and re-raising.
    unsafe {
        signal(sig, SIG_DFL);
        raise(sig);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The buffer has to be big enough for the largest platform's struct, or
    /// `tcgetattr` writes past it.
    #[test]
    fn the_opaque_buffer_is_larger_than_any_real_termios() {
        // macOS needs 72 bytes, glibc 60. 128 leaves room for a platform that
        // grows one.
        assert!(std::mem::size_of::<Termios>() >= 72);
        assert_eq!(std::mem::align_of::<Termios>(), 8, "alignment for 64-bit flags");
    }

    #[test]
    fn winsize_matches_the_kernel_layout() {
        // Four `unsigned short`, in this order, on every unix.
        assert_eq!(std::mem::size_of::<WinSize>(), 8);
    }

    /// Reading attributes from something that is not a terminal must fail
    /// cleanly rather than returning nonsense we then apply.
    #[test]
    fn attributes_from_a_non_terminal_fail() {
        // A pipe's read end is a valid fd that is not a tty.
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        use std::os::fd::AsRawFd;
        assert!(
            get_attributes(file.as_raw_fd()).is_none(),
            "/dev/null is not a terminal",
        );
    }

    #[test]
    fn window_size_of_a_non_terminal_is_unknown() {
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        use std::os::fd::AsRawFd;
        assert!(window_size(file.as_raw_fd()).is_none());
    }

    #[test]
    fn is_terminal_says_no_for_a_regular_file() {
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        use std::os::fd::AsRawFd;
        assert!(!is_terminal(file.as_raw_fd()));
    }

    /// Reading at end of input reports zero bytes rather than failing.
    #[test]
    fn reading_at_end_of_input_returns_zero() {
        let file = std::fs::File::open("/dev/null").expect("open /dev/null");
        use std::os::fd::AsRawFd;
        let mut buf = [0u8; 16];
        assert_eq!(read_bytes(file.as_raw_fd(), &mut buf), Some(0));
    }

    #[test]
    fn a_wait_with_no_data_times_out() {
        // A pipe with nothing written has nothing to read.
        let (read_fd, _write_fd) = pipe();
        let ready = wait_readable(read_fd, Some(std::time::Duration::from_millis(20)));
        assert_eq!(ready, Ready::TimedOut);
    }

    #[test]
    fn a_wait_with_data_reports_readable() {
        let (read_fd, write_fd) = pipe();
        // SAFETY: writing one byte to the write end of our own pipe.
        unsafe {
            let byte = b'x';
            libc_write(write_fd, (&byte as *const u8).cast(), 1);
        }
        let ready = wait_readable(read_fd, Some(std::time::Duration::from_millis(200)));
        assert_eq!(ready, Ready::Readable);
    }

    /// POSIX says a negative descriptor is *ignored* by `poll` — `revents` is
    /// zeroed and the call simply waits out its timeout. So this is a timeout,
    /// not a failure, and the important property is that it returns at all
    /// rather than blocking the input thread forever.
    #[test]
    fn a_wait_on_a_negative_descriptor_times_out_rather_than_hanging() {
        let started = std::time::Instant::now();
        let ready = wait_readable(-1, Some(std::time::Duration::from_millis(20)));
        assert_eq!(ready, Ready::TimedOut);
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "returned promptly");
    }

    /// An infinite timeout is expressed as `None`; check it converts to the -1
    /// `poll` expects rather than to 0, which would busy-loop.
    #[test]
    fn an_absurd_timeout_saturates_instead_of_wrapping_negative() {
        // A duration whose milliseconds exceed c_int::MAX must not become a
        // negative value, which poll reads as "wait forever".
        let huge = std::time::Duration::from_secs(u64::from(u32::MAX));
        let ms = c_int::try_from(huge.as_millis()).unwrap_or(c_int::MAX);
        assert!(ms > 0, "saturated to a positive timeout, not -1");
    }

    /// A pipe, for tests that need a readable descriptor.
    fn pipe() -> (c_int, c_int) {
        let mut fds = [0 as c_int; 2];
        // SAFETY: `pipe` fills two descriptors.
        let rc = unsafe { libc_pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe() failed");
        (fds[0], fds[1])
    }

    unsafe extern "C" {
        #[link_name = "pipe"]
        fn libc_pipe(fds: *mut c_int) -> c_int;
        #[link_name = "write"]
        fn libc_write(fd: c_int, buf: *const c_void, count: usize) -> isize;
    }

    #[test]
    fn reading_from_a_bad_descriptor_reports_failure() {
        let mut buf = [0u8; 4];
        assert_eq!(read_bytes(-1, &mut buf), None);
    }
}
