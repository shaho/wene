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
  <!-- Image types wene can decode, so Finder offers it in Open With
       and lets it be made the default. -->
  <key>CFBundleDocumentTypes</key>
  <array>
    <dict>
      <key>CFBundleTypeName</key><string>Image</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Default</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.jpeg</string>
        <string>public.png</string>
        <string>public.heic</string>
        <string>com.compuserve.gif</string>
        <string>org.webmproject.webp</string>
        <string>public.tiff</string>
      </array>
    </dict>
    <dict>
      <key>CFBundleTypeName</key><string>Folder</string>
      <key>CFBundleTypeRole</key><string>Viewer</string>
      <key>LSHandlerRank</key><string>Alternate</string>
      <key>LSItemContentTypes</key>
      <array>
        <string>public.folder</string>
      </array>
    </dict>
  </array>
</dict>
</plist>
PLIST

# Launch Services keeps the old document types until it re-reads the
# bundle, so force it during development.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[ -x "$LSREGISTER" ] && "$LSREGISTER" -f "$APP" || true

echo "built $APP"
