# Windows exe — cross-built from Linux

The build now produces a real `omd.exe` from this Linux tree. It is
compile-verified only: it has never been run on Windows here, so treat it
as a preview artifact.

## Toolchain (all without root)

| Piece | How |
| --- | --- |
| Rust target | `rustup target add x86_64-pc-windows-msvc` |
| cargo-xwin | `cargo install cargo-xwin --locked` |
| clang-cl / lld-link | `apt-get download clang-21 lld-21 libclang-common-21-dev`, then `dpkg-deb -x` them and put `/usr/lib/llvm-21/bin` on PATH (`libclang-cpp-21` must already be installed). `libclang-common-21-dev` carries clang's builtin headers — without it `ring` fails on `stdalign.h`. |
| llvm-lib | already in `/usr/lib/llvm-20/bin` |
| cmake / ninja | `python3 -m venv` + `pip install cmake ninja` |
| ghostty-vt.dll | vendored; rebuilt per `vendor/libghostty-vt/windows/README.md` |

## Build

```sh
packaging/windows/build.sh     # -> packaging/dist/omd-windows.zip
```

First run downloads the MSVC CRT/SDK (cargo-xwin). The Windows CEF
distribution is vendored under `vendor/cef/156.0.26/cef_windows_x86_64`
(built the same way as the Linux one; `packaging/windows/vendor-cef.sh`
refreshes it) because the crate's index download now exceeds its own
10 MB response limit.

## What is ported

- `dsh-host`: data dirs under `%LOCALAPPDATA%`, detached daemon via
  `CREATE_NEW_PROCESS_GROUP`, `taskkill` for stop/alive checks, `node.exe`
  and `npm.cmd` names.
- `omd` app: browser blocks run through CEF off-screen rendering (the same
  path as Wayland), pixel upload instead of DMA-BUF; `x11_embed` and the
  fontconfig/GPU-pinning startup hooks stay Linux-only.

## Known gaps on Windows

- The block control socket (`omd block ...`, `TERMINAL_BROWSER_TERMINAL_SOCKET`)
  is a Unix-domain socket and is not started; agent-created terminal/browser
  blocks fall back to the engine's own creation path.
- CEF shared-texture import (D3D11) is not wired; the browser renders via
  CPU pixel upload.
- IME, clipboard and popup behaviour are unverified.
- No installer, no code signing; the zip must be unpacked and run from a
  writable directory (CEF cache lives next to the exe).
