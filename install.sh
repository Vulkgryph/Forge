#!/usr/bin/env bash
# Native Linux source installer. Builds this checkout; never pulls source.
set -euo pipefail

repo="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
component=both
yes=false
shortcuts=true
prerequisites=true
prefix="${HOME}/.local"
usage() {
    cat <<'EOF'
Usage: bash install.sh [--yes] [--component both|ide|terminal|agent]
                      [--no-shortcuts] [--skip-prerequisites] [--prefix PATH]

Installs missing dependencies on Ubuntu/Debian, then builds and installs Forge.
Both interfaces and application-menu shortcuts are installed by default.
Run as your normal user; sudo is used only for system packages.
--yes accepts package installation without an apt confirmation prompt.
Other Linux distributions can use --skip-prerequisites after preparing tools.
EOF
}
while (($#)); do
    case "$1" in
        --yes) yes=true; shift ;;
        --no-shortcuts) shortcuts=false; shift ;;
        --skip-prerequisites) prerequisites=false; shift ;;
        --component|--prefix)
            (($# >= 2)) || { echo "Missing value for $1" >&2; exit 2; }
            if [[ "$1" == --component ]]; then component="$2"; else prefix="$2"; fi
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
case "$component" in both|ide|terminal|agent) ;; *) echo 'Invalid component' >&2; exit 2 ;; esac
[[ "$(uname -s)" == Linux ]] || { echo 'This installer requires Linux. See WINDOWS.md or forge-agent/install.sh for other systems.' >&2; exit 1; }
[[ "$(uname -m)" == x86_64 ]] || { echo 'This installer currently supports Linux x86-64 only.' >&2; exit 1; }
[[ "$prefix" == /* && "$prefix" != *$'\n'* ]] || { echo '--prefix must be an absolute path without newlines.' >&2; exit 2; }

if $prerequisites; then
    # shellcheck source=/dev/null
    source /etc/os-release
    case "${ID:-}" in
        ubuntu|debian) ;;
        *) echo 'Automatic dependencies currently support Ubuntu/Debian. See LINUX.md for manual prerequisites.' >&2; exit 1 ;;
    esac
    packages=(ca-certificates curl git build-essential pkg-config cmake perl nasm python3 ripgrep libssl-dev)
    if [[ "$component" == both || "$component" == ide ]]; then
        packages+=(libx11-dev libxcursor-dev libxi-dev libxrandr-dev libxkbcommon-dev libxkbcommon-x11-dev
            libwayland-dev libgl1-mesa-dev libegl1-mesa-dev mesa-vulkan-drivers
            xdg-utils desktop-file-utils)
    fi
    missing=()
    for package in "${packages[@]}"; do
        if [[ "$(dpkg-query -W -f='${Status}' "$package" 2>/dev/null || true)" != 'install ok installed' ]]; then
            missing+=("$package")
        fi
    done
    if ((${#missing[@]})); then
        admin=()
        if ((EUID != 0)); then
            command -v sudo >/dev/null || { echo 'sudo is missing. Ask your administrator to install the packages listed in LINUX.md.' >&2; exit 1; }
            admin=(sudo)
        fi
        echo "Installing system dependencies: ${missing[*]}"
        "${admin[@]}" apt-get update
        apt_options=()
        $yes && apt_options=(-y)
        "${admin[@]}" apt-get install "${apt_options[@]}" "${missing[@]}"
    fi
fi

export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
if $prerequisites; then
    if ! command -v rustup >/dev/null; then
        installer="$(mktemp)"
        trap 'rm -f -- "$installer"' EXIT
        curl --proto '=https' --tlsv1.2 --fail --location https://sh.rustup.rs -o "$installer"
        # rustup's official bootstrap script downloads and verifies its components.
        sh "$installer" -y --profile minimal --default-toolchain stable --no-modify-path
        rm -f -- "$installer"
        trap - EXIT
    fi
    rustup toolchain install stable --profile minimal
fi
for tool in cargo rustup cc pkg-config cmake perl nasm python3 rg git; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool (see LINUX.md)" >&2; exit 1; }
done

packages=(-p forge-agent)
sources=(forge-agent)
names=(forge-agent)
if [[ "$component" == both || "$component" == terminal ]]; then
    packages+=(-p forge-tui-rs); sources+=(forge-tui-rs); names+=(forge)
fi
if [[ "$component" == both || "$component" == ide ]]; then
    packages+=(-p forge-ide); sources+=(forge-ide); names+=(forge-ide)
fi
build="$repo/target/linux-installer"
RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-D warnings" cargo +stable build --locked --release \
    --manifest-path "$repo/Cargo.toml" --target-dir "$build" \
    --target x86_64-unknown-linux-gnu "${packages[@]}"
for source in "${sources[@]}"; do "$build/x86_64-unknown-linux-gnu/release/$source" --version; done

destination="$prefix/share/forge"
mkdir -p "$destination/bin" "$prefix/bin"
# Build and validate all binaries before replacing any installed executable.
# Rename preserves running Unix processes and avoids ETXTBSY on reinstall.
for i in "${!sources[@]}"; do
    staged="$(mktemp "$destination/bin/.${names[$i]}.XXXXXX")"
    install -m 755 "$build/x86_64-unknown-linux-gnu/release/${sources[$i]}" "$staged"
    mv -f -- "$staged" "$destination/bin/${names[$i]}"
done
cp -- "$repo/LICENSE" "$repo/NOTICE" "$destination/"
cp -R -- "$repo/licenses" "$destination/"
cp -- "$repo/forge-ide/assets/forge-ide.png" "$destination/forge.png"
git -C "$repo" rev-parse HEAD > "$destination/version" 2>/dev/null || echo unknown > "$destination/version"

python3 - "$prefix" "$component" "$shortcuts" <<'PY'
import os, pathlib, shlex, sys
prefix, component, shortcuts = sys.argv[1:]
root = pathlib.Path(prefix)
bin_dir = str(root / 'bin')
# Standard login shells, plus interactive Bash/Zsh shells opened after install.
home = pathlib.Path.home()
shell = pathlib.Path(os.environ.get('SHELL', '/bin/bash')).name
startup_files = [home / '.profile']
if shell in ('bash', 'zsh'):
    startup_files.append(home / ('.bashrc' if shell == 'bash' else '.zshrc'))
path_line = 'export PATH=' + shlex.quote(bin_dir) + ':"$PATH"'
for startup in startup_files:
    existing = startup.read_text() if startup.exists() else ''
    if path_line not in existing:
        with startup.open('a') as f:
            f.write('\n# Forge installer: available in new shells.\ncase ":$PATH:" in\n    *:' +
                shlex.quote(bin_dir) + ':*) ;;\n    *) ' + path_line + ' ;;\nesac\n')
names = ['forge-agent'] + (['forge'] if component in ('both', 'terminal') else []) + (['forge-ide'] if component in ('both', 'ide') else [])
for name in names:
    binary = root / 'share/forge/bin' / name
    launcher = root / 'bin' / name
    # Replace old symlinks without opening their targets for writing.
    temporary = launcher.with_name('.' + name + '.new')
    if temporary.is_symlink():
        temporary.unlink()
    script = '#!/bin/sh\n'
    if name == 'forge-ide':
        script += '''# Some WSL/systemd sessions set XDG_RUNTIME_DIR without linking WSLg's socket.
if [ -n "${WSL_DISTRO_NAME:-}" ] && [ -n "${WAYLAND_DISPLAY:-}" ]; then
    case "$WAYLAND_DISPLAY" in
        /*) socket="$WAYLAND_DISPLAY" ;;
        *) socket="${XDG_RUNTIME_DIR:-}/$WAYLAND_DISPLAY" ;;
    esac
    if [ ! -S "$socket" ] && [ -S /mnt/wslg/runtime-dir/wayland-0 ]; then
        export WAYLAND_DISPLAY=/mnt/wslg/runtime-dir/wayland-0
    fi
fi
'''
    script += 'exec ' + shlex.quote(str(binary)) + ' "$@"\n'
    temporary.write_text(script)
    temporary.chmod(0o755)
    temporary.replace(launcher)
    if name == 'forge-agent':
        continue
    apps = pathlib.Path(os.environ.get('XDG_DATA_HOME', str(pathlib.Path.home()/'.local/share'))) / 'applications'
    desktop = apps / (name + '.desktop')
    if shortcuts == 'true':
        apps.mkdir(parents=True, exist_ok=True)
        def value(s):
            return str(s).replace('\\', '\\\\').replace('\n', '\\n').replace('\t', '\\t').replace('\r', '\\r')
        def argument(s):
            s = str(s).replace('%', '%%')
            for c in ('\\', '"', '`', '$'):
                s = s.replace(c, '\\' + c)
            return value('"' + s + '"')
        desktop.write_text('[Desktop Entry]\nType=Application\nName=' + ('Forge IDE' if name == 'forge-ide' else 'Forge Terminal') +
            '\nExec=' + argument(launcher) + '\nIcon=' + value(root/'share/forge/forge.png') +
            '\nTerminal=' + ('false' if name == 'forge-ide' else 'true') + '\nCategories=Development;IDE;\n')
    # Opt-out skips creating entries; existing user customizations are preserved.
PY
if $shortcuts && command -v update-desktop-database >/dev/null; then
    update-desktop-database "${XDG_DATA_HOME:-$HOME/.local/share}/applications" || true
fi
echo "Installed $component to $destination/bin"
echo 'Open a new shell to use Forge by command name, or launch it from the application menu.'
echo "Launch using $prefix/bin/forge-ide or $prefix/bin/forge (for the selected components)."
if [[ ":$PATH:" != *":$prefix/bin:"* ]]; then
    # shellcheck disable=SC2016 # Print a command for the user's next shell.
    printf 'To use command names in this shell: export PATH=%q:"$PATH"\n' "$prefix/bin"
fi
echo 'Configure your model in the IDE or ~/.config/forge/config.toml. No credentials were changed.'
