#!/bin/sh
# Build an amd64 .deb for the machine that compiled omd.
# The package bundles CEF and libghostty-vt next to the binary because CEF
# looks for icudtl.dat / *.pak / locales beside the executable, and both
# libraries are RUNPATH-linked to the dev tree otherwise.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
version=${OMD_VERSION:-0.1.0}
arch=amd64
dist="$root/packaging/dist"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT

cef=$(find "$root/vendor/cef" -type f -name libcef.so | head -1)
cef_release=$(dirname "$cef")
cef_root=$(dirname "$cef_release")
if [ ! -f "$root/target/release/omd" ]; then
    echo "missing $root/target/release/omd — build it first" >&2
    exit 1
fi
if [ ! -f "$cef" ]; then
    echo "missing libcef.so under vendor/cef" >&2
    exit 1
fi

lib="$stage/usr/lib/omd"
mkdir -p "$lib" "$stage/usr/bin" "$stage/usr/share/applications" \
    "$stage/DEBIAN" "$dist"

install -m 0755 "$root/target/release/omd" "$lib/omd"
# CEF resolves ICU and pak files relative to the executable, not resources_dir_path.
cp -a "$cef_release/." "$lib/"
rm -f "$lib/chrome-sandbox"
if [ -d "$cef_root/Resources/locales" ]; then
    mkdir -p "$lib/locales"
    cp -a "$cef_root/Resources/locales/." "$lib/locales/"
fi
cp -a "$root/vendor/libghostty-vt/lib/libghostty-vt.so.1.0.0" "$lib/"
ln -s libghostty-vt.so.1.0.0 "$lib/libghostty-vt.so.1"
ln -s libghostty-vt.so.1.0.0 "$lib/libghostty-vt.so"

cat > "$stage/usr/bin/omd" <<'EOF'
#!/bin/sh
export LD_LIBRARY_PATH="/usr/lib/omd${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec /usr/lib/omd/omd "$@"
EOF
chmod 0755 "$stage/usr/bin/omd"
install -m 0644 "$root/packaging/omd.desktop" "$stage/usr/share/applications/omd.desktop"

for size in 16 24 32 48 64 128 256 512; do
    dir="$stage/usr/share/icons/hicolor/${size}x${size}/apps"
    mkdir -p "$dir"
    install -m 0644 "$root/packaging/icons/omd-${size}.png" "$dir/omd.png"
done

cat > "$stage/DEBIAN/control" <<EOF
Package: omd
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: OMD <omd@localhost>
Depends: libc6, libgcc-s1, libx11-6, libxcb1, libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libfontconfig1, libfreetype6, libnss3, libnspr4, libdrm2, libgbm1, libpango-1.0-0, libcairo2, libdbus-1-3, libxcomposite1, libxdamage1, libxext6, libxfixes3, libxrandr2, libxi6, libglib2.0-0t64 | libglib2.0-0, libgtk-3-0t64 | libgtk-3-0, libatk1.0-0t64 | libatk1.0-0, libatk-bridge2.0-0t64 | libatk-bridge2.0-0, libasound2t64 | libasound2, libexpat1, libpng16-16t64 | libpng16-16
Recommends: nodejs
Description: Native desktop client for DeepSeek Harness
 OMD is a GPUI desktop client for the dsh engine. The first launch
 uses an already installed runtime under ~/.local/share/omd/runtime,
 or installs @deepseek-ai/dsh with npm when node is available.
EOF

cat > "$stage/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q /usr/share/icons/hicolor || true
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q /usr/share/applications || true
fi
EOF
chmod 0755 "$stage/DEBIAN/postinst"

out="$dist/omd_${version}_${arch}.deb"
rm -f "$out"
dpkg-deb --root-owner-group -Zxz -z6 --build "$stage" "$out"
echo "wrote $out"
dpkg-deb -I "$out"
