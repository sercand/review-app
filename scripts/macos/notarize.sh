#!/usr/bin/env bash
# Notarize the app from the .xcarchive that archive.sh built, staple the
# tickets, and produce the files to distribute:
#
#   target/dist/GH-Review-<version>.dmg   drag-to-Applications disk image
#   target/dist/GH-Review-<version>.zip   the stapled app, zipped
#
# One-time setup — store notarization credentials in the keychain (use an
# app-specific password from appleid.apple.com, and the team ID of the signing
# certificate):
#
#   xcrun notarytool store-credentials gh-review-notary \
#       --apple-id <apple-id> --team-id <team-id> --password <app-specific-password>
#
# Environment (see common.sh): NOTARY_PROFILE, SIGN_IDENTITY.
set -euo pipefail
source "$(dirname "$0")/common.sh"

app="$ARCHIVE_PATH/Products/Applications/$APP_NAME.app"
[ -d "$app" ] || die "no archive at ${ARCHIVE_PATH#"$ROOT/"}; run scripts/macos/archive.sh first"
[ -n "$SIGN_IDENTITY" ] || die "no \"Developer ID Application\" identity in the keychain; set SIGN_IDENTITY"

base="$DIST_DIR/${APP_NAME// /-}-$VERSION"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Submit $1 to Apple and wait for the verdict; print the log on rejection.
notarize() {
    local file="$1" result id status
    log "notarizing $(basename "$file") (this usually takes a few minutes)"
    result="$work/submit.json"
    xcrun notarytool submit "$file" --keychain-profile "$NOTARY_PROFILE" \
        --wait --output-format json >"$result" || true
    id="$(plutil -extract id raw "$result" 2>/dev/null || true)"
    status="$(plutil -extract status raw "$result" 2>/dev/null || true)"
    if [ "$status" != "Accepted" ]; then
        cat "$result" >&2
        [ -n "$id" ] && xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" >&2
        die "notarization of $(basename "$file") failed (status: ${status:-unknown})"
    fi
    log "accepted (submission $id)"
}

codesign --verify --strict --verbose=2 "$app"

# --- app ---------------------------------------------------------------------

# Notarize a zip of the app, then staple the ticket to the app itself so
# Gatekeeper can verify it offline.
staged="$work/$APP_NAME.app"
ditto "$app" "$staged"
ditto -c -k --keepParent "$staged" "$work/upload.zip"
notarize "$work/upload.zip"
xcrun stapler staple "$staged"

mkdir -p "$DIST_DIR"
rm -f "$base.zip"
ditto -c -k --keepParent "$staged" "$base.zip"

# --- dmg ---------------------------------------------------------------------

dmg_root="$work/dmg"
mkdir "$dmg_root"
ditto "$staged" "$dmg_root/$APP_NAME.app"
ln -s /Applications "$dmg_root/Applications"

rm -f "$base.dmg"
log "building $(basename "$base.dmg")"
hdiutil create -quiet -volname "$APP_NAME" -srcfolder "$dmg_root" \
    -fs HFS+ -format UDZO -ov "$base.dmg"
codesign --force --timestamp --sign "$SIGN_IDENTITY" "$base.dmg"
notarize "$base.dmg"
xcrun stapler staple "$base.dmg"

# --- check -------------------------------------------------------------------

spctl --assess --type execute --verbose=2 "$staged"
spctl --assess --type open --context context:primary-signature --verbose=2 "$base.dmg"

log "ready to distribute:"
log "  ${base#"$ROOT/"}.dmg"
log "  ${base#"$ROOT/"}.zip"
