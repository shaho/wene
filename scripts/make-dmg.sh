#!/bin/sh
# Build wene.app and wrap it in a compressed .dmg for quick sharing.
set -eu

cd "$(dirname "$0")/.."
./scripts/make-app.sh

VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' wene/Cargo.toml | head -1)
DMG=target/wene-$VERSION.dmg

STAGE=target/dmg-stage
rm -rf "$STAGE" "$DMG"
mkdir -p "$STAGE"
cp -R target/wene.app "$STAGE/"
ln -s /Applications "$STAGE/Applications"

hdiutil create -volname "Wêne $VERSION" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
echo "built $DMG"
