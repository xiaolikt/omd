#!/bin/sh
# Publish a GitHub release from the locally built packages.
#
# The in-app updater (Settings → General → Updates, and `omd-desktop update`)
# reads `releases/latest` from github.com/xiaolikt/omd and picks the asset by
# name, so the names must stay:
#   Linux:   omd-desktop_<version>_amd64.deb
#   Windows: omd-windows.zip
#
# Usage:
#   packaging/release.sh              # publish v<Cargo.toml version>
#   packaging/release.sh --notes FILE # use custom release notes
#   packaging/release.sh --draft      # upload as a draft (not published yet)
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$version" ] || { echo "cannot read the workspace version" >&2; exit 1; }
tag="v$version"

notes=""
draft=""
while [ $# -gt 0 ]; do
    case "$1" in
        --notes) notes=$2; shift 2 ;;
        --draft) draft="--draft"; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

deb="packaging/dist/omd-desktop_${version}_amd64.deb"
zip="packaging/dist/omd-windows.zip"

if [ ! -f "$deb" ]; then
    echo "missing $deb — run packaging/build-deb.sh first" >&2
    exit 1
fi
if [ -n "$(git status --porcelain)" ]; then
    echo "working tree is dirty; commit before releasing" >&2
    git status --short >&2
    exit 1
fi

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
if [ -n "$notes" ]; then
    cp "$notes" "$tmp"
else
    cat > "$tmp" <<EOF
OMD $version

- Self-update from GitHub Releases (Settings → General → Updates, or \`omd-desktop update\`)
- Package renamed to \`omd-desktop\` so distro upgrades can no longer replace it with
  Ubuntu's unrelated \`omd\` package

Linux: \`omd-desktop_${version}_amd64.deb\` (bundles CEF + libghostty-vt)
EOF
    if [ -f "$zip" ]; then
        echo "Windows: \`omd-windows.zip\` (compile-time verified only — see packaging/windows/README.md)" >> "$tmp"
    fi
fi

set -- "$deb"
[ -f "$zip" ] && set -- "$@" "$zip"

echo "publishing $tag"
gh release create "$tag" "$@" \
    --repo xiaolikt/omd \
    --title "OMD $version" \
    --notes-file "$tmp" \
    $draft
echo "done: https://github.com/xiaolikt/omd/releases/tag/$tag"
