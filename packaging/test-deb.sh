#!/bin/sh
# Install-layout smoke test. Does not dpkg -i into the real system.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
deb=${1:-"$root/packaging/dist/omd-desktop_${version}_amd64.deb"}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

dpkg-deb -I "$deb" > "$work/info.txt"
dpkg-deb -c "$deb" > "$work/contents.txt"
grep -q 'Package: omd-desktop' "$work/info.txt"
grep -q './usr/bin/omd-desktop' "$work/contents.txt"
grep -q './usr/lib/omd-desktop/libcef.so' "$work/contents.txt"
grep -q './usr/lib/omd-desktop/libghostty-vt.so.1' "$work/contents.txt"
grep -q './usr/share/applications/omd-desktop.desktop' "$work/contents.txt"
grep -q './usr/share/icons/hicolor/256x256/apps/omd-desktop.png' "$work/contents.txt"
grep -q './usr/lib/omd-desktop/icudtl.dat' "$work/contents.txt"
grep -q './usr/lib/omd-desktop/locales/' "$work/contents.txt"
dpkg-deb -x "$deb" "$work"
lib=$work/usr/lib/omd-desktop
LC_ALL=C grep -aqF 'omd-build:' "$lib/omd" || {
    echo "packaged binary predates the update CLI" >&2
    exit 1
}
actual=$(timeout 8 env LD_LIBRARY_PATH="$lib" "$lib/omd" --version)
[ "$actual" = "omd $(dpkg-deb -f "$deb" Version)" ] || {
    echo "packaged binary and Debian package versions differ" >&2
    exit 1
}
timeout 8 env LD_LIBRARY_PATH="$lib" "$lib/omd" update --help | grep -q 'update \[--check'

# The wrapper must resolve both private libraries without the dev-tree RUNPATH.
if ! LD_LIBRARY_PATH="$work/usr/lib/omd-desktop" ldd "$work/usr/lib/omd-desktop/omd" | grep -q 'libcef.so =>'; then
    echo "libcef.so did not resolve from the package lib dir" >&2
    LD_LIBRARY_PATH="$work/usr/lib/omd-desktop" ldd "$work/usr/lib/omd-desktop/omd" || true
    exit 1
fi
if LD_LIBRARY_PATH="$work/usr/lib/omd-desktop" ldd "$work/usr/lib/omd-desktop/omd" | grep -q 'not found'; then
    echo "packaged binary has unresolved libraries" >&2
    LD_LIBRARY_PATH="$work/usr/lib/omd-desktop" ldd "$work/usr/lib/omd-desktop/omd"
    exit 1
fi

# Launch from the extracted layout. timeout 124 is the expected GUI hang.
log=$work/smoke.log
set +e
mkdir -p "$work/data" "$work/config" "$work/cache"
timeout 12 env XDG_DATA_HOME="$work/data" XDG_CONFIG_HOME="$work/config" XDG_CACHE_HOME="$work/cache" \
    OMD_NO_UPDATE_CHECK=1 DSH_GUI_NO_AUTO_INSTALL=1 LD_LIBRARY_PATH="$lib" "$lib/omd" >"$log" 2>&1
code=$?
set -e
if [ "$code" -ne 124 ]; then
    echo "expected timeout 124 from a live window, got $code" >&2
    tail -40 "$log" >&2
    exit 1
fi
if grep -q panic "$log"; then
    echo "packaged binary panicked" >&2
    grep panic "$log" >&2
    exit 1
fi
echo "deb smoke ok: $deb"
