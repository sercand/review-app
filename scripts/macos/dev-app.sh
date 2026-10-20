#!/usr/bin/env bash
# Assemble a minimal "GH Review.app" around a locally built binary and print
# its path. A bare cargo binary has no Info.plist, so the Dock shows the
# generic executable placeholder instead of the app icon; running the binary
# from inside a bundle fixes the icon and the Dock name.
#
#   scripts/macos/dev-app.sh [path-to-binary]   # default: target/debug binary
#
# Launch the bundle with `open`, or let the cargo runner (scripts/macos-run.sh)
# exec the binary from inside it so `cargo run` gets the same treatment.
# The bundle is disposable; it lives in target/bundle.
set -euo pipefail
source "$(dirname "$0")/common.sh"

bin="${1:-$ROOT/target/debug/$BIN_NAME}"
[ -x "$bin" ] || die "no executable at ${bin#"$ROOT/"}; run: cargo build"
[ -f "$ICNS_PATH" ] || "$(dirname "$0")/make-icon.sh"

app="$DEV_APP_DIR/$APP_NAME.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

# Overwriting a running binary fails with ETXTBSY; stage beside it and rename.
tmp="$app/Contents/MacOS/.$BIN_NAME.$$"
cp "$bin" "$tmp"
mv -f "$tmp" "$app/Contents/MacOS/$BIN_NAME"
cp "$ICNS_PATH" "$app/Contents/Resources/AppIcon.icns"

cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>$APP_NAME</string>
    <key>CFBundleExecutable</key>
    <string>$BIN_NAME</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>$APP_NAME</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$BUILD_NUMBER</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MIN_MACOS</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
EOF

printf '%s\n' "$app"
