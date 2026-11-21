#!/bin/sh
# Install-layout smoke test. Does not dpkg -i into the real system.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
deb=$(ls -1 "$root/packaging/dist"/omd_*_amd64.deb | head -1)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

dpkg-deb -I "$deb" > "$work/info.txt"
dpkg-deb -c "$deb" > "$work/contents.txt"
grep -q 'Package: omd' "$work/info.txt"
grep -q './usr/bin/omd' "$work/contents.txt"
grep -q './usr/lib/omd/libcef.so' "$work/contents.txt"
grep -q './usr/lib/omd/libghostty-vt.so.1' "$work/contents.txt"
grep -q './usr/share/icons/hicolor/256x256/apps/omd.png' "$work/contents.txt"
grep -q './usr/lib/omd/icudtl.dat' "$work/contents.txt"
grep -q './usr/lib/omd/locales/' "$work/contents.txt"
dpkg-deb -x "$deb" "$work"

# The wrapper must resolve both private libraries without the dev-tree RUNPATH.
if ! LD_LIBRARY_PATH="$work/usr/lib/omd" ldd "$work/usr/lib/omd/omd" | grep -q 'libcef.so =>'; then
    echo "libcef.so did not resolve from the package lib dir" >&2
    LD_LIBRARY_PATH="$work/usr/lib/omd" ldd "$work/usr/lib/omd/omd" || true
    exit 1
fi
if LD_LIBRARY_PATH="$work/usr/lib/omd" ldd "$work/usr/lib/omd/omd" | grep -q 'not found'; then
    echo "packaged binary has unresolved libraries" >&2
    LD_LIBRARY_PATH="$work/usr/lib/omd" ldd "$work/usr/lib/omd/omd"
    exit 1
fi

# Launch from the extracted layout. timeout 124 is the expected GUI hang.
log=$work/smoke.log
set +e
timeout 12 env LD_LIBRARY_PATH="$work/usr/lib/omd" "$work/usr/lib/omd/omd" >"$log" 2>&1
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
