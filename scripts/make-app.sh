#!/bin/sh
# Build wene and wrap it in a minimal .app bundle (see the map's
# packaging decision: no cargo-bundle, hand-written Info.plist).
set -eu

cd "$(dirname "$0")/.."
cargo build --release

APP=target/wene.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS"
cp target/release/wene "$APP/Contents/MacOS/wene"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>wene</string>
  <key>CFBundleIdentifier</key><string>dev.shaho.wene</string>
  <key>CFBundleName</key><string>wene</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.5</string>
</dict>
</plist>
PLIST

echo "built $APP"
