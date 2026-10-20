#!/usr/bin/env bash
# Tag the release and publish the notarized .dmg and .zip on GitHub.
#
#   scripts/macos/release.sh [--draft] [--dry-run]
#
# The tag is v<version> from Cargo.toml, on the commit archive.sh built. It
# refuses to run if the build doesn't match that commit, if the working tree has
# changes, or if the .dmg isn't stapled. Rerunning for an existing release
# replaces its assets.
#
#   --draft     create the release as a draft, to review before publishing
#   --dry-run   print what would happen; tag, push and upload nothing
#
# Needs the GitHub CLI, signed in: gh auth login
set -euo pipefail
source "$(dirname "$0")/common.sh"

draft=()
dry_run=false
for arg in "$@"; do
    case "$arg" in
        --draft) draft=(--draft) ;;
        --dry-run) dry_run=true ;;
        *) die "unknown option $arg (expected --draft or --dry-run)" ;;
    esac
done

# Print the command; run it unless this is a dry run.
run() {
    printf '    $ %s\n' "$*"
    $dry_run || "$@"
}

TAG="v$VERSION"
dmg="$DIST_BASE.dmg"
zip="$DIST_BASE.zip"
sums="$DIST_DIR/SHA256SUMS.txt"

# --- checks ------------------------------------------------------------------

command -v gh >/dev/null || die "install the GitHub CLI: brew install gh"
gh api user --silent 2>/dev/null || die "sign in to GitHub first: gh auth login"

for file in "$dmg" "$zip"; do
    [ -f "$file" ] || die "missing ${file#"$ROOT/"}; run scripts/macos/archive.sh and notarize.sh first"
done
xcrun stapler validate -q "$dmg" ||
    die "$(basename "$dmg") has no notarization ticket; run scripts/macos/notarize.sh"

[ -z "$(git -C "$ROOT" status --porcelain)" ] ||
    die "the working tree has uncommitted changes; commit them, then rebuild"

head="$(git -C "$ROOT" rev-parse HEAD)"
built="$(plutil -extract GitCommit raw "$ARCHIVE_PATH/Info.plist" 2>/dev/null || true)"
built_dirty="$(plutil -extract GitDirty raw "$ARCHIVE_PATH/Info.plist" 2>/dev/null || true)"
[ "$built" = "$head" ] ||
    die "the build is from ${built:0:7}, not HEAD (${head:0:7}); rebuild with archive.sh and notarize.sh"
[ "$built_dirty" = false ] ||
    die "the build includes uncommitted changes; rebuild from a clean tree"

if tagged="$(git -C "$ROOT" rev-parse -q --verify "refs/tags/$TAG^{commit}")"; then
    [ "$tagged" = "$head" ] || die "tag $TAG already points at ${tagged:0:7}; bump the version in Cargo.toml"
fi
remote="$(git -C "$ROOT" ls-remote --tags origin "refs/tags/$TAG^{}" | cut -f1)"
[ -z "$remote" ] || [ "$remote" = "$head" ] ||
    die "tag $TAG on origin points at ${remote:0:7}; bump the version in Cargo.toml"

$dry_run && log "dry run: nothing will be tagged, pushed or uploaded"

# --- tag ---------------------------------------------------------------------

if [ -z "${tagged:-}" ]; then
    log "tagging ${head:0:7} as $TAG"
    run git -C "$ROOT" tag -a "$TAG" -m "$APP_NAME $VERSION" "$head"
fi
if [ -z "$remote" ]; then
    log "pushing $TAG to origin"
    run git -C "$ROOT" push origin "refs/tags/$TAG"
fi

# --- release -----------------------------------------------------------------

log "writing $(basename "$sums")"
(cd "$DIST_DIR" && shasum -a 256 "$(basename "$dmg")" "$(basename "$zip")") >"$sums"
sed 's/^/    /' "$sums"

if gh release view "$TAG" >/dev/null 2>&1; then
    log "release $TAG exists; replacing its assets"
    run gh release upload "$TAG" "$dmg" "$zip" "$sums" --clobber
else
    log "creating release $TAG"
    run gh release create "$TAG" "$dmg" "$zip" "$sums" \
        --verify-tag --title "$APP_NAME $VERSION" --generate-notes "${draft[@]+"${draft[@]}"}"
fi

$dry_run || log "released: $(gh release view "$TAG" --json url --jq .url)"
