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
case "$prefix" in /*) ;; *) prefix=$PWD/$prefix ;; esac
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

share=$prefix/share
if [ -z "${OMD_PREFIX:-}" ]; then
    case "${XDG_DATA_HOME:-}" in /*) share=$XDG_DATA_HOME ;; esac
fi
opt=$prefix/opt/omd-desktop
bin=$prefix/bin
apps=$share/applications
backup=$prefix/opt/.omd-desktop.previous
mkdir -p "$prefix/opt" "$bin" "$apps"
exec 9>>"$prefix/opt/.omd-desktop.lock"
flock -n 9 || { echo "another user installation is in progress" >&2; exit 1; }
work=$(mktemp -d "$prefix/opt/.omd-install.XXXXXXXX")
wrapper=""
desktop=""
cleanup() {
    if [ -d "$backup" ] && [ ! -e "$opt" ] && [ ! -L "$opt" ]; then
        mv -T -- "$backup" "$opt" || echo "previous installation kept at $backup" >&2
    fi
    rm -rf -- "$work"
    [ -z "$wrapper" ] || rm -f -- "$wrapper"
    [ -z "$desktop" ] || rm -f -- "$desktop"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
dpkg-deb -x "$deb" "$work"

payload=$work/usr/lib/omd-desktop
if [ ! -f "$payload/omd" ] || [ ! -x "$payload/omd" ] || [ -L "$payload/omd" ]; then
    echo "unexpected layout in $deb (no usr/lib/omd-desktop/omd)" >&2
    exit 1
fi

wrapper=$(mktemp "$bin/.omd-launcher.XXXXXXXX")
cat > "$wrapper" <<'EOF'
#!/bin/sh
dir=$(CDPATH= cd -- "$(dirname -- "$0")/../opt/omd-desktop" && pwd) || exit 1
export LD_LIBRARY_PATH="$dir${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
exec "$dir/omd" "$@"
EOF
chmod 0755 "$wrapper"

for src in "$work"/usr/share/icons/hicolor/*/apps/omd-desktop.png "$work"/usr/share/icons/hicolor/*/apps/omd.png; do
    [ -f "$src" ] || continue
    size=$(basename -- "$(dirname -- "$(dirname -- "$src")")")
    dir=$share/icons/hicolor/$size/apps
    mkdir -p "$dir"
    install -m 0644 "$src" "$dir/omd-desktop.png"
done

desktop=$(mktemp "$apps/.omd-desktop.XXXXXXXX")
exec_path=$(printf '%s' "$bin/omd-desktop" | sed 's/\\/\\\\\\\\/g; s/["$`]/\\\\&/g; s/%/%%/g')
{
    sed '/^Exec=/d' "$root/packaging/omd.desktop"
    printf 'Exec="%s"\n' "$exec_path"
} > "$desktop"
chmod 0644 "$desktop"

# Even on systems without mv --exchange, never discard the previous payload
# until the new tree is complete. The trap restores it if activation fails.
if [ -d "$backup" ] && [ ! -e "$opt" ] && [ ! -L "$opt" ]; then
    mv -T -- "$backup" "$opt"
fi
if [ -e "$opt" ] || [ -L "$opt" ]; then
    if mv --help | grep -q -- --exchange; then
        mv --exchange --no-copy -T -- "$payload" "$opt"
    else
        rm -rf -- "$backup"
        mv -T -- "$opt" "$backup"
        mv -T -- "$payload" "$opt"
        rm -rf -- "$backup"
    fi
else
    mv -T -- "$payload" "$opt"
fi
mv -fT -- "$wrapper" "$bin/omd-desktop"
mv -fT -- "$desktop" "$apps/omd-desktop.desktop"

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database -q "$apps" 2>/dev/null || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -q "$share/icons/hicolor" 2>/dev/null || true
fi

version=$(dpkg-deb -f "$deb" Version)
echo "installed omd-desktop $version"
echo "  binary:  $bin/omd-desktop"
echo "  desktop: $apps/omd-desktop.desktop"
echo "  data:    ${XDG_DATA_HOME:-$HOME/.local/share}/omd (unchanged)"
