#!/bin/sh
# Build wene.app for the Mac App Store: signed with Apple Distribution,
# with the store profile inside, packed in a signed .pkg. Upload the
# .pkg with Apple's Transporter app; Transporter validates it first.
#
# Needs, once per Mac:
#   WENE_APPSTORE_IDENTITY  the full identity name that
#                           `security find-identity -v -p codesigning`
#                           prints, "Apple Distribution: Name (TEAMID)"
#   WENE_PROFILE            the path to the "Mac App Store Connect"
#                           provisioning profile for dev.shaho.wene
# The keychain must also hold "3rd Party Mac Developer Installer" with
# the same name and team; it signs the .pkg.
set -eu

cd "$(dirname "$0")/.."

case "${WENE_APPSTORE_IDENTITY:-}" in
  "Apple Distribution: "*) ;;
  *)
    echo "WENE_APPSTORE_IDENTITY is \"${WENE_APPSTORE_IDENTITY:-}\": give it the full \"Apple Distribution: Name (TEAMID)\" name that security find-identity -v -p codesigning prints" >&2
    exit 1
    ;;
esac
if [ ! -f "${WENE_PROFILE:-}" ]; then
  echo "WENE_PROFILE is \"${WENE_PROFILE:-}\": give it the path to the Mac App Store Connect profile (.provisionprofile)" >&2
  exit 1
fi
INSTALLER="3rd Party Mac Developer Installer: ${WENE_APPSTORE_IDENTITY#Apple Distribution: }"

# Fail before the long build, not after it.
if ! security find-identity -v -p codesigning | grep -qF "\"$WENE_APPSTORE_IDENTITY\""; then
  echo "no valid code signing identity \"$WENE_APPSTORE_IDENTITY\" in the keychain; security find-identity -v -p codesigning lists the ones there are" >&2
  exit 1
fi
if ! security find-identity -v | grep -qF "\"$INSTALLER\""; then
  echo "no valid installer identity \"$INSTALLER\" in the keychain; security find-identity -v lists the ones there are" >&2
  exit 1
fi

# The profile says which app and team it is for. The store build signs
# with exactly those values, so they come from the profile, not from
# a second copy here.
PROFILE_PLIST=target/appstore-profile.plist
mkdir -p target
security cms -D -i "$WENE_PROFILE" >"$PROFILE_PLIST"
APP_ID=$(plutil -extract Entitlements.com\\.apple\\.application-identifier raw -o - "$PROFILE_PLIST")
TEAM=$(plutil -extract TeamIdentifier.0 raw -o - "$PROFILE_PLIST")
if [ "$APP_ID" != "$TEAM.dev.shaho.wene" ]; then
  echo "the profile is for \"$APP_ID\", not \"$TEAM.dev.shaho.wene\"" >&2
  exit 1
fi

./scripts/make-app.sh
APP=target/wene.app
VERSION=$(./scripts/version.sh)

cp -X "$WENE_PROFILE" "$APP/Contents/embedded.provisionprofile"
# App Store Connect refuses a bundle with com.apple.quarantine on any
# file (error 91109). A profile from the browser has it, and macOS
# puts it back on a copy even with cp -X, so remove it from the whole
# bundle and check.
xattr -dr com.apple.quarantine "$APP"
if xattr -r "$APP" | grep -q com.apple.quarantine; then
  echo "com.apple.quarantine is still on a file in $APP:" >&2
  xattr -r "$APP" | grep com.apple.quarantine >&2
  exit 1
fi

# The brew build's entitlements plus the two identifiers the store
# needs. The Developer ID build must not carry them: without a profile
# the app would not launch.
ENTITLEMENTS=target/appstore.entitlements
cp wene/wene.entitlements "$ENTITLEMENTS"
/usr/libexec/PlistBuddy \
  -c "Add :com.apple.application-identifier string $APP_ID" \
  -c "Add :com.apple.developer.team-identifier string $TEAM" \
  "$ENTITLEMENTS"

# Replaces make-app.sh's ad-hoc seal. No notarization and no stapling:
# the store does its own checks.
codesign --force --timestamp --entitlements "$ENTITLEMENTS" --sign "$WENE_APPSTORE_IDENTITY" "$APP"
codesign --verify --strict "$APP"

PKG=target/wene-$VERSION.pkg
rm -f "$PKG"
productbuild --component "$APP" /Applications --sign "$INSTALLER" "$PKG"
pkgutil --check-signature "$PKG"
rm -f "$PROFILE_PLIST" "$ENTITLEMENTS"

echo "built $PKG: drag it into Transporter and click Deliver"
