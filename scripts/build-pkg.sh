#!/usr/bin/env bash
# Build dist/NovakeyR-<version>.pkg (plus a .sha256 file) from the already signed dist/NovakeyR.app
# (in CI also notarized and stapled). The package installs into the current user's home only
# (~/Library/Input Methods), so it needs no admin password. Installing it over an existing bundle
# keeps NovakeyR in Input Sources; deleting the bundle first does not. Never modifies the app.
# Usage: scripts/build-pkg.sh <identity>   (a "Developer ID Installer: ..." identity, or "-" to
#        leave the package unsigned for local tests)
set -euo pipefail

cd "$(dirname "$0")/.."
APP=dist/NovakeyR.app
PKG_ID=dev.n-seiji.inputmethod.NovakeyR.pkg

fail() {
  echo "build-pkg: $*" >&2
  exit 1
}

(($# == 1)) || fail "usage: scripts/build-pkg.sh <identity>"
identity=$1
[[ -d "$APP" ]] || fail "$APP not found (run stage-app.sh and sign-app.sh first)"

version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version '$version' in $APP is not X.Y.Z"
pkg=dist/NovakeyR-$version.pkg

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$tmp/packages" "$tmp/resources"
# The payload root stands for ~/Library/Input Methods; keep it 0755 whatever the umask.
chmod 755 "$tmp/root"
# ditto keeps the signature, the stapled ticket and extended attributes.
ditto "$APP" "$tmp/root/NovakeyR.app"

# Install exactly where the package says, even if another NovakeyR.app exists elsewhere.
pkgbuild --analyze --root "$tmp/root" "$tmp/component.plist" >/dev/null
plutil -replace 0.BundleIsRelocatable -bool NO "$tmp/component.plist"

# The install location is relative to the chosen domain: the user's home (see Distribution).
pkgbuild --root "$tmp/root" \
  --component-plist "$tmp/component.plist" \
  --identifier "$PKG_ID" \
  --version "$version" \
  --install-location "/Library/Input Methods" \
  "$tmp/packages/NovakeyR.pkg"

cat > "$tmp/resources/conclusion.txt" <<'TXT'
NovakeyR is installed in ~/Library/Input Methods.

First install: add NovakeyR in System Settings > Keyboard > Input Sources. If it is not listed yet, log out and back in first.

Updates: opening a newer NovakeyR package over this one keeps NovakeyR in Input Sources. Do not delete the old app first; deleting it drops NovakeyR from Input Sources.
TXT

cat > "$tmp/Distribution" <<XML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>NovakeyR</title>
    <options customize="never" require-scripts="false" hostArchitectures="arm64"/>
    <domains enable_anywhere="false" enable_currentUserHome="true" enable_localSystem="false"/>
    <allowed-os-versions>
        <os-version min="26.0"/>
    </allowed-os-versions>
    <conclusion file="conclusion.txt" mime-type="text/plain"/>
    <choices-outline>
        <line choice="default">
            <line choice="$PKG_ID"/>
        </line>
    </choices-outline>
    <choice id="default"/>
    <choice id="$PKG_ID" visible="false">
        <pkg-ref id="$PKG_ID"/>
    </choice>
    <pkg-ref id="$PKG_ID" version="$version" onConclusion="none">NovakeyR.pkg</pkg-ref>
</installer-gui-script>
XML

rm -f "$pkg" "$pkg.sha256"
if [[ "$identity" == "-" ]]; then
  productbuild --distribution "$tmp/Distribution" --package-path "$tmp/packages" \
    --resources "$tmp/resources" "$pkg"
  echo "build-pkg: $pkg is unsigned (identity '-'); for local tests only" >&2
else
  productbuild --distribution "$tmp/Distribution" --package-path "$tmp/packages" \
    --resources "$tmp/resources" "$tmp/unsigned.pkg"
  productsign --sign "$identity" --timestamp "$tmp/unsigned.pkg" "$pkg"
  # Quiet: the output names the signer and Team ID, which stay out of the public CI logs.
  pkgutil --check-signature "$pkg" >/dev/null || fail "$pkg has no valid signature"
fi

(cd dist && shasum -a 256 "NovakeyR-$version.pkg" > "NovakeyR-$version.pkg.sha256")
echo "packaged $pkg"
