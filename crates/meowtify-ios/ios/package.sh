#!/usr/bin/env bash
# Wraps the iOS binary into an unsigned .ipa. Runs on a macOS machine, from the repo root.
# Usage: crates/meowtify-ios/ios/package.sh <binary> <version> <output.ipa>
# The .ipa is unsigned on purpose. Sideloadly, AltStore or SideStore sign it with the user's Apple ID.
set -euo pipefail

bin="$1"
version="$2"
out="$3"
here="$(cd "$(dirname "$0")" && pwd)"

work="$(mktemp -d)"
app="$work/Payload/Meowtify.app"
mkdir -p "$app"
cp "$bin" "$app/meowtify-ios"
chmod +x "$app/meowtify-ios"
sed "s/__VERSION__/$version/g" "$here/Info.plist" > "$app/Info.plist"
printf 'APPL????' > "$app/PkgInfo"

mkdir -p "$(dirname "$out")"
out_abs="$(cd "$(dirname "$out")" && pwd)/$(basename "$out")"
(cd "$work" && zip -qr "$out_abs" Payload)
rm -rf "$work"
echo "wrote $out_abs"
