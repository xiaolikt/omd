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
# Push a tag at the build commit before publishing.
#   packaging/release.sh              # publish v<Cargo.toml version>
#   packaging/release.sh --notes FILE # use custom release notes
#   packaging/release.sh --draft      # upload as a draft (not published yet)
#   packaging/release.sh --check      # validate assets without accessing GitHub
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
caller=$PWD
cd "$root"

repo=xiaolikt/omd
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$version" ] || { echo "cannot read the workspace version" >&2; exit 1; }
tag="v$version"

notes=""
draft=""
check=""
linux_only=""
prerelease=""
case "${version%%+*}" in *-*) prerelease=--prerelease ;; esac
while [ $# -gt 0 ]; do
    case "$1" in
        --notes)
            [ $# -ge 2 ] || { echo "missing file after --notes" >&2; exit 2; }
            case "$2" in /*) notes=$2 ;; *) notes=$caller/$2 ;; esac
            shift 2 ;;
        --draft) draft="--draft"; shift ;;
        --check) check=1; shift ;;
        --linux-only) linux_only=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

deb="packaging/dist/omd-desktop_${version}_amd64.deb"
zip="packaging/dist/omd-windows.zip"

if [ ! -f "$deb" ]; then
    echo "missing $deb — run packaging/build-deb.sh first" >&2
    exit 1
fi
commit=$(git rev-parse --verify HEAD)
if [ -z "$check" ]; then
    tree=$(git status --porcelain)
    if [ -n "$tree" ]; then
        printf 'working tree is dirty; commit before releasing\n%s\n' "$tree" >&2
        exit 1
    fi
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
marker="omd-build:$version:$commit"
[ "$(dpkg-deb -f "$deb" Package)" = omd-desktop ] &&
    [ "$(dpkg-deb -f "$deb" Version)" = "$version" ] &&
    [ "$(dpkg-deb -f "$deb" Architecture)" = amd64 ] || {
        echo "unexpected Debian package metadata in $deb" >&2; exit 1;
    }
dpkg-deb -x "$deb" "$work/deb"
lib="$work/deb/usr/lib/omd-desktop"
if ! LC_ALL=C grep -aqF -- "$marker" "$lib/omd" ||
    ! actual=$(timeout 10 env LD_LIBRARY_PATH="$lib" "$lib/omd" --build-info) ||
    [ "$actual" != "$marker" ]; then
    echo "$deb is not built from $commit at version $version; rebuild it" >&2
    exit 1
fi

set -- "$deb"
if [ -z "$linux_only" ] && [ -f "$zip" ]; then
    # Cross-platform validation: the same marker is embedded in the PE binary.
    if ! unzip -p "$zip" omd-windows/omd.exe > "$work/omd.exe" ||
        ! LC_ALL=C grep -aqF -- "$marker" "$work/omd.exe"; then
        echo "$zip is not built from $commit at version $version; rebuild it or pass --linux-only" >&2
        exit 1
    fi
    set -- "$@" "$zip"
fi
if [ -n "$check" ]; then
    printf 'verified %s:\n' "$marker"
    printf '  %s\n' "$@"
    exit 0
fi

remote=$(git ls-remote --exit-code "https://github.com/$repo.git" "refs/tags/$tag" "refs/tags/$tag^{}") || {
    echo "push $tag at $commit before publishing" >&2; exit 1;
}
# Prefer the peeled commit for an annotated tag; lightweight tags have one row.
if ! printf '%s\n' "$remote" | awk -v ref="refs/tags/$tag" -v commit="$commit" '
    $2 == ref { tag = $1 }
    $2 == ref "^{}" { peeled = $1 }
    END { exit ((peeled ? peeled : tag) != commit) }
'; then
    echo "remote $tag does not point to $commit" >&2
    exit 1
fi

if [ -z "$notes" ]; then
    notes=$work/notes
    cat > "$notes" <<EOF
OMD $version

- Self-update from GitHub Releases (Settings → General → Updates, or \`omd-desktop update\`)
- Package renamed to \`omd-desktop\` so distro upgrades can no longer replace it with
  Ubuntu's unrelated \`omd\` package

Linux: \`omd-desktop_${version}_amd64.deb\` (bundles CEF + libghostty-vt)
EOF
    if [ "$#" -gt 1 ]; then
        echo "Windows: \`omd-windows.zip\` (compile-time verified only — see packaging/windows/README.md)" >> "$notes"
    fi
fi

echo "publishing $tag"
gh release create "$tag" "$@" \
    --verify-tag \
    --repo "$repo" \
    --title "OMD $version" \
    --notes-file "$notes" \
    $draft $prerelease
