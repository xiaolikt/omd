#!/bin/sh
# Install OMD into the user's own prefix (default ~/.local) — no root needed.
#
# Why this exists: the .deb is fine on machines where you control dpkg, but the
# package *name* has to stay out of the way of distro packages (Ubuntu ships an
# unrelated `omd`, the OCaml Markdown frontend, and a routine `apt upgrade`
# replaced ours with it on 2026-11-21). A user-local install lives in ~/.local
# and is therefore invisible to apt, Software Updater and any distro upgrade.
#
# Usage:
#   packaging/install-user.sh                 # install the newest built .deb
#   packaging/install-user.sh --deb <file>    # install a specific .deb
#   OMD_PREFIX=/some/prefix packaging/install-user.sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
prefix=${OMD_PREFIX:-$HOME/.local}
deb=""

while [ $# -gt 0 ]; do
    case "$1" in
        --deb) deb=$2; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$deb" ]; then
    deb=$(ls -1t "$root"/packaging/dist/omd-desktop_*_amd64.deb 2>/dev/null | head -1)
fi
if [ -z "$deb" ] || [ ! -f "$deb" ]; then
    echo "no omd-desktop .deb found; run packaging/build-deb.sh first" >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
dpkg-deb -x "$deb" "$work"

payload=$work/usr/lib/omd-desktop
if [ ! -x "$payload/omd" ]; then
    echo "unexpected layout in $deb (no usr/lib/omd-desktop/omd)" >&2
    exit 1
fi

share=$prefix/share
opt=$prefix/opt/omd-desktop
bin=$prefix/bin
apps=$share/applications

# Flatten the payload into <prefix>/opt/omd-desktop so CEF keeps finding
# icudtl.dat / *.pak / locales next to the executable.
rm -rf "$opt"
mkdir -p "$opt" "$bin" "$apps"
cp -a "$payload/." "$opt/"

cat > "$bin/omd-desktop" <<EOF
#!/bin/sh
export LD_LIBRARY_PATH="$opt\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
exec "$opt/omd" "\$@"
EOF
chmod 0755 "$bin/omd-desktop"

for size in 16 24 32 48 64 128 256 512; do
    src=$work/usr/share/icons/hicolor/${size}x${size}/apps/omd.png
    [ -f "$src" ] || continue
    dir=$share/icons/hicolor/${size}x${size}/apps
    mkdir -p "$dir"
    install -m 0644 "$src" "$dir/omd.png"
done

cat > "$apps/omd.desktop" <<EOF
[Desktop Entry]
Name=OMD
GenericName=DeepSeek Harness
Comment=Native desktop client for DeepSeek Harness
Exec=$bin/omd-desktop
Icon=omd
Terminal=false
Type=Application
Categories=Development;Utility;
StartupWMClass=omd
Keywords=dsh;deepseek;agent;terminal;
EOF

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$apps" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q "$share/icons/hicolor" 2>/dev/null || true
fi

version=$(dpkg-deb -f "$deb" Version)
echo "installed omd-desktop $version"
echo "  binary:  $bin/omd-desktop"
echo "  desktop: $apps/omd.desktop"
echo "  data:    ${XDG_DATA_HOME:-$HOME/.local/share}/omd (unchanged)"
