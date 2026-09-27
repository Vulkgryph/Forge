# Linux installation

From a Forge source checkout or extracted source archive, run as your normal user:

```bash
bash install.sh
```

The installer supports **Ubuntu/Debian x86-64**, installs missing system packages
using apt (sudo may request your password), installs Rust if absent, and builds
the agent, terminal interface, and IDE. Internet access and several GB of free
disk space are required. A minimal system needs Bash, apt, and either sudo or
administrator execution. No Git, Rust, C/C++ compiler, or graphics development
packages need to be installed beforehand when using an extracted source archive.
The script builds the current source with its lockfile; it never downloads newer
Forge source or changes the checkout.

```bash
bash install.sh --yes                  # accept apt package installation
bash install.sh --component terminal   # terminal and agent only
bash install.sh --component ide        # editor and agent only
bash install.sh --no-shortcuts         # skip creating application-menu entries
bash install.sh --prefix "$HOME/apps/forge"  # alternate installation prefix
```

By default the launchers are in `~/.local/bin`, with application-menu entries for
**Forge IDE** and **Forge Terminal**, using the Forge icon. A graphical desktop
session is needed for the IDE. Log out and back in if a desktop has not refreshed
its application menu. The terminal menu entry requires a desktop terminal emulator.
The installer adds its launcher directory to your login profile and Bash/Zsh
startup file. Open a new shell to use `forge` or `forge-ide` by name; it also prints
a PATH command for the current shell. Application-menu entries use absolute paths.

Executables live in `~/.local/share/forge/bin`. Rerun the script to rebuild and
replace them without overwriting running executable files. Configuration and
credentials are preserved. The older `forge-agent/install.sh` remains available
for its terminal-only installer and interactive provider configuration.

## WSL

WSL2 with WSLg can run both interfaces. The IDE launcher repairs a missing WSLg
Wayland socket reference for that process only, if WSL's real socket is available.
It does not change system environment files. WSL configurations exposing only
software Vulkan rendering can work, but do not establish native GPU performance.

## Other distributions

Automatic package installation is limited to Ubuntu/Debian. On other x86-64 Linux
distributions, install the equivalent packages below and rustup with a stable
toolchain, then use `bash install.sh --skip-prerequisites`:

- Git, curl, CA certificates, C/C++ compiler and make, pkg-config, CMake, Perl,
  NASM, Python 3, ripgrep, OpenSSL development files.
- For the IDE: X11, Xcursor, Xi, Xrandr, xkbcommon (including its X11 library), Wayland, EGL/OpenGL development
  files; a Vulkan driver (Mesa software rendering is sufficient for startup),
  xdg-utils and desktop-file-utils.

32-bit x86, ARM64, and other distribution package managers have not been validated
by this installer. A successful software-rendered GUI smoke test does not replace
testing graphics performance and desktop integration on physical Linux hardware.

## Validation

On September 27, 2026, the installer passed in a fresh minimal Ubuntu 24.04
x86-64 filesystem under WSL2. The test user began without Git, curl, Rust,
compilers, make, CMake, NASM, Python, pkg-config, or ripgrep. Only the base OS,
standard Ubuntu package repositories, and sudo access were prepared by the test
harness. Source files were supplied as an archive, with no build or Cargo cache.

One `bash install.sh --yes` run installed dependencies and both interfaces.
Installed launchers, PATH setup, desktop-entry validation, terminal startup and
exit with terminal restoration, and IDE startup with its agent and Bash shell
passed. The GUI used WSLg's display server and software Vulkan rendering; the
application and its libraries came from the isolated filesystem. The launcher
automatically handled the WSL Wayland socket mismatch. An initial exploratory
X11 run exposed a missing xkbcommon-X11 dependency; it was added before the final
clean run. Debian's package path is supported but has not had this clean test.

Packaging regression tests (also run in CI):

```bash
shellcheck install.sh scripts/test_linux_installer.sh
bash scripts/test_linux_installer.sh
```

The packaging test needs Python 3 and desktop-file-utils. It uses fixture
executables to check launchers, menu entries, icons, paths with spaces, failed
build preservation, reinstall, configuration preservation, PATH and shortcut
opt-out. The separate clean test described above built the actual application.
