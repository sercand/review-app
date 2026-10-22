# macOS release

```sh
scripts/macos/archive.sh    # universal, signed .app inside an .xcarchive
scripts/macos/notarize.sh   # notarize, staple, and build the .dmg / .zip
```

Outputs land in `target/dist/`.

`archive.sh` builds with `--features local-classifier` (the optional on-device
command classifier, Metal); the weights are not bundled, the app downloads them
only when the user turns the classifier on. Metal needs no entitlement.

## One-time setup

- A **Developer ID Application** certificate for team `8SWC36YF9N` in the login
  keychain. To sign with another team, set `TEAM_ID`, or set `SIGN_IDENTITY` to
  the certificate's full name.
- Both Rust targets: `rustup target add aarch64-apple-darwin x86_64-apple-darwin`.
- Notarization credentials, saved as a keychain profile (create the
  app-specific password at appleid.apple.com):

  ```sh
  xcrun notarytool store-credentials gh-review-notary \
      --apple-id <apple-id> --team-id 8SWC36YF9N --password <app-specific-password>
  ```

## Icon

`assets/app-icon/AppIcon.svg` is the source. After editing it, run
`scripts/macos/make-icon.sh` to rebuild `AppIcon.icns`. You need
`rsvg-convert` (`brew install librsvg`) or `resvg`.

For development, `cargo run` wraps the binary in a minimal bundle under
`target/bundle/` (via `scripts/macos/dev-app.sh`) so the Dock shows the real
app icon; run `scripts/macos/dev-app.sh` directly and `open` the printed path
to launch the bundle without cargo. If the Dock keeps showing the generic
placeholder after an update, the icon cache is stale:

```sh
/System/Library/Frameworks/CoreServices.framework/Versions/A/Frameworks/LaunchServices.framework/Versions/A/Support/lsregister -f "/Applications/GH Review.app"
killall Dock
```

## Settings

The defaults live in `common.sh`, and you can override any of them from the
environment: `APP_NAME` (default "GH Review"), `BUNDLE_ID`
(`com.sercand.gh-review-desktop`), `MIN_MACOS` (12.0), `BUILD_NUMBER` (git
commit count), `NOTARY_PROFILE`, `TARGETS`, and `DIST_DIR`. The version comes
from `Cargo.toml`.
