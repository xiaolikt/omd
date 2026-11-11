# 04 · 终端栈（herdr 同款）

## 结论：herdr 的终端 = PTY + Ghostty 的 `libghostty-vt` + GPUI 自绘

不是 alacritty_terminal、不是 wezterm-term、不是 xterm.js（这三个在 herdr 二进制里都查不到）。

### 证据（直接来自二进制）

```sh
$ strings ~/.local/bin/herdr-core | grep -i ghostty
ghostty_terminal_new      ghostty_cell_get_multi     ghostty_build_info
ghostty_alloc             ghostty_sgr_set_params     ghostty_color_palette_*
libghostty
==== Ghostty's libghostty-vt (MIT)
Copyright (c) 2024 Mitchell Hashimoto, Ghostty contributors
==== simdutf, in libghostty-vt (MIT)
==== Highway, in libghostty-vt (BSD-3-Clause)
```

其他证据：

- `kitty_gfx`、`KITTY_GRAPHICS`、`KITTY_IMAGE`、图片逐出 / 动画帧 / kitty 颜色协议 /
  kitty 键盘协议 / kitty 剪贴板 / kitty DnD → 全部来自 libghostty-vt。
- `ghostty_cell_get` / `ghostty_cell_get_multi` → **cell 级 API**：VT 状态在 core，App 拉格子。
- `herdr-app` 里没有 ghostty 符号，只有 `gpui::*`（`elements::div`、`style::TextOverflow`、
  `ScrollWheelEvent`…）→ 渲染是 App 用 GPUI 自绘。
- PTY 是 OS 原生：`conpty.dll`（Windows 下载）、`win32-input-mode`（ConPTY 会设置）。
- 滚动回滚在 core 里（`failed to restore compressed terminal page` → 回滚页压缩存储）。

### 架构含义

```
herdr-core（headless，常驻）
  ├─ OS PTY（真 openpty / ConPTY）
  ├─ libghostty-vt（VT 解析 + 屏幕/回滚 + kitty 图形状态）
  └─ 对外：cell grid / 输出 / 输入 / resize（socket API：hc）
herdr-app（GPUI 窗口）
  └─ 拉 cell → 画文本 run + 图片层 + 滚动条 + 选择
```

好处：
- 关窗/重启终端不丢（core 持有状态）——和 tern 的 daemon 模型一致。
- `hc read t3` 能直接读屏（core 有屏幕模型）。
- 渲染完全自由（主题、字体、对比度、透明度都是 App 的事）。

## 对本项目的落地方式

> herdr 的应用层也开源（Apache-2.0），但**v1 不嵌 herdr-core**：
> 它的公开 socket API 是文本级（`pane.read` 只有 `text|ansi`），不是 PTY 字节流。
> 详见 [08-build-on-herdr.md](08-build-on-herdr.md)。

### 分期

| 阶段 | VT | 能跑什么 |
|---|---|---|
| v1（M2） | `alacritty_terminal` | vim、lazygit、shell；无 Kitty 图形 |
| 以后 | `libghostty-vt` | 加 Kitty 图形（yazi 预览、`kitten icat`）、Kitty 键盘协议 |

下面两种方案是终局对比，v1 走 alacritty_terminal。

### 方案 A（herdr 的做法，终局）

- `crates/term`：FFI 绑定 `libghostty-vt`（MIT，C ABI；Zig 实现，有头文件）。
  - 建立 terminal：`ghostty_terminal_new`，喂 PTY 字节，取 cell（`ghostty_cell_get_multi`）。
  - 图片：libghostty-vt 已实现 Kitty 图形协议（含 Unicode placeholder），
    我们只需把图片对象合成到 cell 层。
- PTY 来自 dsh：`terminal/attach` 流（字节 + `write`/`resize` 上行），
  或本地自己 spawn（若某天需要本地 shell）。
- 渲染：GPUI 里按 cell 画 `shape_line` 文本 run + 图片层 + 自绘滚动条/选择/搜索。

### 方案 B（省事）

- `alacritty_terminal`（Zed 使用）：VTE 解析 + 网格成熟，Rust 原生；
  但 Kitty 图形协议要自己补（Yazi、图片预览会缺），kitty 键盘协议也要自己接。

### 对比

| | libghostty-vt | alacritty_terminal |
|---|---|---|
| 语言/集成 | C ABI（FFI），Zig 实现 | 纯 Rust crate |
| VT 覆盖 | Ghostty 同款（最全） | 很全 |
| Kitty 图形协议 | ✅ 原生 | ❌ 需自实现 |
| Kitty 键盘协议 | ✅ | ⚠️ 部分 |
| 滚动回滚/压缩 | ✅（core 侧） | ✅ |
| 许可证 | MIT | Apache-2.0/MIT |
| 与 herdr 一致性 | 完全一致 | 需自证 |

## 需要 App 自己做的部分（herdr 也是自研）

- cell → GPUI 文本 run 的批量成型（性能关键：按行合并 style run）。
- 光标/选择/复制（Kitty 键盘模式、bracketed paste、OSC 52 剪贴板的策略开关）。
- 搜索（屏 + 回滚）、滚动条命中区。
- 图片合成（Kitty 图片在 cell 坐标系里的定位/滚动/裁剪）。
- 透明度/对比度/字体连字选项。

## 参考行为（来自 herdr 的 AGENTS.md，可作为验收清单）

- Kitty 图形（含 yazi 的 Unicode placeholder）正常显示，部分图片与滚动跟随 placeholder。
- 备用屏幕（vim/less）无回滚、滚动不生效。
- 程序接管鼠标时仍收鼠标事件，Shift 选择文本。
- Kitty 键盘模式协商（含 Windows 上 Escape 与后续键分离）。
- 搜索覆盖屏幕 + 回滚，且不破坏文本选择。
