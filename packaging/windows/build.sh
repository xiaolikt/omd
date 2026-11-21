#!/bin/sh
# Honest Windows packaging check. This does not emit an exe.
set -eu
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
ico="$root/packaging/icons/omd.ico"
if [ ! -f "$ico" ]; then
    echo "missing $ico" >&2
    exit 1
fi
cat >&2 <<EOF
omd Windows exe: not built.

vendor/cef has Linux binaries only (libcef.so), the app links
libghostty-vt.so.1, and browser/x11 embedding is cfg(linux).
This machine's rustc target is x86_64-unknown-linux-gnu; there is
no MinGW sysroot and no Windows CEF redistributable.

Icon for a future Windows build: $ico
EOF
exit 2
