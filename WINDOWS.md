# Local Windows installation

From this checkout, double-click **install.cmd**, or run in PowerShell:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\install.ps1
```

The default installs both interfaces and their shared agent. It builds the
current local source with the lockfile and warnings treated as errors; it never
fetches, pulls, switches branches, or changes the Git remote.

The first run needs internet access for Rust, Cargo dependencies, and any missing
build tools. Visual Studio C++ Build Tools may require Windows administrator
consent and a restart. CMake is reused from Visual Studio when available.
Portable Perl and NASM are downloaded from their publishers, checked against
pinned SHA-256 hashes, and unpacked under `%LOCALAPPDATA%\ForgeBuildTools`.
Their directories are added to the build process's PATH only. Git and Rust are
installed with WinGet if missing. Subsequent runs reuse the toolchain and cache.

The installer offers Start menu and desktop shortcuts for each selected interface,
using the Forge logo. Choices are remembered for subsequent installs. The initial
defaults are Start menu shortcuts enabled and desktop shortcuts disabled. Launch
**Forge IDE** from either location to open the editor without typing a command.
The running editor also uses the Forge logo for its window and taskbar icon,
and launches without a companion console window.

Alternatively, once installation succeeds, open a new PowerShell window:

```powershell
forge-ide         # desktop editor; also available in the Start menu
forge             # terminal interface
forge-agent --help
```

Configure the model through the editor's onboarding screen. The installer leaves
existing configuration and credentials untouched. The shared agent configuration
is `%USERPROFILE%\.config\forge\config.toml`.

## Options

```powershell
.\install.ps1 -Check                   # inspect base toolchain without installing
.\install.ps1 -PrerequisitesOnly       # prepare build tools without building Forge
.\install.ps1 -Component Terminal      # terminal interface and agent
.\install.ps1 -Component Ide           # editor and agent
.\install.ps1 -Component Agent         # headless agent only
.\install.ps1 -SkipPrerequisites       # use the already installed toolchain
.\install.ps1 -StartMenuShortcut Yes -DesktopShortcut Yes -NonInteractive
.\install.ps1 -InstallRoot 'D:\Apps\Forge' -NoPath -NoShortcut
```

Shortcut options accept `Ask`, `Yes`, or `No`. `-NonInteractive` uses saved choices
or the initial defaults unless explicitly overridden. Choosing `No` removes that
installation's existing shortcuts; `-NoShortcut` skips shortcut changes entirely.

This installer currently targets x64 Windows. Run the same command to rebuild
and reinstall local changes. `forge-agent\install.ps1` and
`forge-agent\update.ps1` forward to it; updating source is a separate Git operation.

Executables live together in a new directory under
`%LOCALAPPDATA%\Programs\Forge\releases` on each successful install. Launchers in
`bin` and enabled shortcuts select the new copy. Previous directories are
retained so an open Forge session can keep using its binaries. A failed build
does not replace the installed version. `install.json` records the active
installation. No binaries or configuration are written onto the handoff drive.

## Windows development status

This is a source installer for native Windows development, not a signed release
setup executable. The Windows terminal backend uses console input records and
VT output; the editor uses its default wgpu renderer and a direct ConPTY terminal
running PowerShell. Local editor terminals do not survive process restarts on
Windows: the persistent PTY daemon still uses Unix sockets. Remote Linux helpers
are not bundled by this installer. The macOS browser handoff remains macOS-only.

## Checks

```powershell
$env:RUSTFLAGS = '-D warnings'
cargo test --locked -p forge-agent-proto -p forge-search -p forge-tui-rs
cargo test --locked -p forge-tui-rs real_console_input_and_mode_restoration -- --ignored --nocapture
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\test_windows_installer.ps1
```

The console test must run in an attached Windows terminal. The installer test
uses isolated executable fixtures in a temporary path containing spaces. It
checks launchers, reinstalls, failed builds, shortcut targets, artwork, and opt-out
behavior without changing the user PATH,
Start menu, or real Forge installation.

The IDE suite also uses `gzip` as an independent compression encoder. With Git
for Windows installed in its default location, run it with:

```powershell
$env:PATH = "C:\Program Files\Git\usr\bin;$env:PATH"
cargo test --locked -p forge-ide
```

The named cause of this is fixed: the `is_zombie` test and the
`process_exists`/`is_zombie` helpers it exercises are `#[cfg(unix)]`-guarded
now (`forge-agent/src/agent/rewind.rs`). Whether the *whole* agent suite
compiles on Windows is still unverified, because nothing checks it: CI builds
`forge-agent` there and runs only its `resilience` target, not its unit tests.
So treat this as open until somebody runs `cargo test -p forge-agent` on a
Windows box — the remaining risk is another Unix-only test, not the one this
paragraph used to name.

Validated locally on 2026-09-25: the release installer and all three installed
`--version` commands passed; the combined IDE/TUI/protocol/search run passed
1,320 tests, and the attached-console test passed separately. The installed IDE
opened a responsive window using DirectX 12 on an NVIDIA RTX 4090. The installed
TUI reached its prompt, accepted text, and exited cleanly with Ctrl-C. Model/API
requests and SSH sessions have not been exercised by these installation checks.

The Windows wgpu renderer uses a stronger font-coverage exponent (0.85 rather
than egui 0.29's 0.55) to reduce faint text fringes. This keeps grayscale
antialiasing; it does not add DirectWrite hinting or ClearType. Forge artwork is
area-filtered to its displayed pixel size, with separate bounded caches for the
activity icon and watermark. Image rectangles are aligned to physical pixels.
The changes were compared in actual 100% DPI Windows captures; alignment is
also tested at 125%, 150%, and 200%. The IDE suite passed 407 tests (8 ignored).

To capture an existing IDE window at its original resolution for comparison:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\capture_windows_ide.ps1 -ProcessId 1234 -OutputPath C:\Temp\forge.png
```

Use the actual Forge process ID and an existing output directory.

Settings has separate **Editor Font Size** and **Terminal Font Size** controls,
each with a live preview. Both apply immediately to their respective content;
menus and sidebars retain their own text sizes. The terminal previously ignored
its saved size and always rendered at 13; it now uses the selected size and
rebuilds its cached text layout when that changes.

Windows restart commands and the newer-build banner follow the active generation
in this installation's `install.json`. Older generation files remain intact for
running sessions. **Reload Window** refreshes state inside the current process;
use **Restart This Window** or **Restart Forge** to load a newly installed binary.
A plain `cargo build` updates Cargo's output directory; rerun `install.cmd` to
publish local changes to the desktop/Start menu installation.

When **Check for Updates** is enabled, GitHub release checks run at startup and
hourly while the IDE remains open. A newer published release shows a **View
Release** banner; this opens the release page rather than silently installing it.
Failed checks are logged in Output and preserve any update already discovered.
Installing a new local build is detected separately (within about 15 seconds),
with actions to restart one window or all windows into that installed build.
