# libghostty-vt for Windows

`ghostty-vt.dll` and its import library are built from the official
libghostty-vt 1.0.0 release (the same version as the Linux `.so` in `../lib`).

```sh
curl -fsSLO https://release.files.ghostty.org/libghostty-vt/1.0.0/libghostty-vt-1.0.0.tar.gz
curl -fsSLO https://release.files.ghostty.org/libghostty-vt/1.0.0/libghostty-vt-1.0.0-deps.tar.gz
tar -xzf libghostty-vt-1.0.0.tar.gz
tar -xzf libghostty-vt-1.0.0-deps.tar.gz
cd libghostty-vt-1.0.0
DESTDIR=$PWD/out ZIG_GLOBAL_CACHE_DIR=$PWD/cache \
  zig build --prefix /usr --system ../libghostty-vt-1.0.0-deps \
  -Doptimize=ReleaseFast -Dcpu=baseline -Dtarget=x86_64-windows-gnu
```

Zig 0.17.0 is required. The `x86_64-windows-gnu` target is deliberate: Zig
ships MinGW headers but not the MSVC CRT, and the library's simdutf/highway
sources need a full C++ standard library. The result is a plain C DLL with
the Microsoft x64 ABI that depends only on the UCRT (`api-ms-win-crt-*`),
`KERNEL32.dll` and `ntdll.dll` — no MinGW runtime DLLs.

The Rust build links it with the MSVC toolchain, so the import library is
regenerated in MSVC format instead of using Zig's GNU `.lib`:

```sh
llvm-readobj --coff-exports out/usr/bin/ghostty-vt.dll |
  awk '/Name: /{print $2}' | sort -u > exports.txt
{ echo "LIBRARY ghostty-vt.dll"; echo "EXPORTS"; sed 's/^/  /' exports.txt; } > ghostty-vt.def
llvm-dlltool -m i386:x86-64 -d ghostty-vt.def -l ghostty-vt.lib
```
