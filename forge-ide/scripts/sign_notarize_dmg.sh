#!/bin/bash
# Signs the .app built by package_macos.sh with a Developer ID Application
# identity, submits it for notarization, staples the ticket, and produces
# a signed+notarized .dmg. Requires:
#   - a "Developer ID Application" identity in the login keychain
#   - notarization credentials stored via:
#       xcrun notarytool store-credentials "forge-notary" \
#         --apple-id <email> --team-id <team>
#     (the prompt takes an app-specific password from appleid.apple.com,
#     not the account password — notarytool refuses the latter with a 401)
#
# Works on a *copy* of the bundle. Signing rewrites the executables inside it,
# and macOS checks each page of a running process against the signature
# recorded when the file was mapped — so re-signing the bundle a window is
# running from kills that window with `CODESIGNING, Code 2, Invalid Page`.
# That has happened here before. The original is left alone, which means this
# is safe to run with the IDE open.
set -euo pipefail
cd "$(dirname "$0")/.."

SIGN_ID="Developer ID Application: Vulkgryph LLC (W5DSR5XA65)"
KEYCHAIN_PROFILE="forge-notary"
APP_NAME="Forge IDE"
# See package_macos.sh - shared workspace target/, one level up from ide/.
OUT_DIR="../target/dist"
SRC_APP="$OUT_DIR/$APP_NAME.app"
# Everything below happens here, never in $SRC_APP.
STAGE="$OUT_DIR/dmg-build"
APP="$STAGE/$APP_NAME.app"
ENTITLEMENTS="scripts/entitlements.plist"

# The version the .dmg is named for, read from the manifest rather than kept in
# step by hand: a release asset called "Forge IDE.dmg" tells nobody which build
# they downloaded.
VERSION="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
DMG="$OUT_DIR/Forge-IDE-${VERSION}.dmg"
ZIP="$OUT_DIR/$APP_NAME-notarize.zip"

if [ ! -d "$SRC_APP" ]; then
  echo "error: $SRC_APP not found - run scripts/package_macos.sh first" >&2
  exit 1
fi

# Checked before any work: notarization is the slow part and the credential is
# the thing most likely to be missing, so failing here beats failing after
# several minutes of signing and uploading.
if ! xcrun notarytool history --keychain-profile "$KEYCHAIN_PROFILE" >/dev/null 2>&1; then
  echo "error: no usable notarization credentials in keychain profile '$KEYCHAIN_PROFILE'" >&2
  echo "  store them with:" >&2
  echo "    xcrun notarytool store-credentials \"$KEYCHAIN_PROFILE\" \\" >&2
  echo "      --apple-id <apple-id> --team-id W5DSR5XA65" >&2
  echo "  and give it an app-specific password from appleid.apple.com." >&2
  exit 1
fi

echo "==> Staging a copy (the original stays signed as-is, and runnable)"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp -R "$SRC_APP" "$APP"

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

# Every nested executable, found rather than listed. The bundle carries
# forge-agent and forge-server beside the main binary, and this script used to
# name only forge-agent — so forge-server was signed solely by
# package_macos.sh's `--deep` pass, and a bundle assembled any other way went
# to Apple with an unsigned nested binary in it. Notarization rejects that.
echo "==> Signing nested executables"
for exe in "$APP/Contents/MacOS/"*; do
  name="$(basename "$exe")"
  [ "$name" = "$(basename "$APP" .app)" ] && continue   # not the main one
  [ "$name" = "forge-ide" ] && continue                 # signed with entitlements below
  [ -f "$exe" ] || continue
  echo "    $name"
  codesign --force --options runtime --timestamp --sign "$SIGN_ID" "$exe"
done

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
# The check that matters: a notarized disk image is *accepted* here. Before
# stapling this says "rejected / source=Unnotarized Developer ID" even though
# the signature itself is valid, which is the whole reason notarization exists.
spctl -a -vv -t open --context context:primary-signature "$DMG" 2>&1 || true
