#!/bin/sh
# Cross-build the Windows exe from Linux with cargo-xwin.
#
# Toolchain this script expects on PATH (see README.md for how to get each
# without root):
#   clang-cl, lld-link, llvm-lib, cmake, ninja, cargo-xwin
# plus `rustup target add x86_64-pc-windows-msvc`.
#
# The MSVC CRT/SDK and the Windows CEF distribution are downloaded on first
# use (~1 GB combined). ghostty-vt.dll is vendored, built per
# vendor/libghostty-vt/windows/README.md.
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
target=x86_64-pc-windows-msvc
out="$root/packaging/dist/omd-windows"

command -v cargo-xwin >/dev/null 2>&1 || { echo "cargo-xwin missing (cargo install cargo-xwin)" >&2; exit 1; }
for tool in clang-cl lld-link llvm-lib cmake ninja; do
    command -v "$tool" >/dev/null 2>&1 || { echo "$tool missing on PATH" >&2; exit 1; }
done

cd "$root"
CEF_PATH="$root/vendor/cef" cargo xwin build --profile winrelease --target "$target" -p omd

rm -rf "$out"
mkdir -p "$out"
stage="$root/target/$target/winrelease"
cp "$stage/omd.exe" "$out/"
# CEF 运行时（cef-dll-sys 把它们拷到 build/ 下，这里直接取 vendored 源）。
cef="$root/vendor/cef/156.0.26/cef_windows_x86_64"
for f in libcef.dll chrome_elf.dll d3dcompiler_47.dll dxcompiler.dll \
         vk_swiftshader.dll vk_swiftshader_icd.json vulkan-1.dll \
         icudtl.dat resources.pak chrome_100_percent.pak chrome_200_percent.pak \
         v8_context_snapshot.bin; do
    [ -f "$cef/$f" ] && cp "$cef/$f" "$out/"
done
cp -a "$cef/locales" "$out/"
cp "$root/vendor/libghostty-vt/windows/ghostty-vt.dll" "$out/"

work=$(mktemp -d "$(dirname "$out")/.omd-windows.XXXXXXXX")
trap 'rm -rf "$work"' EXIT
(cd "$(dirname "$out")" && zip -q -r "$work/omd-windows.zip" "$(basename "$out")")
mv -f -- "$work/omd-windows.zip" "$out.zip"
echo "wrote $out.zip"
ls -lh "$out.zip"
