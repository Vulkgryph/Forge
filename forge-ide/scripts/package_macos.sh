#!/bin/bash
# Builds a release .app bundle for Forge IDE: Contents/MacOS binary,
# generated .icns and Info.plist. No third-party runtime is bundled.
# Does not sign or notarize - see the .dmg release checklist for those steps.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_NAME="Forge IDE"
# Reverse-DNS from vulkgryph.com, matching the Developer ID this is signed
# with. It was com.windingcreek.forge-ide, which no longer names anything.
#
# Changing it is not free and this is the last cheap moment to do it: macOS
# ties a folder-access grant to the signed identity, so every installation
# re-asks for the folders it already had, and a Dock entry pinned under the old
# identifier stops resolving. Both cost one re-approval, once, before anyone
# else has installed this.
BUNDLE_ID="com.vulkgryph.forge-ide"
VERSION=$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
# forge-ide is a member of the monorepo's shared workspace, not its own
# workspace root - build output lands one level up, at the workspace root's
# target/, not a local ide/target/.
BUILD_DIR="../target/release"
OUT_DIR="../target/dist"
APP="$OUT_DIR/$APP_NAME.app"

# Keep the machine this was built on out of what gets published.
#
# `strip = true` in the release profile does not do it. Every panic site —
# `panic!`, `unwrap`, `expect`, a slice index — stores its source location as a
# string literal in .rodata for `core::panic::Location` to point at. That is
# program data, not debug info, so stripping does not touch it, and a build from
# a checkout under a home directory ships the absolute path of every dependency
# file that can panic. The 0.6.0 disk image carries about nine hundred of them
# across its three binaries; `strings` is all it takes to read them.
#
# Set here rather than in Cargo.toml because the profile key for it
# (`trim-paths`) is still unstable as of Cargo 1.97, and rustflags cannot be
# scoped to a profile — putting these in .cargo/config.toml would remap debug
# builds too and stop a debugger finding dependency sources.
#
# Appended, not assigned: RUSTFLAGS is one string that replaces rather than
# merges, so overwriting an inherited value (CI sets `-D warnings`) would
# quietly change what is being built.
HOME_DIR="${HOME%/}"
WORKSPACE="$(cd .. && pwd)"
# The catch-all goes FIRST because rustc applies the last matching prefix, not
# the first — it walks the list in reverse. So this is the fallback: anything
# under the home directory that the specific rules below do not name still has
# the username replaced, and a dependency vendored or patched from somewhere
# unanticipated cannot leak one just by not having been thought of.
REMAP=" --remap-path-prefix=$HOME_DIR/=home/"
# Then the specific cases, which override it to something a person reading a
# panic report can act on: `crates/serde-1.0/src/de.rs` says which dependency
# and which line, and names nobody.
REMAP="$REMAP --remap-path-prefix=$HOME_DIR/.cargo/registry/src/=crates/"
REMAP="$REMAP --remap-path-prefix=$HOME_DIR/.rustup/toolchains/=rust/"
REMAP="$REMAP --remap-path-prefix=$WORKSPACE/=./"
export RUSTFLAGS="${RUSTFLAGS:-}$REMAP"

echo "==> Building release binary"
cargo build --release

echo "==> Building forge-agent (bundled so the agent panel works standalone)"
cargo build --release -p forge-agent

# forge-server doubles as the *local* pty-host daemon: `ptyhost.rs` looks for it
# next to the forge-ide binary and spawns it as `forge-server --listen <socket>`.
# Without it in the bundle that lookup fails and every terminal silently falls
# back to a directly-owned PTY, which dies with the process — so terminals do not
# survive Reload Window. (Same binary, cross-compiled to musl, is what gets
# uploaded to remote hosts for SSH workspaces.)
echo "==> Building forge-server (local pty-host daemon; also the SSH remote agent)"
cargo build --release -p forge-server

# The terminal client. Not part of the bundle — it is installed separately, to
# ~/.local/share/forge/bin/forge — but it is built here so it can never be
# stale relative to the agent it spawns.
#
# It was omitted, and the omission shipped: a release binary was installed
# carrying the commit hash of the build before it, because packaging rebuilt
# everything except this and the stale artefact was copied out. A product that
# is released is a product that is built by the release script, even when it
# travels by a different route.
echo "==> Building forge-tui-rs (the terminal client; installed separately)"
cargo build --release -p forge-tui-rs

# The remote half of SSH workspaces. forge-ide uploads this to the machine you
# connect to, so it has to be a Linux binary and it has to travel inside the
# app — a launched .app has / for a working directory, so nothing relative to
# the checkout is reachable, and remote development simply could not work from
# an installed build without it.
#
# Not fatal when the cross-compiler is absent: the rest of the app is
# unaffected, CI has no musl toolchain, and a bundle built without it says so
# when a remote workspace is attempted rather than failing to build here.
# forge-agent goes too: the agent runs on the machine you are working on, so
# every tool call is local to it rather than a round trip back here.
REMOTE_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"
REMOTE_CRATES="forge-server forge-agent"
for target in $REMOTE_TARGETS; do
  arch="${target%%-*}"
  for crate in $REMOTE_CRATES; do
    if cargo build --release -p "$crate" --target "$target" 2>/dev/null; then
      echo "==> Built remote $crate for $arch"
    else
      echo "!!! No Linux/$arch $crate — remote development will be limited in this"
      echo "    bundle. Needs the musl cross-linker: brew install FiloSottile/musl-cross/musl-cross"
    fi
  done
done

# The remapping above is a compile flag, and a compile flag that stops working
# fails silently — the build succeeds and the binary ships the paths anyway.
# That is exactly how 0.6.0 went out, so the flags are not trusted: the binaries
# are read back and packaging stops before anything is signed or published.
echo "==> Checking no binary names the build machine"
HOME_USER="$(basename "$HOME_DIR")"
NAMES=(-e "$HOME_DIR")
# A short home-directory name ("dev", "ci") would match far too much to mean
# anything; the path above still covers it.
[ "${#HOME_USER}" -gt 3 ] && NAMES+=(-e "$HOME_USER")
[ "$(id -un)" != "$HOME_USER" ] && NAMES+=(-e "$(id -un)")
leaks=0
check_anonymous() {
    local f="$1" found
    [ -f "$f" ] || return 0
    found=$(LC_ALL=C strings -n 6 "$f" 2>/dev/null | grep -F "${NAMES[@]}" | sort -u || true)
    [ -z "$found" ] && return 0
    echo "!!! $f names the build machine:"
    printf '%s\n' "$found" | head -3 | sed 's/^/        /'
    leaks=$((leaks + 1))
}
for exe in forge-ide forge-agent forge-server forge-tui-rs; do
    check_anonymous "$BUILD_DIR/$exe"
done
for target in $REMOTE_TARGETS; do
    for crate in $REMOTE_CRATES; do
        check_anonymous "../target/$target/release/$crate"
    done
done
if [ "$leaks" != "0" ]; then
    echo
    echo "!!! $leaks binary/binaries would publish the path of the machine that"
    echo "    built them. Nothing has been signed. Panic locations are string"
    echo "    literals that \`strip\` does not remove — check the"
    echo "    --remap-path-prefix flags above still match this machine's layout"
    echo "    (HOME=$HOME_DIR), then build again."
    exit 1
fi
echo "    clean"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# Unlink before copying, never overwrite in place.
#
# macOS pages executable code in lazily and checks each page against the
# signature recorded when the file was mapped. Writing over a binary that a
# process is currently running changes the file under that mapping, and the next
# page it faults in fails the check: the kernel kills it with
# "CODESIGNING / Invalid Page". Packaging while a window is open would take that
# window down — and the crash names a code-signing fault, which reads like a
# signing bug rather than a file that moved under it.
#
# Removing the path first leaves the running process holding the old, now
# unlinked inode. It keeps running from it happily and the new build lands
# beside it.
for exe in forge-ide forge-agent forge-server; do
    rm -f "$APP/Contents/MacOS/$exe"
    cp "$BUILD_DIR/$exe" "$APP/Contents/MacOS/$exe"
done
# Resources, not MacOS: these are Linux ELF binaries for another machine, not
# executables of this app. `local_server_binary` looks for them by this name.
for target in $REMOTE_TARGETS; do
  arch="${target%%-*}"
  for crate in $REMOTE_CRATES; do
    remote="../target/$target/release/$crate"
    if [ -f "$remote" ]; then
      cp "$remote" "$APP/Contents/Resources/$crate-$arch"
    fi
  done
done
# No third-party runtime is bundled. The renderer is wgpu, which targets
# Apple's own Metal framework — already present on every Mac.

echo "==> Generating icon"
ICONSET="$OUT_DIR/AppIcon.iconset"
rm -rf "$ICONSET"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z $size $size Forge.png --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double Forge.png --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"
rm -rf "$ICONSET"

echo "==> Writing Info.plist"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleExecutable</key>
    <string>forge-ide</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon.icns</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
PLIST

# ── Sign ──────────────────────────────────────────────────────────────────────
# macOS ties a folder's permission grant to the application's code signature, and
# `cargo` leaves the binary ad-hoc "linker-signed" under an identifier derived
# from its own hash — a different identifier on every build. So every rebuild
# looked like a different application: the folders you had already approved were
# asked for again, and the Dock could not match its tile to what was running.
#
# Signing with the Developer ID makes the requirement identity-and-team based, so
# it survives rebuilds and the grants stick. Falls back to ad-hoc with a fixed
# identifier where that certificate is absent (CI, another machine): still not
# stable across rebuilds — nothing ad-hoc can be — but at least the app claims one
# consistent name instead of a hash.
SIGN_ID="${FORGE_SIGN_ID:-Developer ID Application: Vulkgryph LLC (W5DSR5XA65)}"

if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$SIGN_ID"; then
    echo "==> Signing with $SIGN_ID"
    # --deep is deprecated for distribution but right here: the bundled
    # forge-agent and forge-server are nested executables and have to be signed
    # too, innermost first, or the outer signature is invalid.
    codesign --force --deep --options runtime --timestamp \
             --identifier "$BUNDLE_ID" \
             --sign "$SIGN_ID" "$APP"
    codesign --verify --deep --strict "$APP"
else
    echo "==> No Developer ID in the keychain; ad-hoc signing"
    echo "    Folder permissions will be asked for again after each rebuild."
    codesign --force --deep --identifier "$BUNDLE_ID" --sign - "$APP"
fi

echo "==> Signed as: $(codesign -dv "$APP" 2>&1 | grep '^Identifier=' || echo unknown)"

echo "==> Done: $APP"
echo "==> Terminal client: $BUILD_DIR/forge-tui-rs"
echo "    Install it with:  rm -f ~/.local/share/forge/bin/forge &&"
echo "                      cp $BUILD_DIR/forge-tui-rs ~/.local/share/forge/bin/forge &&"
echo "                      codesign --force --options runtime --timestamp \\"
echo "                        --identifier com.vulkgryph.forge --sign \"$SIGN_ID\" \\"
echo "                        ~/.local/share/forge/bin/forge"
echo "    (rm first: overwriting a signed binary in place SIGKILLs any process running it.)"
