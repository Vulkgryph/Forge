// SPDX-License-Identifier: Apache-2.0
//! Turning terminal events into [`Input`].
//!
//! Kept separate from the app so the mapping is one small, readable table and
//! the app never sees a crossterm type. That also means the app's state machine
//! can be driven from tests without synthesising terminal events.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::app::Input;

/// Map a terminal event to an [`Input`], or `None` for events we ignore.
pub fn decode(event: Event) -> Option<Input> {
    match event {
        Event::Key(key) => decode_key(key),
        Event::Paste(text) => Some(Input::Paste(text)),
        Event::Resize(cols, rows) => Some(Input::Resize(cols as usize, rows as usize)),
        _ => None,
    }
}

fn decode_key(key: KeyEvent) -> Option<Input> {
    // With the keyboard enhancement flags pushed, terminals report releases and
    // repeats too. Acting on a release would double every keystroke.
    if key.kind == KeyEventKind::Release {
        return None;
    }

    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    // Raw mode disables ISIG, so Ctrl-C is an ordinary key and quitting is our
    // decision rather than the kernel's.
    if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) {
        return Some(Input::Quit);
    }

    Some(match key.code {
        KeyCode::Char(c) if ctrl => return ctrl_key(c),
        KeyCode::Char(c)  => Input::Char(c),
        KeyCode::Enter    => Input::Enter,
        KeyCode::Backspace => Input::Backspace,
        KeyCode::Up       => Input::Up,
        KeyCode::Down     => Input::Down,
        KeyCode::PageUp   => Input::PageUp,
        KeyCode::PageDown => Input::PageDown,
        KeyCode::Home     => Input::Home,
        KeyCode::End      => Input::End,
        KeyCode::Esc      => Input::End,
        _ => return None,
    })
}

/// The few control keys worth binding. Anything else is dropped rather than
/// inserted as a control character into the input.
///
/// Interrupt is Ctrl-X rather than the customary Ctrl-C: raw mode delivers
/// Ctrl-C as an ordinary key, and a user pressing it expects to leave, not to
/// cancel a turn and stay. Binding it to cancel would make quitting impossible.
fn ctrl_key(c: char) -> Option<Input> {
    Some(match c {
        'x' => Input::Interrupt,
        'u' => Input::PageUp,
        'd' => Input::PageDown,
        'g' => Input::Home,
        _   => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn ctrl(c: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
    }

    #[test]
    fn ordinary_characters_pass_through() {
        assert_eq!(decode(key(KeyCode::Char('a'))), Some(Input::Char('a')));
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit() {
        assert_eq!(decode(ctrl('c')), Some(Input::Quit));
        assert_eq!(decode(ctrl('d')), Some(Input::Quit));
    }

    /// Terminals with keyboard enhancements report key releases. Treating one as
    /// a press would insert every character twice.
    #[test]
    fn key_releases_are_ignored() {
        let mut ev = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        ev.kind = KeyEventKind::Release;
        assert_eq!(decode(Event::Key(ev)), None);
    }

    #[test]
    fn navigation_keys_map_over() {
        for (code, expected) in [
            (KeyCode::Up, Input::Up),
            (KeyCode::Down, Input::Down),
            (KeyCode::PageUp, Input::PageUp),
            (KeyCode::PageDown, Input::PageDown),
            (KeyCode::Home, Input::Home),
            (KeyCode::End, Input::End),
            (KeyCode::Enter, Input::Enter),
            (KeyCode::Backspace, Input::Backspace),
        ] {
            assert_eq!(decode(key(code)), Some(expected));
        }
    }

    #[test]
    fn paste_arrives_whole() {
        assert_eq!(
            decode(Event::Paste("two words".into())),
            Some(Input::Paste("two words".into())),
        );
    }

    #[test]
    fn resize_is_forwarded() {
        assert_eq!(decode(Event::Resize(100, 40)), Some(Input::Resize(100, 40)));
    }

    /// An unbound control key must be dropped, not inserted as a literal.
    #[test]
    fn unbound_control_keys_are_dropped() {
        assert_eq!(decode(ctrl('q')), None);
        assert_eq!(decode(ctrl('z')), None);
    }

    #[test]
    fn bound_control_keys_scroll() {
        assert_eq!(decode(ctrl('u')), Some(Input::PageUp));
        assert_eq!(decode(ctrl('d')), Some(Input::Quit)); // quit wins over scroll
        assert_eq!(decode(ctrl('g')), Some(Input::Home));
    }

    /// The help text advertises Ctrl-X for interrupt, so it has to produce one.
    #[test]
    fn ctrl_x_interrupts() {
        assert_eq!(decode(ctrl('x')), Some(Input::Interrupt));
    }

    /// Ctrl-C must remain quit. Binding it to cancel-the-turn would leave no way
    /// out of the program.
    #[test]
    fn ctrl_c_remains_quit_not_interrupt() {
        assert_eq!(decode(ctrl('c')), Some(Input::Quit));
    }
}
