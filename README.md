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
| **插件管理**：`pluginManager/listPlugins` + `setPluginEnabled` + `listBundles` + `installBundle`/`removeBundle` | ✅（安装带 requestId、可取消、构建脚本批准、失败原因回显） |
| **插件市场**：`pluginMarket/listIndexes` + `search`（`nextOffset` 分页） | ✅（搜索、安装/更新、状态徽章、load more） |
| **消息反馈**：`messageFeedback/list` + `put`/`delete`，assistant 块下的 👍/👎（再点撤回） | ✅ |
| **会话管理**：tab 右键 → Fork（`session/fork`）/ 置顶（`workspace/pinSession·unpinSession`）/ 归档（`workspace/archiveSession`） | ✅ |
| **Slash 命令**：`commands/list` 进命令面板；输入框以已知 `/命令` 开头发送时走 `commands/execute` | ✅ |
| **附件**：📎 选择文件 / 拖进窗口 / Ctrl+V 粘贴图片；图片内联，其它文件走 `fileUploads/upload` 换 receipt | ✅（上传 + prompt content 实测通过） |
| **目标（goal）**：`goals/get` 横幅 + create/edit/pause/resume/complete/clear | ✅（回填、CAS revision 实测） |
| **日程**：⏱ 面板（`schedule/catalog` 按会话过滤）+ 投递历史 + 删除 | ✅（catalog/list 实测；创建由 agent 的 schedule 工具完成） |
| **workpace 状态**：`workspace/follow` baseline + `pinned`/`archived` 增量；置顶/归档跨重启 | ✅（baseline 实测：2 archived） |
| **子代理**：⇄ 面板列出 `parentSessionId` 子会话，可打断（`subagents/interruptByParent`）、追问（`subagents/prompt`） | ✅（参数实测） |
| **终端搜索/回滚**：Ctrl+F 搜 scrollback（Enter 下一个 / Shift+Enter 上一个 / k·n 计数），滚轮与 Shift+PageUp·Down 滚动视口 | ✅（单测：30 行回滚定位 + 视口滚动） |
| **长会话分页**：follow snapshot 的 `hasMore` → 「load older messages」按 `session/page`（throughSeq/beforeSeq）补页 | ✅（分页参数实测：seq 0–24 按序返回） |
| **终端粘贴**：Ctrl+Shift+V——剪贴板有图走 kitty 图形协议本地显示（不进 PTY），文本按 bracketed paste 编码（`ghostty_paste_encode`） | ✅（单测：分块传输 + 200~ 包裹） |
| **权限预设**：composer 的 🛡 pill 显示/切换 read-only·workspace-write·danger-full-access（`/permission` 命令）；审批卡提供「always: <preset>」一键放行+切换 | ✅（`permission/preset`+`sandbox/mode` 事件实测） |
| **终端选区**：鼠标拖选（高亮），松开自动复制；Ctrl+Shift+C 复制；单击清空。取文本走 `ghostty_formatter`（plain + unwrap + trim） | ✅（单测：行内/跨行提取，两端包含语义） |
| **Fork 到指定消息**：转写里带 seq 的消息下方「⎇ fork here」（`session/fork` 的 `atSeq`）；tab 右键仍是最近完成 turn | ✅（atSeq=5 实测返回新会话） |
| **子代理树**：⇄ 面板按 `parentSessionId` DFS 到第 4 层，孙代缩进 + L 层标 | ✅ |
| **goal/schedule 编辑表单**：goal 目标 + 轮次上限（`maxGoalRounds`）；日程 title/prompt 行内编辑（`schedule/update` 带 `expected`） | ✅（create 7 → edit 9 → clear 实测） |
| **编辑器格式化**：❏ format 按钮 / Ctrl+Shift+F → `textDocument/formatting`（缩进按文件嗅探），结果经事件回应用到 open buffer | ✅（rust-analyzer 实测：3 edits → 正确格式化） |
| **编辑器大纲**：☰ outline 按钮 → `textDocument/documentSymbol`（层级扁平化），浮层列表点击跳转（`set_cursor_position`），Esc 关闭 | ✅（rust-analyzer 实测：Foo/x/main + 行号） |

**dsh 的单写者模型**（踩到的坑）：一个会话同时只能有一个写句柄。
浏览器里开着这个会话时，GUI 发 `session/prompt` 会得到
`session/writer-held`。所以 GUI 要能自己建会话（`session/create`），
或者先关掉浏览器里的那个会话。

**已知缺口**：

- 计划/目标、日程、子代理面板（`goals/*`、`schedule/*`、`subagents/*`）尚未做 UI

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

### 内置浏览器（Linux / Wayland）

CEF 二进制位于 `vendor/cef/<version>/cef_linux_x86_64/`，不随源码分发。

```sh
CEF_PATH="$PWD/vendor/cef" cargo build --release -p omd
# 在 Wayland 桌面的终端运行；WAYLAND_DISPLAY 和 XDG_RUNTIME_DIR 由会话提供。
OMD_BROWSER=1 LD_LIBRARY_PATH="$PWD/vendor/libghostty-vt/lib" ./target/release/omd
```

release 冒烟（2026-11-20）：无浏览器路径首帧 8.2ms、engine ready 无 panic；
`OMD_BROWSER=1` 下 CEF init ok、浏览器 pane 创建成功（浏览器块的鼠标监听已移到
paint 阶段注册，debug/release 均不再触发 paint-only 断言）。

- GPUI 与 CEF 在初始化前选择会话；有 `WAYLAND_DISPLAY` 时使用原生 Wayland，不需要 `DISPLAY`。
- Wayland 使用 CEF OSR 的 BGRA 像素回读，再上传到复用的 GPUI 纹理；这不是零拷贝。
  当前 CEF 156 在 Wayland 下共享纹理捕获分配失败，因此不启用 DMA-BUF 导出，也不关闭 Chromium GPU 渲染。
- X11 可设置 `OMD_FORCE_X11=1 OMD_BROWSER_ACCEL=1` 使用 DMA-BUF；
  `OMD_BROWSER_CHILD=1` 则选择原生 X11 子窗口（需要同时设置 `OMD_BROWSER=1`）。
- `OMD_BROWSER_URL` 指定初始页面，默认 `https://example.com`；CDP 默认端口为本机 `9222`，可用 `OMD_CDP_PORT` 修改。
- 已在 GNOME/Mutter 原生 Wayland（移除 `DISPLAY`、隔离合成器禁用 XWayland）验证纹理显示、
  原生鼠标聚焦、字母输入、Backspace 编辑及 CDP；页面收到的输入事件为可信事件。
  中文 IME、剪贴板、弹出菜单与跨显示器缩放尚未验证。
- 当前 CEF 配置关闭了 Chromium 沙箱；浏览器块尚不具备系统浏览器同等的安全隔离，不用于不可信站点。

### 代码结构

```
crates/
├── host/       # 启动 dsh 子进程、解析 token URL、换 cookie
├── transport/  # Typert Remote：unary(POST /api/<ep>) + WS mux(open/item/end/error)
├── spike/      # M0 验证工具：--list-only / --events N / --follow N / --prompt "..."
└── app/        # GPUI 窗口：会话列表 + 消息流 + 输入框
```

## 编辑器（文件面板）

- LSP：补全（含 `additionalTextEdits` 自动 import）、hover、定义跳转、code actions、
  语义高亮、诊断装饰（`crates/app/src/lsp.rs`；`OMD_LSP_<EXT>` 可覆盖 server 命令）。
- 格式化：编辑器 pane 头部「❏ format」或 Ctrl+Shift+F；缩进从文件前导空白嗅探，
  结果通过 `Event::Formatted` 回来后在 render 阶段应用到 open buffer（标记 dirty）。
- 大纲：pane 头部「☰ outline」拉取 documentSymbol（层级 DFS 扁平化，深度缩进），
  右上浮层列出 kind/名称/行号，点击跳到定义处，Esc 关闭。

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

M0–M2 完成，M3 进行中（文件/编辑器、插件管理与市场、消息反馈、会话 fork/置顶/归档、
slash 命令面板、附件上传、目标横幅、日程面板、workspace 状态流、子代理面板、终端
搜索/回滚、长会话分页、终端粘贴已接）。

### 插件页与 tab 菜单

- 设置 → Plugins 现在分三段：内置插件（启停）、Bundles（官方 on-demand 安装 / 卸载）、
  Market（搜索 + 安装/更新；安装带 requestId，可取消，构建脚本需显式批准）。
  `pluginMarket/*` 在未安装市场 bundle 的 profile 里返回 404，UI 会提示未启用而不是报错。
- tab 上右键：Rename / Fork session / Pin·Unpin / Archive（归档会关闭对应 tab 并刷新列表）。
  转写里带事件 seq 的消息下方有「⎇ fork here」，从那条消息处切开（`atSeq`）。
- assistant 消息下方有 👍/👎（再点撤回），走 `messageFeedback/*`。
- `Ctrl+K` 命令面板里会列出 `commands/list` 的 slash 命令；输入框里以已知
  `/命令` 开头发送时直接走 `commands/execute`，不再当普通 prompt 发给模型。
- 附件：输入框左下 📎 打开系统文件框（zenity / kdialog），也可以把文件拖进窗口、
  或用 Ctrl+V 粘贴剪贴板里的图片。图片以 base64 内联；其它文件先 `fileUploads/upload`
  换 receipt，再以 `{type:"file",receiptId}` 进 prompt。上传未完成的附件会阻止发送。
- 目标：消息区顶部一条 goal 横幅（目标、阶段、已用轮次/上限、阻塞原因），可直接
  暂停/继续/完成/清除/改目标（含轮次上限）；没有 goal 时显示「◎ goal」入口。变更事件（`goal/change`）
  会自动重新拉取，所有写操作都带 revision 做 CAS。
- 日程：标题栏 ⏱ 打开当前会话的提醒列表（`schedule/catalog` 全量按 sessionId 过滤），
  可看投递历史、行内编辑 title/prompt、删除；`schedule/changed` 时自动刷新。
  创建提醒由 agent 的 `schedule_create` 工具完成（协议没有给客户端的创建端点）。
- 子代理：标题栏 ⇄ 列出当前会话派生的后代树（`session/list` 的 `parentSessionId`，
  DFS 到第 4 层，孙代缩进 + L 层标），选中后可发消息（queue）/ 打断。
- 终端：Ctrl+F 打开回滚搜索条（ghostty 的 search API，覆盖主/副屏与 scrollback；
  Enter 向旧、Shift+Enter 向新、显示 k/n，Esc 关闭并回底部）；滚轮和
  Shift+PageUp/PageDown 滚动视口；Ctrl+Shift+V 粘贴——图片用 kitty 图形协议
  （RGBA 分块 APC，本地显示，字节不进 PTY），文本按 mode 2004 编码（bracketed
  前后缀 / 换行转 CR）；鼠标拖选文本（pane 边界由 canvas 记录，选区变化整屏
  重绘），松开即复制，Ctrl+Shift+C 也可。
- 置顶/归档来自 `workspace/follow`（启动时 baseline + `pinned`/`archived` 增量），
  重启后仍然生效；归档的会话从侧栏过滤掉。
