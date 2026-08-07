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

        // Ctrl-T expands and collapses reasoning. The TypeScript UI prints
        // "(ctrl+t to expand)" in its own transcript, so the binding is part of
        // the design rather than a free choice.
        Key::Ctrl('n') => Input::Newline,
        Key::Ctrl('t') => Input::ToggleReasoning,
        // The menu takes Ctrl-O, since Ctrl-T is spoken for.
        Key::Ctrl('o') => Input::Menu,
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
        Key::Delete => Input::Delete,
        Key::Left => Input::Left,
        Key::Right => Input::Right,

        // Readline's line-start and line-end, which work in every terminal —
        // unlike modified arrows, which Forge IDE's own terminal does not send.
        Key::Ctrl('a') => Input::Home,
        Key::Ctrl('e') => Input::LineEnd,
        Key::Ctrl('w') => Input::DeleteWord,

        // Escape is the instinctive "stop that", and the TypeScript client bound
        // it to cancelling a running turn. It is its own input rather than being
        // folded in with End, which has never meant cancel.
        Key::Escape => Input::Escape,
        // End means "stop scrolling and follow the newest
        // output", which is also how a dialog reads a dismissal.
        Key::End => Input::End,

        // Unbound: Tab, arrows we do not use for navigation, and any control
        // chord not listed above. Dropped rather than inserted as a stray
        // control character into the input line.
        // Tab completes a slash command.
        Key::Tab => Input::Complete,
        Key::BackTab => Input::CyclePermission,

        Key::Ctrl(_) => return None,
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
        assert_eq!(bind(Key::Escape), Some(Input::Escape));
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
        // Delete used to be here; it removes the grapheme at the caret now.
        for key in [Key::Ctrl('q'), Key::Ctrl('z'), Key::Ctrl('p')] {
            assert_eq!(bind(key.clone()), None, "{key:?} should be unbound");
        }
    }

    /// Ctrl-T is expand/collapse reasoning, because the TypeScript UI says so in
    /// its own transcript text. Taking it for the menu broke that muscle memory.
    #[test]
    fn ctrl_t_toggles_reasoning_not_the_menu() {
        assert_eq!(bind(Key::Ctrl('t')), Some(Input::ToggleReasoning));
    }

    /// The menu still needs a key, or model switching is unreachable.
    /// The placeholder advertises Ctrl+N for a newline, so it has to produce one.
    #[test]
    fn ctrl_n_inserts_a_newline() {
        assert_eq!(bind(Key::Ctrl('n')), Some(Input::Newline));
    }

    #[test]
    fn tab_completes_a_command() {
        assert_eq!(bind(Key::Tab), Some(Input::Complete));
    }

    #[test]
    fn ctrl_o_opens_the_menu() {
        assert_eq!(bind(Key::Ctrl('o')), Some(Input::Menu));
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
    /// Every binding, driven by the bytes Forge IDE's terminal actually emits.
    ///
    /// The TUI runs inside that terminal, so a binding is only real if the
    /// emulator sends something the decoder recognises — and a key that is not
    /// translated is indistinguishable, from here, from a key that does nothing.
    /// Shift+Tab was exactly that: the emulator sent a plain Tab, so cycling
    /// permission modes completed a slash command instead.
    ///
    /// The other half of this contract is `shift_tab_sends_back_tab_not_a_plain_tab`
    /// and the ctrl-code tests in `forge-ide`, which pin what it emits.
    #[test]
    fn the_bindings_survive_forge_ides_terminal() {
        use crate::keys::Decoder;
        let cases: &[(&str, &[u8], Input)] = &[
            ("Ctrl-C",    &[0x03],                   Input::Quit),
            ("Ctrl-D",    &[0x04],                   Input::Quit),
            ("Ctrl-X",    &[0x18],                   Input::Interrupt),
            ("Ctrl-N",    &[0x0e],                   Input::Newline),
            ("Ctrl-T",    &[0x14],                   Input::ToggleReasoning),
            ("Ctrl-O",    &[0x0f],                   Input::Menu),
            ("Ctrl-U",    &[0x15],                   Input::PageUp),
            ("Ctrl-G",    &[0x07],                   Input::Home),
            ("Enter",     &[0x0d],                   Input::Enter),
            ("Backspace", &[0x7f],                   Input::Backspace),
            ("Up",        b"\x1b[A",                 Input::Up),
            ("Down",      b"\x1b[B",                 Input::Down),
            ("PageUp",    b"\x1b[5~",                Input::PageUp),
            ("PageDown",  b"\x1b[6~",                Input::PageDown),
            ("Home",      b"\x1b[H",                 Input::Home),
            ("End",       b"\x1b[F",                 Input::End),
            ("Tab",       &[0x09],                   Input::Complete),
            ("Shift-Tab", b"\x1b[Z",                 Input::CyclePermission),
        ];
        for (name, bytes, want) in cases {
            let mut d = Decoder::new();
            let keys = d.feed(bytes);
            let actions: Vec<Input> = keys.into_iter().filter_map(bind).collect();
            assert_eq!(actions, vec![want.clone()], "{name} did not arrive as {want:?}");
        }
    }

    /// Escape is the one that needs the timeout: the emulator sends a bare `ESC`,
    /// which has to be held in case a sequence follows it.
    #[test]
    fn escape_from_the_ide_arrives_after_the_timeout() {
        use crate::keys::Decoder;
        let mut d = Decoder::new();
        assert!(d.feed(&[0x1b]).is_empty(), "held, in case more follows");
        assert!(d.has_pending());
        let key = d.flush_pending_escape().expect("flushed once nothing followed");
        assert_eq!(bind(key), Some(Input::Escape));
    }

}
