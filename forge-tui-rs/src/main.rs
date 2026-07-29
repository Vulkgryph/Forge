// SPDX-License-Identifier: Apache-2.0
//! Spike binary: runs the TUI with a seeded transcript and no agent attached.
//!
//! Everything of substance lives in the library (see `lib.rs`); this is the thin
//! shell that owns the terminal and pumps events, kept small so the render loop
//! is readable in one screen.
//!
//! Run with `cargo run -p forge-tui-rs`.

use std::io::{self, Write};

use forge_tui_rs::app::{App, Outcome, Role};
use forge_tui_rs::screen::Screen;
use forge_tui_rs::{input, term};

fn main() -> io::Result<()> {
    // Everything below this line runs with the terminal in raw mode on the
    // alternate screen; the guard puts it back on every exit path, panics and
    // signals included.
    let _guard = term::Guard::new()?;

    let (cols, rows) = term::size();
    let mut screen = Screen::new(cols, rows);
    let mut app = App::new();
    seed(&mut app);

    let mut out = io::BufWriter::new(io::stdout());
    app.view(&mut screen);
    screen.flush(&mut out)?;

    loop {
        // Blocking: an idle session wakes for nothing and costs no CPU.
        let event = crossterm::event::read()?;

        if let crossterm::event::Event::Resize(cols, rows) = event {
            screen.resize(cols as usize, rows as usize);
        }

        let Some(decoded) = input::decode(event) else { continue };
        if app.update(decoded, &screen) == Outcome::Quit {
            break;
        }

        app.view(&mut screen);
        screen.flush(&mut out)?;
    }

    // Drop restores the terminal; flush first so nothing is left buffered.
    out.flush()?;
    Ok(())
}

/// Content that exercises the cases which broke the old renderer, so the
/// problems are visible on screen rather than only in tests.
fn seed(app: &mut App) {
    app.push(Role::System, "forge-tui spike — Ctrl-C to quit, ↑/↓ and PgUp/PgDn to scroll");
    app.push(
        Role::Agent,
        "# Rendering from scratch\n\
         \n\
         Every frame is diffed and written inside a **synchronized update**, so \
         nothing is ever presented half-drawn. Try resizing the window while \
         scrolled up.\n\
         \n\
         The cases that used to overlap:\n\
         \n\
         - wide text: 日本語のテキストが折り返されるところ\n\
         - emoji sequences: \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467} counts as two cells, not six\n\
         - variation selectors: \u{26A0}\u{FE0F} is two cells, \u{26A0} is one\n\
         - combining marks: cafe\u{301} is four cells\n\
         \n\
         Code is clipped rather than rewrapped, because rewrapping changes it:\n\
         \n\
         ```\n\
         fn cluster_width(cluster: &str) -> usize { /* one rule, used twice */ }\n\
         ```\n\
         \n\
         > Scroll up far enough and the rule above the prompt says how far behind \
         you are.",
    );
    for i in 1..=24 {
        app.push(Role::Agent, format!("filler line {i} — scroll to see the viewport window move"));
    }
}
