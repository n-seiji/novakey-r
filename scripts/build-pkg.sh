#!/usr/bin/env bash
# Build dist/Shizuku-<version>.pkg (plus a .sha256 file) from the already signed dist/Shizuku.app
# (in CI also notarized and stapled). The package installs into the current user's home only
# (~/Library/Input Methods), so it needs no admin password. It updates an existing bundle in place,
# which keeps Shizuku usable in Input Sources (see component.plist below). Never modifies the app.
# Usage: scripts/build-pkg.sh <identity>   (a "Developer ID Installer: ..." identity, or "-" to
#        leave the package unsigned for local tests)
set -euo pipefail

cd "$(dirname "$0")/.."
APP=dist/Shizuku.app
PKG_ID=dev.n-seiji.inputmethod.Shizuku.pkg

fail() {
  echo "build-pkg: $*" >&2
  exit 1
}

(($# == 1)) || fail "usage: scripts/build-pkg.sh <identity>"
identity=$1
[[ -d "$APP" ]] || fail "$APP not found (run stage-app.sh and sign-app.sh first)"

version=$(/usr/libexec/PlistBuddy -c 'Print :CFBundleShortVersionString' "$APP/Contents/Info.plist")
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version '$version' in $APP is not X.Y.Z"
pkg=dist/Shizuku-$version.pkg

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$tmp/root" "$tmp/packages" "$tmp/resources" "$tmp/scripts"
# The payload root stands for ~/Library/Input Methods; keep it 0755 whatever the umask.
chmod 755 "$tmp/root"
# ditto keeps the signature, the stapled ticket and extended attributes.
ditto "$APP" "$tmp/root/Shizuku.app"

# An in-place update leaves files that the new version no longer ships, and an extra file breaks the
# bundle's code signature seal, so the postinstall removes everything not in this list.
(cd "$tmp/root/Shizuku.app" && find . -type f | sed 's|^\./||') > "$tmp/scripts/payload-files.txt"
[[ -s "$tmp/scripts/payload-files.txt" ]] || fail "empty payload"

# After the files are updated, remove stale files and stop this user's running Shizuku so the next
# key launches the new binary (macOS starts an input method on demand). Never fails the install.
cat > "$tmp/scripts/postinstall" <<'SH'
#!/bin/sh
list="$(/usr/bin/dirname "$0")/payload-files.txt"
app="$2/Shizuku.app"
[ -d "$app" ] || app="$HOME/Library/Input Methods/Shizuku.app"
if [ -s "$list" ] && [ -d "$app/Contents" ]; then
  /usr/bin/find "$app" -type f | while IFS= read -r file; do
    /usr/bin/grep -Fxq -- "${file#"$app"/}" "$list" || /bin/rm -f "$file"
  done
  /usr/bin/find "$app" -type d -empty -delete
fi
/usr/bin/pkill -x -u "$(/usr/bin/id -u)" shizuku || true
exit 0
SH
chmod 755 "$tmp/scripts/postinstall"

# No bundle entries: the installer then just writes the files. With a bundle entry, the default
# action ("upgrade") swaps the whole bundle, and macOS stops offering the input method until it is
# removed and added again in System Settings (seen on macOS 26); "update" keeps the bundle but
# installs nothing when there is none yet. Without entries the package is also not relocatable.
cat > "$tmp/component.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array/></plist>
PLIST

# The install location is relative to the chosen domain: the user's home (see Distribution).
pkgbuild --root "$tmp/root" \
  --component-plist "$tmp/component.plist" \
  --identifier "$PKG_ID" \
  --version "$version" \
  --install-location "/Library/Input Methods" \
  --scripts "$tmp/scripts" \
  "$tmp/packages/Shizuku.pkg"

cat > "$tmp/resources/conclusion.txt" <<'TXT'
Shizuku is installed in ~/Library/Input Methods.

Add Shizuku in System Settings > Keyboard > Input Sources. If it is not listed yet, log out and back in first.

To update later, open the newer Shizuku package over this one. The running Shizuku is restarted, so the new version is used from the next key.
TXT

cat > "$tmp/Distribution" <<XML
<?xml version="1.0" encoding="utf-8"?>
<installer-gui-script minSpecVersion="2">
    <title>Shizuku</title>
    <options customize="never" require-scripts="true" hostArchitectures="arm64"/>
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
    <pkg-ref id="$PKG_ID" version="$version" onConclusion="none">Shizuku.pkg</pkg-ref>
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

(cd dist && shasum -a 256 "Shizuku-$version.pkg" > "Shizuku-$version.pkg.sha256")
echo "packaged $pkg"
