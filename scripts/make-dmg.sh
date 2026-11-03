#!/bin/sh
# Build wene.app and wrap it in a compressed .dmg for quick sharing.
set -eu

cd "$(dirname "$0")/.."
./scripts/make-app.sh
DMG=$(./scripts/pack-dmg.sh)
echo "built $DMG"
