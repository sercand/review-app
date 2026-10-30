#!/usr/bin/env bash
# Build a signed, arm64-only "GH Review.app" and wrap it in an .xcarchive.
#
#   scripts/macos/archive.sh
#
# Output: target/dist/gh-review-desktop-<version>.xcarchive, holding
#   Products/Applications/<APP_NAME>.app   signed with hardened runtime
#   dSYMs/gh-review-desktop.dSYM           for symbolicating crash reports
#
# Environment (see common.sh): SIGN_IDENTITY, BUNDLE_ID, APP_NAME, MIN_MACOS,
# BUILD_NUMBER, and TARGETS (default "aarch64-apple-darwin"; add more triples
# for a universal build).
#
# Next step: scripts/macos/notarize.sh
set -euo pipefail
source "$(dirname "$0")/common.sh"

TARGETS="${TARGETS:-aarch64-apple-darwin}"

[ -n "$SIGN_IDENTITY" ] || die "no \"Developer ID Application\" identity for team $TEAM_ID in the keychain; set TEAM_ID or SIGN_IDENTITY"
for target in $TARGETS; do
    rustup target list --installed | grep -qx "$target" ||
        die "missing Rust target $target; run: rustup target add $target"
done

[ -f "$ICNS_PATH" ] || "$(dirname "$0")/make-icon.sh"

# --- build -------------------------------------------------------------------

# Line tables are enough to symbolicate crash reports; "packed" makes cargo run
# dsymutil and leave a .dSYM next to each binary.
export CARGO_PROFILE_RELEASE_DEBUG=line-tables-only
export CARGO_PROFILE_RELEASE_SPLIT_DEBUGINFO=packed
export MACOSX_DEPLOYMENT_TARGET="$MIN_MACOS"

bins=()
dsyms=()
for target in $TARGETS; do
    log "building $BIN_NAME $VERSION ($BUILD_NUMBER) for $target"
    cargo build --manifest-path "$ROOT/Cargo.toml" --release --locked --target "$target" --bin "$BIN_NAME" --features local-classifier
    bins+=("$ROOT/target/$target/release/$BIN_NAME")
    dsyms+=("$ROOT/target/$target/release/$BIN_NAME.dSYM")
done

# --- assemble the archive ----------------------------------------------------

rm -rf "$ARCHIVE_PATH"
app="$ARCHIVE_PATH/Products/Applications/$APP_NAME.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$ARCHIVE_PATH/dSYMs"

log "assembling ${app#"$ROOT/"}"
lipo -create "${bins[@]}" -output "$app/Contents/MacOS/$BIN_NAME"
cp "$ICNS_PATH" "$app/Contents/Resources/AppIcon.icns"

# Cargo's .dSYM is a symlink into deps/, and the DWARF file inside is named
# after the hashed artifact, so take whichever single file it holds.
dsym="$ARCHIVE_PATH/dSYMs/$BIN_NAME.dSYM"
cp -RL "${dsyms[0]}" "$dsym"
rm -f "$dsym"/Contents/Resources/DWARF/*
dwarf=()
for d in "${dsyms[@]}"; do dwarf+=("$d"/Contents/Resources/DWARF/*); done
lipo -create "${dwarf[@]}" -output "$dsym/Contents/Resources/DWARF/$BIN_NAME"

archs="$(lipo -archs "$app/Contents/MacOS/$BIN_NAME")"
year="$(date +%Y)"

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
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MIN_MACOS</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSHumanReadableCopyright</key>
    <string>Copyright © $year Sercan Degirmenci</string>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
</dict>
</plist>
EOF
plutil -lint -s "$app/Contents/Info.plist"

# --- sign --------------------------------------------------------------------

# Notarization requires the hardened runtime and a secure timestamp.
log "signing with $SIGN_IDENTITY"
codesign --force --options runtime --timestamp --sign "$SIGN_IDENTITY" "$app"
codesign --verify --strict --verbose=2 "$app"

# --- archive metadata --------------------------------------------------------

cat >"$ARCHIVE_PATH/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>ApplicationProperties</key>
    <dict>
        <key>ApplicationPath</key>
        <string>Applications/$APP_NAME.app</string>
        <key>Architectures</key>
        <array>
$(for a in $archs; do printf '            <string>%s</string>\n' "$a"; done)
        </array>
        <key>CFBundleIdentifier</key>
        <string>$BUNDLE_ID</string>
        <key>CFBundleShortVersionString</key>
        <string>$VERSION</string>
        <key>CFBundleVersion</key>
        <string>$BUILD_NUMBER</string>
        <key>SigningIdentity</key>
        <string>$SIGN_IDENTITY</string>
    </dict>
    <key>ArchiveVersion</key>
    <integer>2</integer>
    <key>CreationDate</key>
    <date>$(date -u +%Y-%m-%dT%H:%M:%SZ)</date>
    <key>Name</key>
    <string>$APP_NAME</string>
    <key>SchemeName</key>
    <string>$BIN_NAME</string>
    <key>GitCommit</key>
    <string>$(git -C "$ROOT" rev-parse HEAD)</string>
    <key>GitDirty</key>
    <$([ -z "$(git -C "$ROOT" status --porcelain)" ] && echo false || echo true)/>
</dict>
</plist>
EOF
plutil -lint -s "$ARCHIVE_PATH/Info.plist"

log "archive ready: ${ARCHIVE_PATH#"$ROOT/"} ($archs)"
log "next: scripts/macos/notarize.sh"
