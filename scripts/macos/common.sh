# Shared settings for the macOS packaging scripts. Source it; don't run it.
#
# Every value can be overridden from the environment, e.g.
#   SIGN_IDENTITY="Developer ID Application: ..." scripts/macos/archive.sh

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# Cargo package / binary name.
BIN_NAME="gh-review-desktop"
# Name shown in Finder, the Dock and the menu bar.
APP_NAME="${APP_NAME:-GH Review}"
BUNDLE_ID="${BUNDLE_ID:-com.sercand.gh-review-desktop}"
MIN_MACOS="${MIN_MACOS:-12.0}"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
# Monotonic build number; notarization and Sparkle-style updaters want it to grow.
BUILD_NUMBER="${BUILD_NUMBER:-$(git -C "$ROOT" rev-list --count HEAD 2>/dev/null || echo 1)}"

DIST_DIR="${DIST_DIR:-$ROOT/target/dist}"
ARCHIVE_PATH="$DIST_DIR/$BIN_NAME-$VERSION.xcarchive"
ICON_SVG="$ROOT/assets/app-icon/AppIcon.svg"
ICNS_PATH="$ROOT/assets/app-icon/AppIcon.icns"

# Signing identity: the first "Developer ID Application" in the keychain unless
# set. Distribution outside the App Store needs a Developer ID certificate.
SIGN_IDENTITY="${SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null |
    awk -F'"' '/Developer ID Application/ { print $2; exit }')}"

# notarytool keychain profile, created once with:
#   xcrun notarytool store-credentials gh-review-notary \
#       --apple-id <apple-id> --team-id <team-id> --password <app-specific-password>
NOTARY_PROFILE="${NOTARY_PROFILE:-gh-review-notary}"

log() { printf '==> %s\n' "$*"; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }
