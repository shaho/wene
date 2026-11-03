#!/bin/sh
# Build wene.app and its .dmg signed with a Developer ID, notarized by
# Apple, and stapled, so Gatekeeper opens both on any Mac with no prompt.
#
# Needs, once per Mac:
#   WENE_SIGN_IDENTITY   the full identity name that
#                        `security find-identity -v -p codesigning` prints,
#                        "Developer ID Application: Name (TEAMID)"
#   WENE_NOTARY_PROFILE  a profile saved with
#                        `xcrun notarytool store-credentials <profile> ...`
set -eu

cd "$(dirname "$0")/.."

if [ -z "${WENE_SIGN_IDENTITY:-}" ]; then
  echo "WENE_SIGN_IDENTITY is not set: give it the full name that security find-identity -v -p codesigning prints, \"Developer ID Application: Name (TEAMID)\"" >&2
  exit 1
fi
case "$WENE_SIGN_IDENTITY" in
  "Developer ID Application: "*) ;;
  *)
    echo "WENE_SIGN_IDENTITY is \"$WENE_SIGN_IDENTITY\": a release outside the App Store must be signed with a \"Developer ID Application: \" identity" >&2
    exit 1
    ;;
esac
if [ -z "${WENE_NOTARY_PROFILE:-}" ]; then
  echo "WENE_NOTARY_PROFILE is not set: give it the profile name saved with xcrun notarytool store-credentials" >&2
  exit 1
fi

# Fail before the long build, not after it.
if ! security find-identity -v -p codesigning | grep -qF "\"$WENE_SIGN_IDENTITY\""; then
  echo "no valid code signing identity \"$WENE_SIGN_IDENTITY\" in the keychain; security find-identity -v -p codesigning lists the ones there are" >&2
  exit 1
fi
if ! xcrun notarytool history --keychain-profile "$WENE_NOTARY_PROFILE" >/dev/null; then
  echo "notarytool cannot sign in with the keychain profile \"$WENE_NOTARY_PROFILE\" (its error is above)" >&2
  exit 1
fi

# Submit one file to Apple's notary service and wait for the verdict.
# Anything but "Accepted" stops the release and prints Apple's log.
notarize() {
  file=$1
  result=target/notary-result.json
  submit_exit=0
  # notarytool's own output goes to the result file, so say why it is quiet.
  echo "notarizing $file: uploading, then waiting for Apple's verdict (usually 1-5 minutes)"
  xcrun notarytool submit "$file" --keychain-profile "$WENE_NOTARY_PROFILE" \
    --wait --output-format json >"$result" || submit_exit=$?
  if [ "$submit_exit" -eq 0 ] &&
    [ "$(plutil -extract status raw -o - "$result")" = "Accepted" ]; then
    echo "notarized $file"
    return
  fi
  echo "notarization of $file failed (notarytool exit $submit_exit); its answer:" >&2
  cat "$result" >&2
  if id=$(plutil -extract id raw -o - "$result"); then
    echo "Apple's log for submission $id:" >&2
    xcrun notarytool log "$id" --keychain-profile "$WENE_NOTARY_PROFILE" >&2
  fi
  exit 1
}

./scripts/make-app.sh
APP=target/wene.app

# Replaces make-app.sh's ad-hoc seal. Notarization requires the hardened
# runtime and a secure timestamp. wene needs none of the runtime's
# exceptions (no JIT, no unsigned libraries); its entitlements are the
# App Sandbox ones, the same as the ad-hoc build.
codesign --force --options runtime --timestamp --entitlements wene/wene.entitlements --sign "$WENE_SIGN_IDENTITY" "$APP"
codesign --verify --strict "$APP"

# The notary service takes a zip, a dmg, or a pkg, not a bare bundle.
ZIP=target/wene-notarize.zip
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"
notarize "$ZIP"
rm -f "$ZIP"
# Stapled, the app opens offline too, also after it is copied out of the dmg.
xcrun stapler staple "$APP"
spctl --assess --type execute --verbose "$APP"

DMG=$(./scripts/pack-dmg.sh)
codesign --force --timestamp --sign "$WENE_SIGN_IDENTITY" "$DMG"
notarize "$DMG"
xcrun stapler staple "$DMG"
spctl --assess --type open --context context:primary-signature --verbose "$DMG"

echo "released $DMG"
shasum -a 256 "$DMG"
