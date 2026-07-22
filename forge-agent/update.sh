#!/usr/bin/env bash
set -euo pipefail

# Forge updater — fast-forwards the source checkout, rebuilds, and reinstalls.
# Usage:
#   ./update.sh
#   ./update.sh --no-pull
#   ./update.sh --branch main

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
RESET='\033[0m'

info()  { echo -e "${BOLD}==>${RESET} $1"; }
warn()  { echo -e "${YELLOW}warning:${RESET} $1"; }
error() { echo -e "${RED}error:${RESET} $1" >&2; exit 1; }
ok()    { echo -e "${GREEN}✓${RESET} $1"; }

# install.sh installs this as a symlink at ~/.local/bin/forge-update —
# ${BASH_SOURCE[0]} doesn't auto-resolve symlinks, so a plain
# dirname/BASH_SOURCE would give ~/.local/bin instead of the real checkout
# (a real, pre-existing bug: this presumably went unnoticed because it was
# usually run as ./update.sh from inside the checkout, not via the
# installed command). Same symlink-resolution loop as forge-agent's own
# `forge` launcher script.
SOURCE="${BASH_SOURCE[0]}"
while [ -h "$SOURCE" ]; do
    DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)"
    SOURCE="$(readlink "$SOURCE")"
    [[ "$SOURCE" != /* ]] && SOURCE="$DIR/$SOURCE"
done
SCRIPT_DIR="$(cd -P "$(dirname "$SOURCE")" >/dev/null 2>&1 && pwd)"

# forge-agent lives inside the monorepo now (forge-agent/, forge-tui/,
# forge-ide/ as siblings) — the actual git root is one level up, resolved
# via git itself rather than assumed, so this still works if the monorepo's
# own directory ever gets renamed.
REPO_ROOT="$(cd "$SCRIPT_DIR" && git rev-parse --show-toplevel 2>/dev/null)" || REPO_ROOT="$SCRIPT_DIR"
PULL=true
BRANCH=""

usage() {
    cat <<EOF
Forge updater

Usage:
  ./update.sh [options]
  forge-update [options]

Options:
  --branch <name>  Check out and update the given branch before building
  --no-pull        Rebuild and reinstall the current checkout without git pull
  -h, --help       Show this help

The updater uses git pull --ff-only and refuses to pull over local changes.
Local config in ~/.config/forge is preserved and migrated by install.sh.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --branch)
            [[ $# -ge 2 ]] || error "--branch requires a branch name"
            BRANCH="$2"
            shift 2
            ;;
        --no-pull)
            PULL=false
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            error "Unknown option: $1. Run ./update.sh --help for valid options."
            ;;
    esac
done

cd "$REPO_ROOT"

[[ -f "$SCRIPT_DIR/install.sh" ]] || error "install.sh not found next to this script."
[[ -d "$REPO_ROOT/.git" ]] || error "This updater expects a git checkout of Forge."

info "Updating Forge in $REPO_ROOT"

if [[ "$PULL" == true ]]; then
    command -v git >/dev/null 2>&1 || error "git is required to update Forge"

    if [[ -n "$BRANCH" ]]; then
        info "Fetching branch $BRANCH..."
        git fetch origin "$BRANCH"

        if [[ -n "$(git status --porcelain)" ]]; then
            error "Local changes are present. Commit/stash them or run ./update.sh --no-pull."
        fi

        info "Checking out $BRANCH..."
        git checkout "$BRANCH"
    fi

    if [[ -n "$(git status --porcelain)" ]]; then
        warn "Local changes detected; skipping git pull. Run git status to inspect, or use a clean checkout."
    else
        current_branch="$(git rev-parse --abbrev-ref HEAD)"
        if [[ "$current_branch" == "HEAD" ]]; then
            warn "Detached HEAD; skipping git pull. Use --branch <name> to switch branches."
        else
            info "Pulling latest changes for $current_branch..."
            git pull --ff-only
            ok "Source updated"
        fi
    fi
else
    warn "Skipping git pull; rebuilding current checkout"
fi

info "Building and reinstalling Forge..."
"$SCRIPT_DIR/install.sh"

ok "Forge update complete"
echo ""
echo "Run: forge"
