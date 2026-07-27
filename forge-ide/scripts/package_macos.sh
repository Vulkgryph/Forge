#!/bin/bash
# Builds a release .app bundle for Forge IDE: Contents/MacOS binary,
# generated .icns and Info.plist. No third-party runtime is bundled.
# Does not sign or notarize - see the .dmg release checklist for those steps.
set -euo pipefail
cd "$(dirname "$0")/.."

APP_NAME="Forge IDE"
BUNDLE_ID="com.windingcreek.forge-ide"
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

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BUILD_DIR/forge-ide" "$APP/Contents/MacOS/forge-ide"
cp "$BUILD_DIR/forge-agent" "$APP/Contents/MacOS/forge-agent"
cp "$BUILD_DIR/forge-server" "$APP/Contents/MacOS/forge-server"
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

echo "==> Done: $APP"
