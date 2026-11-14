# OMD — Rust 原生 GUI 的 DeepSeek Harness（dsh）客户端

> 研究结论文档集。目标：用 Rust 写一个原生 GPU 渲染的 dsh 桌面 App，
> 支持 dsh 全部功能（会话、工具、审批、终端、文件、插件安装、市场、账号、计划/目标、子代理…），
> UI 层不出现 Node/浏览器。

## 一句话结论

**可行。** dsh 的官方 web 前端只是一个「渲染器」，底层能力面是一组结构化 RPC
（Typert gateway，实测 144 个端点，带 JSON Schema）；把这层 RPC 实现到 Rust 侧，
就等于拿到了 100% 功能。引擎（Node）以 headless 子进程/服务形式运行，
UI 层 100% Rust + GPUI，与 herdr 同技术栈。

## 关键事实

| 问题 | 结论 |
|---|---|
| dsh 是什么 | `@deepseek-ai/dsh` = **DeepSeek Harness**，MIT，`deepseek-ai/deepseek-harness`，TypeScript + cordis DI，307 个包 |
| 多 UI 形态 | `dsh web`（浏览器）/ `dsh tui`（社区 TUI）/ `dsh headless` / `--profile acp` / `--profile sdk` |
| 哪条路功能完整 | **Typert gateway**（web 前端用的同一套 API）；ACP 是阉割版；SDK stdio 无审批/终端/插件 |
| 插件安装 | 网关里有 `pluginManager/*`（12 个方法）+ `pluginMarket/*` + `pluginInventory/list`，不需要 shell 调 pnpm |
| UI 框架 | GPUI / gpui-kit（herdr 同栈，Apache-2.0，75+ 组件：dock、markdown、diff、DataTable、tree-sitter 编辑器） |
| 终端 | herdr 用 **PTY + Ghostty 的 `libghostty-vt` + GPUI 自绘 cell**（非 alacritty/wezterm/xterm.js）。本项目 v1 用 `alacritty_terminal`，Kitty 图形再换 libghostty-vt |
| 和 herdr 的关系 | 学它的模式（GPU 壳 + 引擎子进程），**v1 不嵌 herdr-core**。它的公开 API 只有 ANSI 快照，不能当终端字节流。dsh 的 `terminal/attach` 才是真 PTY |
| 图片 | 终端块走 Kitty 图形协议（libghostty-vt 自带）；App 内容走 GPUI 原生图片管线；dsh 图片是 `session/attachment` |
| 表格 | 不要 TSP 也行：dsh 的表格数据来自结构化事件，用 gpui-kit DataTable 直接画；tern 的表格是它自己的 Surface Protocol（节点树），与终端无关 |

## 当前状态：第一版（M0 + M1 主链路）

已经能跑：

```sh
# 需要全局 dsh，或用 DSH_BIN 指向 npx 缓存里的 bin.js
DSH_BIN=$HOME/.npm/_npx/1e7f6d9597241db0/node_modules/@deepseek-ai/dsh/lib/bin.js \
  cargo run -p omd
```

窗口里：左侧真实会话列表（可点击切换）、右侧消息流（历史 records + 实时事件）、底部输入框（Enter 发送）。

**已验证**（`crates/spike` + GUI 双向确认）：

| 能力 | 状态 |
|---|---|
| 启动 host + 读 token URL | ✅ |
| token → cookie 鉴权（HTTP/WS 共用） | ✅ |
| `session/list` / `session/create` | ✅ |
| `session/follow`：baseline + records + 实时 event + `assistant-stream` 增量 | ✅ |
| `session/prompt` + 流式渲染（text-delta 逐字上屏） | ✅ |
| 过滤 host 注入上下文、过滤 reasoning 块 | ✅ |
| IME 输入（gpui-component `InputState`）+ Enter 发送 | ✅ |
| 消息区自动滚到底 | ✅ |
| 「＋ 新会话」按钮 | ✅ |
| **审批弹窗**（waterfall → `$events/result`） | ✅ 端到端验证（sandbox 提权 → 允许 → 工具执行 → 文件落盘） |
| **停止按钮**（`session/cancel`，agent 运行中显示） | ✅ |
| **进程清理**（独立进程组 + SIGTERM 钩子，退出无残留 host） | ✅ |
| **终端块**：`terminal/create` + `attach`（snapshot/output/state 帧）→ **libghostty-vt** → 视口文本；write/resize 上行 | ✅（vt-1.0.0 官方源码包构建，`echo` 命令端到端验证） |
| **终端颜色**：SGR → GhosttyStyle(palette/RGB) → xterm256 展开 → GPUI 文本色；bold/inverse | ✅（bash 绿色加粗用户名、蓝色目录验证） |
| **终端键盘**：点击终端聚焦，按键→PTY（Enter/Backspace/方向键/Home/End/Delete/PageUp·Down/Ctrl+字母/可打印字符） | ✅ |
| **流式/完整回复**：`assistant/message` 事件（**不用 `assistantStream`**——它只推增量帧，流结束不固化） | ✅ |
| **图片消息**：事件里 image 块 → `session/attachment` 拉 base64 → 解码 BGRA → GPUI `img()`（240px 圆角） | ✅（内联图 prompt 端到端验证） |

**dsh 的单写者模型**（踩到的坑）：一个会话同时只能有一个写句柄。
浏览器里开着这个会话时，GUI 发 `session/prompt` 会得到
`session/writer-held`。所以 GUI 要能自己建会话（`session/create`），
或者先关掉浏览器里的那个会话。

**已知缺口**：

- 没有停止/取消按钮（`session/cancel`）
- 没有终端块（M2）
- 没有图片渲染（`session/attachment`）

### 构建依赖（Linux）

```sh
sudo apt install -y libfontconfig1-dev libfreetype-dev libwayland-dev \
  libxkbcommon-dev libxkbcommon-x11-dev libx11-dev libxcb1-dev \
  libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev
```

**inotify**（dsh host 的 HMR 要 watch profile 目录；默认 128 实例在桌面环境常不够）：

```sh
sudo sysctl fs.inotify.max_user_instances=512   # 临时
echo fs.inotify.max_user_instances=512 | sudo tee /etc/sysctl.d/99-dsh.conf  # 持久
```

### libghostty-vt（终端引擎）

vt-1.0.0 官方源码包构建（不用系统包）：

```sh
# Zig 0.17.0（官方 tarball，装到 /tmp 或任意目录）
curl -L https://ziglang.org/download/0.17.0/zig-x86_64-linux-0.17.0.tar.xz | tar xJ -C /tmp
export PATH=/tmp/zig-x86_64-linux-0.17.0:$PATH

# 构建静态/动态库（源码+deps tarball 校验和见 release.files.ghostty.org）
cd /tmp && curl -LO https://release.files.ghostty.org/libghostty-vt/1.0.0/libghostty-vt-1.0.0.tar.gz \
                  -O https://release.files.ghostty.org/libghostty-vt/1.0.0/libghostty-vt-1.0.0-deps.tar.gz
tar xzf libghostty-vt-1.0.0.tar.gz && tar xzf libghostty-vt-1.0.0-deps.tar.gz
cd libghostty-vt-1.0.0
DESTDIR=/tmp/gvt-pkg ZIG_GLOBAL_CACHE_DIR=/tmp/gvt-cache \
  zig build --prefix /usr --system ../libghostty-vt-1.0.0-deps -Doptimize=ReleaseFast -Dcpu=baseline
# 产物拷进 vendor/
```

运行时：`LD_LIBRARY_PATH=vendor/libghostty-vt/lib`（或 rpath）。绑定在 `crates/app/src/ghostty_vt.rs`（render-state 脏行路径）。

### 代码结构

```
crates/
├── host/       # 启动 dsh 子进程、解析 token URL、换 cookie
├── transport/  # Typert Remote：unary(POST /api/<ep>) + WS mux(open/item/end/error)
├── spike/      # M0 验证工具：--list-only / --events N / --follow N / --prompt "..."
└── app/        # GPUI 窗口：会话列表 + 消息流 + 输入框
```

## 文档索引

| 文件 | 内容 |
|---|---|
| [docs/01-dsh-architecture.md](docs/01-dsh-architecture.md) | dsh 分层、四个能力面、为什么只选 gateway |
| [docs/02-gateway-protocol.md](docs/02-gateway-protocol.md) | Typert RPC 传输/鉴权/帧/事件/严格校验；端点总表；能力→端点映射 |
| [docs/03-rust-gui-architecture.md](docs/03-rust-gui-architecture.md) | 目标架构、传输层、契约 codegen、进程管理、blocks 设计 |
| [docs/04-terminal-stack.md](docs/04-terminal-stack.md) | herdr 终端实现（libghostty-vt）+ v1 分期（alacritty_terminal → libghostty-vt） |
| [docs/05-images-and-tables.md](docs/05-images-and-tables.md) | 图片三层、tern 表格/TSP 机制、本项目取舍 |
| [docs/06-plugin-management.md](docs/06-plugin-management.md) | 插件安装/卸载/启停/市场/清单 + 供应链安全 |
| [docs/07-risks-and-roadmap.md](docs/07-risks-and-roadmap.md) | 风险对策、M0–M5 里程碑、工程量 |
| [docs/08-build-on-herdr.md](docs/08-build-on-herdr.md) | 为什么 v1 不嵌 herdr-core；三种形态的评审 |
| [docs/09-mobile.md](docs/09-mobile.md) | 手机版评估：架构顺、UI 壳是难点、路线排序 |
| [docs/10-startup-timings.md](docs/10-startup-timings.md) | 启动时间实测：首帧 ~110ms、独显唤醒坑、字体 fontconfig 收窄、首帧按帧补铺 |
| [contracts/endpoints.json](contracts/endpoints.json) | 29 个包 / 144 个端点，含参数与结果 JSON Schema（自动导出） |

## 研究方法（可复现）

```sh
# dsh 包本体（npx 缓存）
ls ~/.npm/_npx/1e7f6d9597241db0/node_modules/@deepseek-ai

# 网关端点契约（生成物，含 jsonSchema）
#   每个 dsh-api-*/lib/typert.remote-client.js 的 methods 表
# 导出脚本见 docs/02-gateway-protocol.md 附录

# herdr 终端栈证据
strings ~/.local/bin/herdr-core | grep -i ghostty     # libghostty-vt、kitty_gfx、ghostty_cell_get_multi
strings ~/.local/bin/herdr-app  | grep gpui           # GPUI 自绘
```

## 状态

研究阶段完成，尚未开始编码。下一步：M0 spike（spawn host + token/cookie +
`session/list` + `$events` 流式回显）。
