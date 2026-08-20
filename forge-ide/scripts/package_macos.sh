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

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BUILD_DIR/forge-ide" "$APP/Contents/MacOS/forge-ide"
cp "$BUILD_DIR/forge-agent" "$APP/Contents/MacOS/forge-agent"
cp "$BUILD_DIR/forge-server" "$APP/Contents/MacOS/forge-server"
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
# No third-party runtime is bundled. The default renderer uses wgpu, which
# targets Apple's own Metal framework — already present on every Mac. The
# optional `vulkan-renderer` build needs MoltenVK installed on the host
# instead; it is deliberately not redistributed here.

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
