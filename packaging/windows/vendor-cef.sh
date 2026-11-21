#!/bin/sh
# Refresh the vendored Windows CEF distribution under vendor/cef.
#
# cef-dll-sys downloads CEF by fetching cef-builds' index.json, which is now
# larger than the 10 MB response limit baked into its HTTP client. Vendoring
# the archive (like the Linux one next to it) sidesteps the index entirely:
# CEF_PATH=<repo>/vendor/cef makes the build script read the versioned dir
# and its archive.json without any network access.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
version=156.0.26
name="cef_binary_${version}+g8ea0c73+chromium-156.0.8078.88_windows64_minimal.tar.bz2"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

curl -fsS -o "$work/$name" "https://cef-builds.spotifycdn.com/$name"
echo "15d29eea209d857c22dc163706080295aea69b5c  $work/$name" | sha1sum -c -

tar -xjf "$work/$name" -C "$work"
dest="$root/vendor/cef/$version/cef_windows_x86_64"
rm -rf "$dest"
src="$work/cef_binary_${version}+g8ea0c73+chromium-156.0.8078.88_windows64_minimal"
# Same flattening download-cef's extract step does: Release/ becomes the dir
# root and Resources/ merges into it, so libcef.dll and locales/ end up next
# to each other where cef-dll-sys expects them.
mkdir -p "$dest"
mv "$src"/Release/* "$dest"/
rmdir "$src"/Release
mv "$src"/Resources/* "$dest"/
rmdir "$src"/Resources
mv "$src"/CMakeLists.txt "$src"/cmake "$src"/include "$src"/libcef_dll "$dest"/
rm -rf "$work/${src##*/}"
cat > "$dest/archive.json" <<EOF
{
  "type": "minimal",
  "name": "$name",
  "sha1": "15d29eea209d857c22dc163706080295aea69b5c"
}
EOF
echo "vendored $dest"
