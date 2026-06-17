#!/usr/bin/env bash
# Forge bootstrap installer
#
# Quick install — runs preflight, clones, and invokes install.sh.
#
#   curl -fsSL https://raw.githubusercontent.com/Vulkgryph/Forge/main/bootstrap.sh | bash
#
# Environment overrides:
#   FORGE_REPO    repository URL (default: github.com/Vulkgryph/Forge)
#   FORGE_DEST    clone destination (default: ~/forge)
#   FORGE_BRANCH  branch to check out (default: main)
#
# The destination is the load-bearing source checkout — install.sh symlinks
# binaries from this directory into ~/.local/bin. Don't delete it after install.

set -euo pipefail

REPO="${FORGE_REPO:-https://github.com/Vulkgryph/Forge.git}"
DEST="${FORGE_DEST:-$HOME/forge}"
BRANCH="${FORGE_BRANCH:-main}"

BOLD='\033[1m'; GREEN='\033[0;32m'; YELLOW='\033[0;33m'; RED='\033[0;31m'; RESET='\033[0m'
info()  { echo -e "${BOLD}==>${RESET} $1"; }
warn()  { echo -e "${YELLOW}warning:${RESET} $1"; }
error() { echo -e "${RED}error:${RESET} $1" >&2; exit 1; }
ok()    { echo -e "${GREEN}✓${RESET} $1"; }

# -------------------------------------------------------------------
# 1. Detect platform
# -------------------------------------------------------------------
OS="$(uname -s)"
case "$OS" in
    Darwin|Linux) ;;
    *) error "Unsupported OS: $OS (Forge supports macOS and Linux only)" ;;
esac
info "Detected $OS $(uname -m)"

# -------------------------------------------------------------------
# 2. Preflight system packages
# -------------------------------------------------------------------
if [[ "$OS" == "Linux" ]]; then
    MISSING=()
    command -v git    >/dev/null 2>&1 || MISSING+=(git)
    command -v unzip  >/dev/null 2>&1 || MISSING+=(unzip)        # bun installer
    command -v cc     >/dev/null 2>&1 || MISSING+=(build-essential)  # cargo

    if [[ ${#MISSING[@]} -gt 0 ]]; then
        if command -v apt-get >/dev/null 2>&1; then
            info "Installing system packages: ${MISSING[*]}"

            SUDO=""
            if [[ "$EUID" -ne 0 ]]; then
                if command -v sudo >/dev/null 2>&1; then
                    SUDO="sudo"
                else
                    error "Need root to install ${MISSING[*]}. Re-run as root, or install sudo."
                fi
            fi

            $SUDO apt-get update -qq
            $SUDO DEBIAN_FRONTEND=noninteractive apt-get install -y "${MISSING[@]}"
            ok "System packages installed"
        else
            error "Missing required packages: ${MISSING[*]}\n  Install them via your package manager (apt, dnf, pacman, etc.), then re-run."
        fi
    else
        ok "Preflight system packages already present"
    fi
fi

if [[ "$OS" == "Darwin" ]]; then
    if ! xcode-select -p >/dev/null 2>&1; then
        error "Xcode Command Line Tools not installed.\n  Run: xcode-select --install\n  Wait for the install to complete, then re-run this script."
    fi
    ok "Xcode Command Line Tools present"
fi

# -------------------------------------------------------------------
# 3. Clone or update source
# -------------------------------------------------------------------
if [[ -d "$DEST" ]]; then
    if [[ -d "$DEST/.git" ]]; then
        info "Existing checkout at $DEST — pulling latest"
        git -C "$DEST" fetch --quiet origin "$BRANCH"
        git -C "$DEST" checkout --quiet "$BRANCH"
        git -C "$DEST" pull --ff-only --quiet
        ok "Updated $DEST"
    else
        error "$DEST exists and is not a git checkout.\n  Move it aside, or set FORGE_DEST to a different path."
    fi
else
    info "Cloning $REPO → $DEST"
    git clone --quiet --branch "$BRANCH" "$REPO" "$DEST"
    ok "Cloned to $DEST"
fi

# -------------------------------------------------------------------
# 4. Hand off to install.sh
# -------------------------------------------------------------------
cd "$DEST"
chmod +x install.sh update.sh forge

info "Running install.sh"
echo

# When this bootstrap is invoked via `curl ... | bash`, our stdin is the curl
# pipe (consumed by bash) and reads from it would EOF immediately. install.sh
# has an interactive wizard for first-time config, so redirect its stdin to the
# controlling terminal. The probe `true </dev/tty` actually opens /dev/tty —
# `[[ -e /dev/tty ]]` only checks that the device file exists, which is always
# true even when no controlling terminal is attached.
if (true </dev/tty) 2>/dev/null; then
    bash install.sh </dev/tty
else
    echo
    warn "No interactive terminal detected (running without a controlling TTY)."
    warn "Source is ready at $DEST, but the config wizard requires a terminal."
    warn "To finish the install, run from a real shell:"
    warn "  cd $DEST && ./install.sh"
    exit 0
fi
