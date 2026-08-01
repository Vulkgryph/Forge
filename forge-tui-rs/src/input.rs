// SPDX-License-Identifier: Apache-2.0
//! Binding keys to actions.
//!
//! Kept apart from both the decoder and the app: [`crate::keys`] decides what
//! key was pressed, this decides what it means, and [`crate::app`] carries it
//! out. One small table, and the app never sees a keycode.

use crate::app::Input;
use crate::keys::Key;

/// Map a key to an action, or `None` for keys we do not bind.
pub fn bind(key: Key) -> Option<Input> {
    Some(match key {
        // Raw mode turns off signal generation, so Ctrl-C arrives here as an
        // ordinary key and leaving is this program's decision rather than the
        // kernel's.
        Key::Ctrl('c') | Key::Ctrl('d') => Input::Quit,

        // Interrupt is Ctrl-X, not Ctrl-C: a user pressing Ctrl-C expects to
        // leave, and binding it to cancel-the-turn would make quitting
        // impossible.
        Key::Ctrl('x') => Input::Interrupt,

        Key::Ctrl('t') => Input::Menu,
        Key::Ctrl('u') => Input::PageUp,
        Key::Ctrl('g') => Input::Home,

        Key::Char(c) => Input::Char(c),
        Key::Paste(text) => Input::Paste(text),
        Key::Enter => Input::Enter,
        Key::Backspace => Input::Backspace,

        Key::Up => Input::Up,
        Key::Down => Input::Down,
        Key::PageUp => Input::PageUp,
        Key::PageDown => Input::PageDown,
        Key::Home => Input::Home,

        // Escape and End both mean "stop scrolling and follow the newest
        // output", which is also how a dialog reads a dismissal.
        Key::Escape | Key::End => Input::End,

        // Unbound: Tab, arrows we do not use for navigation, and any control
        // chord not listed above. Dropped rather than inserted as a stray
        // control character into the input line.
        Key::Tab | Key::Left | Key::Right | Key::Delete | Key::Ctrl(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_characters_pass_through() {
        assert_eq!(bind(Key::Char('a')), Some(Input::Char('a')));
        assert_eq!(bind(Key::Char('日')), Some(Input::Char('日')));
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit() {
        assert_eq!(bind(Key::Ctrl('c')), Some(Input::Quit));
        assert_eq!(bind(Key::Ctrl('d')), Some(Input::Quit));
    }

    /// The help text advertises Ctrl-X, so it has to produce an interrupt.
    #[test]
    fn ctrl_x_interrupts() {
        assert_eq!(bind(Key::Ctrl('x')), Some(Input::Interrupt));
    }

    /// Ctrl-C must remain quit. Binding it to cancel-the-turn would leave no way
    /// out of the program, since raw mode means the kernel will not do it for us.
    #[test]
    fn ctrl_c_is_not_an_interrupt() {
        assert_ne!(bind(Key::Ctrl('c')), Some(Input::Interrupt));
    }

    #[test]
    fn navigation_keys_map_over() {
        for (key, expected) in [
            (Key::Up, Input::Up),
            (Key::Down, Input::Down),
            (Key::PageUp, Input::PageUp),
            (Key::PageDown, Input::PageDown),
            (Key::Home, Input::Home),
            (Key::End, Input::End),
            (Key::Enter, Input::Enter),
            (Key::Backspace, Input::Backspace),
        ] {
            assert_eq!(bind(key.clone()), Some(expected), "{key:?}");
        }
    }

    #[test]
    fn escape_follows_the_newest_output() {
        assert_eq!(bind(Key::Escape), Some(Input::End));
    }

    #[test]
    fn paste_arrives_whole() {
        assert_eq!(
            bind(Key::Paste("two words".into())),
            Some(Input::Paste("two words".into())),
        );
    }

    /// An unbound control chord must be dropped, not inserted as a literal
    /// control character into the input line.
    #[test]
    fn unbound_keys_are_dropped() {
        for key in [Key::Ctrl('q'), Key::Ctrl('z'), Key::Tab, Key::Delete] {
            assert_eq!(bind(key.clone()), None, "{key:?} should be unbound");
        }
    }

    /// The menu needs a key, or model switching is unreachable.
    #[test]
    fn ctrl_t_opens_the_menu() {
        assert_eq!(bind(Key::Ctrl('t')), Some(Input::Menu));
    }

    #[test]
    fn bound_control_keys_scroll() {
        assert_eq!(bind(Key::Ctrl('u')), Some(Input::PageUp));
        assert_eq!(bind(Key::Ctrl('g')), Some(Input::Home));
    }

    /// Every key variant must be handled — a new one added to the decoder
    /// without a binding here should be a deliberate `None`, never a panic.
    #[test]
    fn no_key_panics() {
        let all = [
            Key::Char('x'), Key::Enter, Key::Backspace, Key::Tab, Key::Escape,
            Key::Up, Key::Down, Key::Right, Key::Left, Key::Home, Key::End,
            Key::PageUp, Key::PageDown, Key::Delete, Key::Ctrl('a'),
            Key::Paste(String::new()),
        ];
        for key in all {
            let _ = bind(key);
        }
    }
}
