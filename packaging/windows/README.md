# Windows exe

This tree cannot produce a working Windows executable.

- `vendor/cef` only contains `cef_linux_x86_64`. The binary links `libcef.so`.
- `libghostty-vt.so.1` is a Linux shared library.
- `crates/app/src/browser.rs` and `x11_embed.rs` are `cfg(target_os = "linux")`.
- This machine has the `x86_64-unknown-linux-gnu` rustc target only. There is no
  MinGW sysroot and no Windows CEF redistributable.

`packaging/windows/build.sh` exits 2 for that reason. It does write `omd.ico`
from the same icon used by the deb, so a future Windows build has a mark.
Do not ship a stub exe in its place.
