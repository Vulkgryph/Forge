// SPDX-License-Identifier: Apache-2.0
//! Console records become the same UTF-8/VT input the terminal decoder already
//! understands. Waiting on the console and resize event keeps idle CPU at zero.
use std::collections::VecDeque;
use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::Mutex;
use std::time::{Duration, Instant};

type Handle = *mut c_void;
pub const STDIN: i32 = 0;
pub const SIGWINCH: i32 = 28;
pub const SIGTERM: i32 = 15;
pub const SIGHUP: i32 = 1;
pub type SigHandler = extern "C" fn(i32);
static RESIZE: Mutex<Option<SigHandler>> = Mutex::new(None);
static FATAL: Mutex<Option<SigHandler>> = Mutex::new(None);
static EVENTS: Mutex<Vec<OwnedHandle>> = Mutex::new(Vec::new());
static INPUT: Mutex<Input> = Mutex::new(Input {
    bytes: VecDeque::new(),
    high: None,
});

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Coord {
    x: i16,
    y: i16,
}
#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i16,
    top: i16,
    right: i16,
    bottom: i16,
}
#[repr(C)]
#[derive(Default)]
struct ScreenInfo {
    size: Coord,
    cursor: Coord,
    attributes: u16,
    window: Rect,
    maximum: Coord,
}
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Key {
    down: i32,
    repeat: u16,
    virtual_key: u16,
    scan: u16,
    unicode: u16,
    modifiers: u32,
}
#[repr(C)]
#[derive(Default)]
struct Record {
    kind: u16,
    padding: u16,
    key: Key,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetStdHandle(which: u32) -> Handle;
    fn GetConsoleMode(handle: Handle, mode: *mut u32) -> i32;
    fn SetConsoleMode(handle: Handle, mode: u32) -> i32;
    fn GetConsoleOutputCP() -> u32;
    fn SetConsoleOutputCP(page: u32) -> i32;
    fn GetConsoleScreenBufferInfo(handle: Handle, info: *mut ScreenInfo) -> i32;
    fn ReadConsoleInputW(handle: Handle, record: *mut Record, count: u32, read: *mut u32) -> i32;
    fn WaitForMultipleObjects(count: u32, handles: *const Handle, all: i32, timeout: u32) -> u32;
    fn CreateEventW(
        attributes: *const c_void,
        manual: i32,
        initial: i32,
        name: *const u16,
    ) -> Handle;
    fn SetEvent(handle: Handle) -> i32;
    fn ResetEvent(handle: Handle) -> i32;
    fn SetConsoleCtrlHandler(handler: extern "system" fn(u32) -> i32, add: i32) -> i32;
}
unsafe extern "C" {
    fn _wexecv(path: *const u16, argv: *const *const u16) -> isize;
    fn _time64(out: *mut i64) -> i64;
    fn _localtime64_s(out: *mut i32, clock: *const i64) -> i32;
    fn strftime(buf: *mut u8, max: usize, format: *const i8, tm: *const i32) -> usize;
}

fn stdin() -> Handle {
    unsafe { GetStdHandle((-10i32) as u32) }
}
fn stdout() -> Handle {
    unsafe { GetStdHandle((-11i32) as u32) }
}

#[derive(Clone, Copy)]
pub struct Termios {
    input: u32,
    output: u32,
    page: u32,
}

pub fn get_attributes(_: i32) -> Option<Termios> {
    let mut modes = Termios {
        input: 0,
        output: 0,
        page: unsafe { GetConsoleOutputCP() },
    };
    // Both handles must be consoles: escape output redirected to a file cannot
    // provide an interactive terminal, even if stdin happens to be a console.
    unsafe {
        (GetConsoleMode(stdin(), &mut modes.input) != 0
            && GetConsoleMode(stdout(), &mut modes.output) != 0)
            .then_some(modes)
    }
}

pub fn set_attributes(_: i32, modes: &Termios) -> bool {
    unsafe {
        let input = SetConsoleMode(stdin(), modes.input);
        let output = SetConsoleMode(stdout(), modes.output);
        let page = SetConsoleOutputCP(modes.page);
        input != 0 && output != 0 && page != 0
    }
}

pub fn enable_raw_mode(fd: i32) -> Option<Termios> {
    let original = get_attributes(fd)?;
    // Record input requires WINDOW_INPUT; Quick Edit would suspend delivery
    // while selecting, and processed input would swallow Ctrl-C.
    let raw = Termios {
        input: 0x0080 | 0x0008,
        output: original.output | 0x0005,
        page: 65001,
    };
    if set_attributes(fd, &raw) {
        Some(original)
    } else {
        set_attributes(fd, &original);
        None
    }
}

pub fn is_terminal(_: i32) -> bool {
    let mut mode = 0;
    unsafe { GetConsoleMode(stdin(), &mut mode) != 0 }
}

pub fn window_size(_: i32) -> Option<(usize, usize)> {
    let mut info = ScreenInfo::default();
    if unsafe { GetConsoleScreenBufferInfo(stdout(), &mut info) } == 0 {
        return None;
    }
    let cols = i32::from(info.window.right) - i32::from(info.window.left) + 1;
    let rows = i32::from(info.window.bottom) - i32::from(info.window.top) + 1;
    (cols > 0 && rows > 0).then_some((cols as usize, rows as usize))
}

pub fn make_pipe() -> Option<(i32, i32)> {
    let handle = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
    if handle.is_null() {
        return None;
    }
    let mut events = EVENTS.lock().unwrap();
    events.push(unsafe { OwnedHandle::from_raw_handle(handle) });
    let id = events.len() as i32;
    Some((id, id))
}
fn event(fd: i32) -> Option<Handle> {
    EVENTS
        .lock()
        .unwrap()
        .get(fd.checked_sub(1)? as usize)
        .map(AsRawHandle::as_raw_handle)
}
pub fn notify(fd: i32) {
    if let Some(h) = event(fd) {
        unsafe {
            SetEvent(h);
        }
    }
}
pub fn drain(fd: i32) {
    if let Some(h) = event(fd) {
        unsafe {
            ResetEvent(h);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ready {
    Readable(u32),
    TimedOut,
    Interrupted,
    Failed,
}
pub fn wait_readable(fd: i32, timeout: Option<Duration>) -> Ready {
    wait_readable_many(&[fd], timeout)
}
pub fn wait_readable_many(fds: &[i32], timeout: Option<Duration>) -> Ready {
    if fds.is_empty() || fds.len() > 32 {
        return Ready::Failed;
    }
    let handles: Option<Vec<_>> = fds
        .iter()
        .map(|fd| {
            if *fd == STDIN {
                Some(stdin())
            } else {
                event(*fd)
            }
        })
        .collect();
    let Some(handles) = handles else {
        return Ready::Failed;
    };
    let started = Instant::now();
    loop {
        if let Some(index) = fds.iter().position(|fd| *fd == STDIN) {
            if !INPUT.lock().unwrap().bytes.is_empty() {
                return Ready::Readable(1 << index);
            }
        }
        let ms = timeout
            .map(|t| {
                t.saturating_sub(started.elapsed())
                    .as_millis()
                    .min(u32::MAX as u128 - 1) as u32
            })
            .unwrap_or(u32::MAX);
        let result =
            unsafe { WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), 0, ms) };
        if result == 258 {
            return Ready::TimedOut;
        }
        if result as usize >= fds.len() {
            return Ready::Failed;
        }
        if fds[result as usize] != STDIN {
            return Ready::Readable(1 << result);
        }
        let mut record = Record::default();
        let mut count = 0;
        if unsafe { ReadConsoleInputW(stdin(), &mut record, 1, &mut count) } == 0 {
            return Ready::Failed;
        }
        match record.kind {
            1 => INPUT.lock().unwrap().key(record.key),
            4 => {
                let handler = *RESIZE.lock().unwrap();
                if let Some(handler) = handler {
                    handler(SIGWINCH);
                }
                return Ready::Interrupted;
            }
            _ => {}
        }
    }
}
pub fn read_bytes(fd: i32, buf: &mut [u8]) -> Option<usize> {
    if fd != STDIN {
        drain(fd);
        return Some(0);
    }
    let mut input = INPUT.lock().unwrap();
    let n = buf.len().min(input.bytes.len());
    for byte in &mut buf[..n] {
        *byte = input.bytes.pop_front().unwrap();
    }
    Some(n)
}

struct Input {
    bytes: VecDeque<u8>,
    high: Option<u16>,
}
impl Input {
    fn key(&mut self, key: Key) {
        if key.down == 0 {
            return;
        }
        let shift = key.modifiers & 16 != 0;
        let ctrl = key.modifiers & 12 != 0;
        // AltGr is text entry, not an Alt shortcut.
        let alt = key.modifiers & 3 != 0 && !(key.modifiers & 1 != 0 && ctrl);
        let modifier = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
        let seq = match key.virtual_key {
            9 if shift => Some("\x1b[Z".to_string()),
            33..=40 | 45 | 46 if key.unicode == 0 => {
                let (code, tilde) = match key.virtual_key {
                    33 => ("5", true),
                    34 => ("6", true),
                    35 => ("F", false),
                    36 => ("H", false),
                    37 => ("D", false),
                    38 => ("A", false),
                    39 => ("C", false),
                    40 => ("B", false),
                    45 => ("2", true),
                    _ => ("3", true),
                };
                Some(if modifier == 1 {
                    format!("\x1b[{code}{}", if tilde { "~" } else { "" })
                } else if tilde {
                    format!("\x1b[{code};{modifier}~")
                } else {
                    format!("\x1b[1;{modifier}{code}")
                })
            }
            _ => None,
        };
        for _ in 0..key.repeat.max(1) {
            if let Some(seq) = &seq {
                self.bytes.extend(seq.bytes());
                continue;
            }
            let unit = key.unicode;
            if (0xd800..=0xdbff).contains(&unit) {
                self.high = Some(unit);
                continue;
            }
            if unit == 0 {
                continue;
            }
            let ch = if (0xdc00..=0xdfff).contains(&unit) {
                self.high.take().and_then(|high| {
                    char::from_u32(0x10000 + ((high as u32 - 0xd800) << 10) + unit as u32 - 0xdc00)
                })
            } else {
                self.high = None;
                char::from_u32(unit as u32)
            };
            if let Some(ch) = ch {
                if alt {
                    self.bytes.push_back(27);
                }
                self.bytes.extend(ch.encode_utf8(&mut [0; 4]).bytes());
            }
        }
    }
}

extern "system" fn control(kind: u32) -> i32 {
    if matches!(kind, 2 | 5 | 6) {
        let handler = *FATAL.lock().unwrap();
        if let Some(handler) = handler {
            handler(SIGTERM);
        }
        return 1;
    }
    0
}
pub fn on_signal(sig: i32, handler: SigHandler) {
    if sig == SIGWINCH {
        *RESIZE.lock().unwrap() = Some(handler);
    } else {
        *FATAL.lock().unwrap() = Some(handler);
        unsafe {
            SetConsoleCtrlHandler(control, 1);
        }
    }
}
pub fn reraise_default(sig: i32) {
    std::process::exit(128 + sig);
}

pub fn exec(program: &std::path::Path, args: &[String]) -> std::io::Error {
    use std::os::windows::ffi::OsStrExt;
    let mut path = program.as_os_str().encode_wide().collect::<Vec<_>>();
    let mut owned = vec![path.clone()];
    owned.extend(args.iter().map(|a| a.encode_utf16().collect()));
    if owned.iter().any(|s| s.contains(&0)) {
        return std::io::Error::new(std::io::ErrorKind::InvalidInput, "nul in argument");
    }
    path.push(0);
    // The CRT exec family joins argv with spaces without quoting it. Preserve
    // workspace paths, embedded quotes, and trailing backslashes on restart.
    let owned: Vec<_> = owned.iter().map(|value| quote_argument(value)).collect();
    let mut pointers: Vec<_> = owned.iter().map(|s| s.as_ptr()).collect();
    pointers.push(std::ptr::null());
    unsafe {
        _wexecv(path.as_ptr(), pointers.as_ptr());
    }
    std::io::Error::last_os_error()
}
fn quote_argument(value: &[u16]) -> Vec<u16> {
    let mut quoted = vec![34];
    let mut slashes = 0;
    for unit in value {
        if *unit == 92 {
            slashes += 1;
            continue;
        }
        quoted.extend(std::iter::repeat_n(
            92,
            if *unit == 34 {
                2 * slashes + 1
            } else {
                slashes
            },
        ));
        slashes = 0;
        quoted.push(*unit);
    }
    quoted.extend(std::iter::repeat_n(92, slashes * 2));
    quoted.extend([34, 0]);
    quoted
}
pub fn local_time(format: &str) -> Option<String> {
    let format = std::ffi::CString::new(format).ok()?;
    let mut clock = 0;
    let mut tm = [0; 9];
    let mut buf = [0; 128];
    unsafe {
        _time64(&mut clock);
        if _localtime64_s(tm.as_mut_ptr(), &clock) != 0 {
            return None;
        }
        let len = strftime(buf.as_mut_ptr(), buf.len(), format.as_ptr(), tm.as_ptr());
        if len == 0 {
            return None;
        }
        String::from_utf8(buf[..len].to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn WriteConsoleInputW(
            handle: Handle,
            record: *const Record,
            count: u32,
            written: *mut u32,
        ) -> i32;
    }
    #[test]
    #[ignore = "requires an attached Windows console; run with --ignored --nocapture"]
    fn real_console_input_and_mode_restoration() {
        let original = enable_raw_mode(STDIN).expect("attached console");
        struct Restore(Termios);
        impl Drop for Restore {
            fn drop(&mut self) {
                set_attributes(STDIN, &self.0);
            }
        }
        let restore = Restore(original);
        let raw = get_attributes(STDIN).unwrap();
        assert_eq!(
            raw.input & 7,
            0,
            "line buffering, echo and Ctrl-C processing must be off"
        );
        assert_ne!(raw.output & 4, 0, "VT output must be on");
        assert!(window_size(STDIN).is_some());
        let key = Record {
            kind: 1,
            key: Key {
                down: 1,
                repeat: 1,
                unicode: 0x03bb,
                ..Key::default()
            },
            ..Record::default()
        };
        let mut written = 0;
        assert_ne!(
            unsafe { WriteConsoleInputW(stdin(), &key, 1, &mut written) },
            0
        );
        loop {
            match wait_readable(STDIN, Some(Duration::from_secs(1))) {
                Ready::Interrupted => continue,
                Ready::Readable(1) => break,
                other => panic!("input did not arrive: {other:?}"),
            }
        }
        let mut bytes = [0; 8];
        let count = read_bytes(STDIN, &mut bytes).unwrap();
        assert_eq!(&bytes[..count], "λ".as_bytes());
        drop(restore);
        let restored = get_attributes(STDIN).unwrap();
        assert_eq!(
            (restored.input, restored.output, restored.page),
            (original.input, original.output, original.page)
        );
    }
    #[test]
    fn console_keys_preserve_unicode_modifiers_and_repeats() {
        let mut input = Input {
            bytes: VecDeque::new(),
            high: None,
        };
        for (virtual_key, unicode, modifiers, repeat) in [
            (38, 0, 8, 1),
            (9, 9, 16, 1),
            (65, 97, 0, 2),
            (0, 0xd83d, 0, 1),
            (0, 0xde00, 0, 1),
            (69, 0x20ac, 9, 1),
        ] {
            input.key(Key {
                down: 1,
                virtual_key,
                unicode,
                modifiers,
                repeat,
                ..Key::default()
            });
        }
        assert_eq!(
            input.bytes.into_iter().collect::<Vec<_>>(),
            "\x1b[1;5A\x1b[Zaa😀€".as_bytes()
        );
    }
    #[test]
    fn notification_wakes_and_drains_without_console_input() {
        let (read, write) = make_pipe().unwrap();
        notify(write);
        assert_eq!(
            wait_readable(read, Some(Duration::ZERO)),
            Ready::Readable(1)
        );
        drain(read);
        assert_eq!(wait_readable(read, Some(Duration::ZERO)), Ready::TimedOut);
    }
    #[test]
    fn clock_uses_local_time() {
        assert_eq!(local_time("%H:%M:%S").unwrap().len(), 8);
    }
    #[test]
    fn restart_quotes_paths_without_losing_trailing_slashes() {
        for (input, expected) in [
            ("", "\"\""),
            ("C:\\my work\\", "\"C:\\my work\\\\\""),
            ("a\"b", "\"a\\\"b\""),
        ] {
            let quoted = quote_argument(&input.encode_utf16().collect::<Vec<_>>());
            assert_eq!(
                String::from_utf16(&quoted[..quoted.len() - 1]).unwrap(),
                expected
            );
        }
    }
}
