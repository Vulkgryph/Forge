#!/usr/bin/env bash
# Packaging checks without downloading dependencies or compiling the workspace.
set -euo pipefail
repo="$(cd -- "$(dirname -- "$0")/.." && pwd)"
test_root="$(mktemp -d -t forge-linux-installer.XXXXXX)"
trap 'rm -rf -- "$test_root"' EXIT
mkdir -p "$test_root/source/forge-ide/assets" "$test_root/source/licenses" "$test_root/tools" "$test_root/home"
cp "$repo/install.sh" "$test_root/source/install.sh"
cp "$repo/forge-ide/assets/forge-ide.png" "$test_root/source/forge-ide/assets/"
touch "$test_root/source/LICENSE" "$test_root/source/NOTICE" "$test_root/source/Cargo.toml"
cat > "$test_root/tools/cargo" <<'EOF'
#!/usr/bin/env bash
set -eu
[[ "${FAKE_BUILD_FAIL:-0}" != 1 ]] || exit 42
locked=false
while (($#)); do
    case "$1" in
        --target-dir) output="$2"; shift 2 ;;
        --locked) locked=true; shift ;;
        *) shift ;;
    esac
done
$locked || exit 43
mkdir -p "$output/x86_64-unknown-linux-gnu/release"
for name in forge-agent forge-tui-rs forge-ide; do
    printf '#!/bin/sh\nprintf "fixture-%%s\\n" "${1:-started}"\n' > "$output/x86_64-unknown-linux-gnu/release/$name"
    chmod +x "$output/x86_64-unknown-linux-gnu/release/$name"
done
EOF
chmod +x "$test_root/tools/cargo"
# No real tools are executed except Python; discovery must also work on a host
# without a compiler. cargo is intentionally a fixture, not a build test.
for name in rustup cc pkg-config cmake perl nasm rg git; do
    printf '#!/bin/sh\nexit 0\n' > "$test_root/tools/$name"
    chmod +x "$test_root/tools/$name"
done
export HOME="$test_root/home"
export CARGO_HOME="$test_root/empty-cargo"
export XDG_DATA_HOME="$test_root/menu data"
export PATH="$test_root/tools:$PATH"
prefix="$test_root/install with spaces"
bash "$test_root/source/install.sh" --skip-prerequisites --prefix "$prefix" > "$test_root/install.log" 2>&1
for name in forge forge-agent forge-ide; do
    [[ "$("$prefix/bin/$name" --version)" == fixture---version ]]
done
desktop-file-validate "$XDG_DATA_HOME/applications/forge.desktop" "$XDG_DATA_HOME/applications/forge-ide.desktop"
python3 - "$prefix" "$XDG_DATA_HOME" <<'PY'
import configparser, pathlib, shlex, sys
prefix, data = map(pathlib.Path, sys.argv[1:])
for name in ('forge', 'forge-ide'):
    entry = configparser.ConfigParser(interpolation=None)
    entry.read(data / 'applications' / (name + '.desktop'))
    props = entry['Desktop Entry']
    assert shlex.split(props['Exec']) == [str(prefix/'bin'/name)]
    assert pathlib.Path(props['Icon']).is_file()
    assert props['Terminal'] == ('false' if name == 'forge-ide' else 'true')
PY
# A failed build must leave the installed application untouched.
before="$(sha256sum "$prefix/share/forge/bin/forge-ide")"
if FAKE_BUILD_FAIL=1 bash "$test_root/source/install.sh" --skip-prerequisites --prefix "$prefix" > "$test_root/failure.log" 2>&1; then
    echo 'Failed build was accepted' >&2; exit 1
fi
[[ "$(sha256sum "$prefix/share/forge/bin/forge-ide")" == "$before" ]]
# Reinstall must preserve personal configuration and replace old symlink launchers.
mkdir -p "$HOME/.config/forge"
printf 'keep me\n' > "$HOME/.config/forge/config.toml"
rm -- "$prefix/bin/forge"
ln -s "$prefix/share/forge/bin/forge" "$prefix/bin/forge"
bash "$test_root/source/install.sh" --skip-prerequisites --prefix "$prefix" > "$test_root/reinstall.log" 2>&1
[[ ! -L "$prefix/bin/forge" ]]
[[ "$(cat "$HOME/.config/forge/config.toml")" == 'keep me' ]]
[[ "$(grep -c '^# Forge installer:' "$HOME/.profile")" == 1 ]]
[[ "$(bash -c 'source "$HOME/.profile"; command -v forge')" == "$prefix/bin/forge" ]]
export XDG_DATA_HOME="$test_root/no-menu"
bash "$test_root/source/install.sh" --skip-prerequisites --component terminal --no-shortcuts --prefix "$test_root/terminal" > "$test_root/terminal.log" 2>&1
[[ -x "$test_root/terminal/bin/forge" && ! -e "$test_root/terminal/bin/forge-ide" && ! -e "$XDG_DATA_HOME/applications/forge.desktop" ]]
echo 'PASS: launchers, menu entries, icons, spaces, failed-build preservation, reinstall, config preservation and shortcut opt-out'
