#!/bin/sh
# Build wene and wrap it in a minimal .app bundle (see the map's
# packaging decision: no cargo-bundle, hand-written Info.plist).
set -eu

cd "$(dirname "$0")/.."
# One universal binary: Apple silicon and Intel Macs run the same bundle.
# Needs `rustup target add x86_64-apple-darwin` once.
cargo build --release --target aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin

VERSION=$(./scripts/version.sh)

APP=target/wene.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
lipo -create \
  target/aarch64-apple-darwin/release/wene \
  target/x86_64-apple-darwin/release/wene \
  -output "$APP/Contents/MacOS/wene"

# App icon: actool compiles the asset catalog into Assets.car plus
# AppIcon.icns. The App Store wants the Assets.car that CFBundleIconName
# names. actool reads only an .xcassets folder, so stage one.
ASSETS=target/Assets.xcassets
rm -rf "$ASSETS"
mkdir -p "$ASSETS"
cp -R branding/macos/AppIcon.appiconset "$ASSETS/"
xcrun actool "$ASSETS" --compile "$APP/Contents/Resources" \
  --platform macosx --minimum-deployment-target 11.5 --app-icon AppIcon \
  --output-partial-info-plist target/actool-info.plist \
  --errors --warnings >/dev/null
rm -rf "$ASSETS" target/actool-info.plist

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>wene</string>
  <key>CFBundleIdentifier</key><string>dev.shaho.wene</string>
  <key>CFBundleName</key><string>Wêne</string>
  <key>CFBundleDisplayName</key><string>Wêne</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleIconName</key><string>AppIcon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <!-- The App Store's build number. Every slice bumps the version, so
       every upload has a higher one. -->
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSMinimumSystemVersion</key><string>11.5</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.photography</string>
  <!-- wene uses no encryption, so App Store Connect does not ask about
       export rules on every upload. -->
  <key>ITSAppUsesNonExemptEncryption</key><false/>
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
        <string>public.heif</string>
        <string>com.microsoft.bmp</string>
        <string>public.avif</string>
        <string>public.jpeg-xl</string>
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

# Seal the whole bundle with an ad-hoc signature. The linker signs
# only the binary, so Info.plist and the icon were not covered, and
# Gatekeeper calls a downloaded copy of such a bundle "damaged". Ad
# hoc is not a Developer ID: another Mac still asks before the first
# open, but it can open the app. scripts/make-release.sh re-signs with a
# Developer ID and notarizes. The entitlements put the app in the App
# Sandbox, which the App Store requires (ADR 0002).
codesign --force --sign - --entitlements wene/wene.entitlements "$APP"
codesign --verify --strict "$APP"

# Launch Services keeps the old document types until it re-reads the
# bundle, so force it during development.
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
[ -x "$LSREGISTER" ] && "$LSREGISTER" -f "$APP" || true

echo "built $APP ($VERSION)"
