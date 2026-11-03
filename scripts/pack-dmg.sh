#!/bin/sh
# Pack target/wene.app, exactly as it is, into a compressed .dmg named
# after the version. It never builds or signs the app, so a release can
# pack the notarized bundle. Prints the .dmg path on stdout; everything
# else goes to stderr.
set -eu

cd "$(dirname "$0")/.."
APP=target/wene.app
if [ ! -d "$APP" ]; then
  echo "$APP does not exist: build it first with scripts/make-app.sh" >&2
  exit 1
fi

VERSION=$(./scripts/version.sh)
DMG=target/wene-$VERSION.dmg

STAGE=target/dmg-stage
rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"

diskutil image create from --format UDZO --volumeName "Wêne $VERSION" "$STAGE" "$DMG" >&2
rm -rf "$STAGE"
echo "$DMG"
