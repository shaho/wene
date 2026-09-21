#!/bin/sh
# Build wene.app and wrap it in a compressed .dmg for quick sharing.
set -eu

cd "$(dirname "$0")/.."
./scripts/make-app.sh

STAGE=target/dmg-stage
rm -rf "$STAGE" target/wene.dmg
mkdir -p "$STAGE"
cp -R target/wene.app "$STAGE/"
ln -s /Applications "$STAGE/Applications"

hdiutil create -volname "Wêne" -srcfolder "$STAGE" -ov -format UDZO target/wene.dmg
rm -rf "$STAGE"
echo "built target/wene.dmg"
