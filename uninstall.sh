#!/usr/bin/env bash
# Removes what install.sh installed, and nothing else.
#
# The hard part of an uninstaller is not deleting things; it is being certain
# that every path it deletes is one you created. `~/.local/bin` is a directory
# people keep their own scripts in — a `forge-rs` or a `forge-update` that the
# installer never wrote — so this removes a launcher only after reading it and
# confirming it points into our own share directory. Anything it cannot
# identify is listed and left alone.
#
# Your configuration and credentials are kept unless you ask for them to go.
# `~/.config/forge/config.toml` holds endpoints and keys; an uninstaller that
# takes those with it makes reinstalling cost you the setup, and most people
# removing a binary are not asking to be logged out. --purge removes them.
#
#   ./uninstall.sh                 # show what would go, then ask
#   ./uninstall.sh --yes           # no questions
#   ./uninstall.sh --dry-run       # print and change nothing
#   ./uninstall.sh --purge         # configuration and credentials too
#   ./uninstall.sh --prefix DIR    # if you installed somewhere else
set -uo pipefail

# Arrays are expanded with the ${a[@]+"${a[@]}"} form throughout. bash 3.2 —
# which is what macOS ships, and so what this actually runs under — treats
# "${a[@]}" on an empty array as an unbound variable and aborts. It surfaced
# only on the second run, once there was no PATH block left to remove: the
# first run found something in every list and never took the empty path.

prefix="${HOME}/.local"
assume_yes=false
dry_run=false
purge=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)  prefix="$2"; shift 2 ;;
        --prefix=*) prefix="${1#*=}"; shift ;;
        --yes|-y)  assume_yes=true; shift ;;
        --dry-run|-n) dry_run=true; shift ;;
        --purge)   purge=true; shift ;;
        -h|--help)
            sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
            exit 0 ;;
        *) printf 'uninstall.sh: unknown option %s\n' "$1" >&2; exit 2 ;;
    esac
done

share="$prefix/share/forge"
bin="$prefix/bin"
applications="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
config="$HOME/.config/forge"
legacy_config="$HOME/.config/sinter"

# Exactly the names the installers write. Not a glob: `forge*` in a directory of
# someone's own scripts is how an uninstaller takes something that was never
# ours. On the machine this was written on, `~/.local/bin` also held a
# `forge-rs` and a `forge-ts` belonging to somebody else.
NAMES=(forge-agent forge forge-ide)

# forge-update is installed too, by forge-agent/install.sh, and is the one with
# a different shape: a symlink to `update.sh` *inside the source checkout*
# rather than into our share directory. So it needs its own ownership rule — and
# only the link is removed. The checkout it points at is the repository someone
# cloned, which is not ours to delete.
UPDATER=forge-update

doomed=()      # paths to remove
unowned=()     # forge-shaped paths that are not ours, reported and kept
rc_edits=()    # "file:line-range" for the PATH block to remove

note() { printf '  %s\n' "$*"; }

# --- what is here, and is it ours? ------------------------------------------

# A launcher is ours if it is a script or symlink resolving into $share/bin.
# Read rather than assumed: install.sh writes a small `#!/bin/sh` wrapper, and
# earlier versions wrote a symlink, so both shapes exist in the wild.
for name in "${NAMES[@]}"; do
    launcher="$bin/$name"
    [[ -e "$launcher" || -L "$launcher" ]] || continue
    if [[ -L "$launcher" ]]; then
        target="$(readlink "$launcher" || true)"
        case "$target" in
            "$share"/*) doomed+=("$launcher") ;;
            *) unowned+=("$launcher -> $target") ;;
        esac
    elif grep -qF "share/forge/bin/$name" "$launcher" 2>/dev/null; then
        doomed+=("$launcher")
    else
        unowned+=("$launcher (does not reference $share/bin/$name)")
    fi
done

# The updater: ours if it is a link to a forge-agent/update.sh anywhere.
updater_path="$bin/$UPDATER"
if [[ -L "$updater_path" ]]; then
    updater_target="$(readlink "$updater_path" || true)"
    case "$updater_target" in
        */forge-agent/update.sh) doomed+=("$updater_path") ;;
        *) unowned+=("$updater_path -> $updater_target") ;;
    esac
elif [[ -e "$updater_path" ]]; then
    if grep -qF "forge-agent/update.sh" "$updater_path" 2>/dev/null; then
        doomed+=("$updater_path")
    else
        unowned+=("$updater_path (does not reference forge-agent/update.sh)")
    fi
fi

[[ -d "$share" ]] && doomed+=("$share")

for name in "${NAMES[@]}"; do
    entry="$applications/$name.desktop"
    # Ours if it launches one of our binaries. A desktop file someone wrote by
    # hand for the same name is theirs.
    if [[ -f "$entry" ]] && grep -qE "^Exec=.*(/)?(forge|forge-ide|forge-agent)( |$)" "$entry" 2>/dev/null; then
        doomed+=("$entry")
    elif [[ -f "$entry" ]]; then
        unowned+=("$entry (Exec= does not name a Forge binary)")
    fi
done

if [[ "$(uname -s)" == "Darwin" && -d "/Applications/Forge IDE.app" ]]; then
    doomed+=("/Applications/Forge IDE.app")
fi

# The installer appends a two-line block it labels. Found by that label, so a
# PATH line you wrote yourself is never touched.
for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.config/fish/config.fish"; do
    [[ -f "$rc" ]] || continue
    line="$(grep -nxF '# Added by Forge installer' "$rc" 2>/dev/null | head -1 | cut -d: -f1)"
    [[ -n "$line" ]] && rc_edits+=("$rc:$line")
done

if $purge; then
    [[ -d "$config" ]] && doomed+=("$config")
    [[ -d "$legacy_config" ]] && doomed+=("$legacy_config")
fi

# --- report ------------------------------------------------------------------

if [[ ${#doomed[@]} -eq 0 && ${#rc_edits[@]} -eq 0 ]]; then
    echo "Nothing to remove: no Forge installation found under $prefix."
    [[ ${#unowned[@]} -gt 0 ]] && {
        echo
        echo "Left alone (not installed by Forge):"
        for u in ${unowned[@]+"${unowned[@]}"}; do note "$u"; done
    }
    exit 0
fi

echo "Would remove:"
for d in ${doomed[@]+"${doomed[@]}"}; do note "$d"; done
for e in ${rc_edits[@]+"${rc_edits[@]}"}; do
    note "${e%:*}  (the 'Added by Forge installer' PATH block at line ${e##*:})"
done

if [[ ${#unowned[@]} -gt 0 ]]; then
    echo
    echo "Left alone — these look like Forge but were not installed by it:"
    for u in ${unowned[@]+"${unowned[@]}"}; do note "$u"; done
fi

if ! $purge; then
    echo
    if [[ -d "$config" ]]; then
        echo "Keeping your configuration and credentials:"
        note "$config"
        note "(pass --purge to remove these as well)"
    fi
fi

echo
echo "Left in place deliberately:"
note "The source checkout you installed from — forge-update only links to it."

if [[ "$(uname -s)" == "Darwin" ]]; then
    echo
    echo "Not removable from a script:"
    note "A Dock entry, if you pinned one — drag it off the Dock."
    note "Folder-access permissions macOS granted the app, under"
    note "System Settings > Privacy & Security > Files and Folders."
fi

if $dry_run; then
    echo
    echo "--dry-run: nothing was changed."
    exit 0
fi

if ! $assume_yes; then
    echo
    printf 'Remove these? [y/N] '
    read -r reply < /dev/tty || reply=""
    case "$reply" in
        y|Y|yes|YES) ;;
        *) echo "Nothing was changed."; exit 1 ;;
    esac
fi

# --- remove ------------------------------------------------------------------

failed=0
for d in ${doomed[@]+"${doomed[@]}"}; do
    # By exact path. A `find`-based sweep matches parents and siblings, and that
    # has cost real data here before.
    if rm -rf -- "$d"; then
        echo "removed  $d"
    else
        echo "FAILED   $d" >&2
        failed=1
    fi
done

for e in ${rc_edits[@]+"${rc_edits[@]}"}; do
    rc="${e%:*}"; line="${e##*:}"
    # The block is the label plus the one line after it, and a blank line
    # before. Removed by position from the label rather than by pattern, so a
    # PATH line of your own that happens to look similar is never matched.
    #
    # Via a temporary file and `mv`: an in-place edit that fails midway leaves
    # a shell with a truncated startup file, which is a worse outcome than
    # leaving the block in place.
    tmp="$(mktemp "${rc}.uninstall.XXXXXX")" || { failed=1; continue; }
    if awk -v start="$line" 'NR==start-1 && $0=="" {next} NR>=start && NR<=start+1 {next} {print}' \
            "$rc" > "$tmp" 2>/dev/null; then
        if mv -f -- "$tmp" "$rc"; then
            echo "edited   $rc (removed the installer's PATH block)"
        else
            rm -f -- "$tmp"; echo "FAILED   $rc" >&2; failed=1
        fi
    else
        rm -f -- "$tmp"; echo "FAILED   $rc" >&2; failed=1
    fi
done

# The desktop database still lists entries whose files are gone until it is
# told. Best-effort: its absence is not a failure to uninstall.
if [[ -d "$applications" ]] && command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$applications" >/dev/null 2>&1 || true
fi

echo
if [[ "$failed" -eq 0 ]]; then
    echo "Forge has been removed."
else
    echo "Some paths could not be removed; see FAILED above." >&2
fi
if ! $purge && [[ -d "$config" ]]; then
    echo "Your configuration is still at $config."
fi
exit "$failed"
