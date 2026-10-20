#!/bin/sh
# Cargo runner on macOS (wired up in .cargo/config.toml).
#
# The keychain remembers "Always Allow" for a binary's code signature, and the
# linker gives every build a new ad-hoc signature, so macOS asks for the
# keychain password again after each rebuild. Re-signing the app binary with a
# stable identity keeps the signature's requirement the same across builds, so
# one "Always Allow" lasts.
#
# The identity is $GH_REVIEW_SIGN_IDENTITY, or the first "Apple Development"
# identity in the keychain. Without one, the binary runs as built.
set -eu

bin="$1"
shift

if [ "$(basename "$bin")" = "gh-review-desktop" ]; then
    identity="${GH_REVIEW_SIGN_IDENTITY:-$(security find-identity -v -p codesigning 2>/dev/null |
        awk '/"Apple Development/ { print $2; exit }')}"
    if [ -z "$identity" ]; then
        echo "note: no code-signing identity found; the keychain will ask again after each rebuild" >&2
    elif ! codesign --force --sign "$identity" --identifier gh-review-desktop "$bin" 2>/dev/null; then
        echo "note: could not sign $bin with $identity; the keychain may ask again" >&2
    fi
fi

exec "$bin" "$@"
