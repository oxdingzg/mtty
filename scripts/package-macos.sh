#!/usr/bin/env bash
# Build one native mtty.app, carrying mtty, mtty-cli and mtty-ptyhost (ADR 0041).
# PROFILE=debug scripts/package-macos.sh also supports a development bundle.
# Signs with a stable identity when one is available (see MTTY_CODESIGN_IDENTITY).
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
profile="${PROFILE:-release}"
target="${CARGO_TARGET_DIR:-target}"
if [ "$profile" = release ]; then
  cargo build --release -p mtty-app -p mtty-cli -p mtty-ptyhost
else
  cargo build -p mtty-app -p mtty-cli -p mtty-ptyhost
fi
built="$target/$profile"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
png="assets/icons/mtty.png"
[ -f "$png" ] || python3 scripts/make-icon.py "$png" 2e3440
iconset="$staging/mtty.iconset"
mkdir -p "$iconset"
for size in 16 32 64 128 256 512; do
  sips -z "$size" "$size" "$png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
  sips -z "$((size * 2))" "$((size * 2))" "$png" \
    --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$staging/mtty.icns"
app="$staging/mtty.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$built/mtty" "$built/mtty-cli" "$built/mtty-ptyhost" "$app/Contents/MacOS/"
cp "$staging/mtty.icns" "$app/Contents/Resources/"
# The bundle carries its own licence and the licences of everything in it.
# Apache-2.0 asks for a copy of the licence with each distribution, and so does
# every MIT, BSD and ISC crate compiled into these binaries.
cp LICENSE "$app/Contents/Resources/LICENSE.txt"
cp THIRD-PARTY-LICENSES.md "$app/Contents/Resources/THIRD-PARTY-LICENSES.md"
cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>mtty</string>
  <key>CFBundleDisplayName</key><string>mtty</string>
  <key>CFBundleIdentifier</key><string>dev.mtty.terminal</string>
  <key>CFBundleExecutable</key><string>mtty</string>
  <key>CFBundleIconFile</key><string>mtty.icns</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${version}</string>
  <key>CFBundleVersion</key><string>${version}</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>CFBundleURLTypes</key><array><dict>
    <key>CFBundleURLName</key><string>dev.mtty.terminal</string>
    <key>CFBundleURLSchemes</key><array>
      <string>mtty</string><string>miaotty</string><string>ssh</string><string>x-man-page</string>
    </array>
  </dict></array>
</dict></plist>
PLIST
# Sign with a stable identity when the machine has one. An ad-hoc signature
# (`--sign -`) gives the bundle a new identity on every build, so macOS cannot
# remember a folder permission the user granted once (Documents, Desktop,
# Downloads) and asks again after each rebuild. A real identity keeps the same
# designated requirement across builds, so the grant sticks. Override the
# choice with MTTY_CODESIGN_IDENTITY; release builds re-sign in CI anyway.
identity="${MTTY_CODESIGN_IDENTITY:-}"
if [ -z "$identity" ]; then
  identity="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)"
fi
if [ -n "$identity" ]; then
  codesign --force --deep --sign "$identity" "$app"
else
  codesign --force --deep --sign - "$app"
fi
mkdir -p "$root/dist"
# Replace only our generated bundles, preserving unrelated dist artifacts.
rm -rf "$root/dist/mtty.app"
mv "$app" "$root/dist/mtty.app"
# Bundles built before the rename (ADR 0032), removed only by identity.
for legacy_entry in miaotty:io.miaotty.terminal miaotty-native:io.miaotty.terminal.native; do
  legacy="$root/dist/${legacy_entry%%:*}.app"
  if [ -f "$legacy/Contents/Info.plist" ] && \
     [ "$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$legacy/Contents/Info.plist")" = "${legacy_entry#*:}" ]; then
    rm -rf "$legacy"
  fi
done
python3 scripts/check-macos-bundle.py "$root/dist/mtty.app"
echo "built: dist/mtty.app (native mtty)"
