#!/bin/bash
# Signs the .app built by package_macos.sh with a Developer ID Application
# identity, submits it for notarization, staples the ticket, and produces
# a signed+notarized .dmg. Requires:
#   - a "Developer ID Application" identity in the login keychain
#   - notarization credentials stored via:
#       xcrun notarytool store-credentials "forge-notary" \
#         --apple-id <email> --team-id <team> --password <app-specific password>
set -euo pipefail
cd "$(dirname "$0")/.."

SIGN_ID="Developer ID Application: Vulkgryph LLC (W5DSR5XA65)"
KEYCHAIN_PROFILE="forge-notary"
APP_NAME="Forge IDE"
# See package_macos.sh - shared workspace target/, one level up from ide/.
OUT_DIR="../target/dist"
APP="$OUT_DIR/$APP_NAME.app"
DMG="$OUT_DIR/$APP_NAME.dmg"
ZIP="$OUT_DIR/$APP_NAME-notarize.zip"
ENTITLEMENTS="scripts/entitlements.plist"

if [ ! -d "$APP" ]; then
  echo "error: $APP not found - run scripts/package_macos.sh first" >&2
  exit 1
fi

# No third-party library is bundled: the default renderer is wgpu, which goes
# through Apple's own Metal framework. The optional `vulkan-renderer` build
# needs MoltenVK installed on the host, and a packager who chooses to embed
# their own copy still needs it signed — hence the existence check rather than
# an unconditional sign, which failed outright once the bundled copy was gone.
if [ -f "$APP/Contents/Frameworks/libMoltenVK.dylib" ]; then
  echo "==> Signing embedded libraries"
  codesign --force --options runtime --timestamp \
    --sign "$SIGN_ID" \
    "$APP/Contents/Frameworks/libMoltenVK.dylib"
fi

echo "==> Signing bundled forge-agent"
codesign --force --options runtime --timestamp \
  --sign "$SIGN_ID" \
  "$APP/Contents/MacOS/forge-agent"

echo "==> Signing main executable"
codesign --force --options runtime --timestamp \
  --entitlements "$ENTITLEMENTS" \
  --sign "$SIGN_ID" \
  "$APP/Contents/MacOS/forge-ide"

echo "==> Signing app bundle"
codesign --force --options runtime --timestamp \
  --entitlements "$ENTITLEMENTS" \
  --sign "$SIGN_ID" \
  "$APP"

echo "==> Verifying signature"
codesign --verify --deep --strict --verbose=2 "$APP"

echo "==> Zipping for notarization submission"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"

echo "==> Submitting for notarization (this can take several minutes)"
xcrun notarytool submit "$ZIP" --keychain-profile "$KEYCHAIN_PROFILE" --wait

echo "==> Stapling notarization ticket to app"
xcrun stapler staple "$APP"

echo "==> Building .dmg"
rm -f "$DMG"
STAGING="$OUT_DIR/dmg_staging"
rm -rf "$STAGING"
mkdir -p "$STAGING"
cp -R "$APP" "$STAGING/"
ln -s /Applications "$STAGING/Applications"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGING" -ov -format UDZO "$DMG"
rm -rf "$STAGING"

echo "==> Signing .dmg"
codesign --force --timestamp --sign "$SIGN_ID" "$DMG"

echo "==> Submitting .dmg for notarization"
xcrun notarytool submit "$DMG" --keychain-profile "$KEYCHAIN_PROFILE" --wait

echo "==> Stapling notarization ticket to .dmg"
xcrun stapler staple "$DMG"

echo "==> Done: $DMG"
spctl -a -vv -t open --context context:primary-signature "$DMG" 2>&1 || true
