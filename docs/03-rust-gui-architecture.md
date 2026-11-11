# 03 · Rust GUI 架构

## 目标

- 原生 GPU 渲染桌面 App（Rust），无 Node/浏览器 UI。
- 功能覆盖 = 02 文档的 144 个端点 + `$events`。
- 引擎（dsh）以子进程/守护进程运行，App 是纯客户端。

## 技术选型

| 层 | 选型 | 理由 |
|---|---|---|
| UI 框架 | **GPUI + gpui-kit**（`gpui-kit = "0.9"`，Apache-2.0） | herdr 同栈；自带 75+ 组件：dock 布局、Markdown、diff、DataTable（虚拟滚动）、tree-sitter 代码编辑器、图表；120fps |
| 异步/网络 | tokio + reqwest + tokio-tungstenite | HTTP unary + WS 流 |
| 终端仿真 | **v1：`alacritty_terminal`**（能跑 vim）；终局：`libghostty-vt`（Kitty 图形） | 见 04；herdr 用的是后者 |
| 终端字节源 | dsh `terminal/attach`（引擎持有 PTY） | 不另管一套进程，也不嵌 herdr-core |
| 序列化 | serde + serde_json（必要处 msgpack） | 契约 codegen 目标 |
| 本地缓存 | SQLite（rusqlite） | 只存 UI 状态/会话索引；真实数据源永远是 host |
| 日志 | tracing | 定位协议层问题 |

## 进程模型

```
OMD (Rust)
 ├─ spawn: dsh --profile web --no-open --port 0 --printUrl true
 │    stdout 捕获 "dsh web: http://127.0.0.1:<port>/?token=…"
 ├─ 用 token URL 换 cookie（GET → Set-Cookie）
 ├─ 后续：POST /api（unary）+ WS /api/remote.mux（流 + $events）
 ├─ 不接 herdr-core：终端只渲染 dsh 自己的 `terminal/attach` 字节流
 └─ 生命周期策略二选一：
     a) 随 App 退出（简单）
     b) 不杀 host 子进程（会话在 `~/.dsh`，重开即连回）
```

> 为什么 v1 不嵌 herdr-core：它的公开 socket API 只有 ANSI 快照，不是 PTY 字节流，
> 画不出真终端。详见 [08-build-on-herdr.md](08-build-on-herdr.md)。

## 契约层（关键工程点）

1. 从 `contracts/endpoints.json` 生成 Rust 类型：
   - 每个端点 → 一个 `fn` + 请求/响应结构体（serde）；
   - `jsonSchema` → 手工/代码生成（`schemars` 反推或 typify 之类）均可；
   - 流端点（`mode: "stream"`）→ 单独的 channel 类型 + uplink 枚举。
2. 协议适配层（薄）：
   - `unary(endpoint, args) -> Result<T, RemoteError>`
   - `stream(endpoint, args) -> (Receiver<Item>, Sender<Uplink>)`
   - `events() -> Receiver<Event>`（`$events`，含 `ready`）
3. 版本兼容：
   - 每次升级 dsh，重跑导出脚本 → diff `endpoints.json`；
   - 破坏性变更（端点删除/参数改名/类型变化）在 CI 里红灯；
   - 运行时把 `serverInfo`/版本写进日志。

## UI 结构（tern 式 blocks）

```
┌ 侧栏 ──────────┬ 工作区 ─────────────────────────────┐
│ 工作区/会话列表 │ [agent block]  [terminal block]      │
│ 插件/设置入口   │ [files/changes] [diff]               │
└────────────────┴──────────────────────────────────────┘
```

| Block | 数据源 | 渲染 |
|---|---|---|
| Agent（对话） | `session/follow` 事件流 + `session/page` | Markdown、工具卡片、reasoning、图片、diff 卡片 |
| 审批 | `approval/request` 事件 | 原生模态（允许/拒绝/始终允许）→ `session/control` |
| 提问问卷 | `user-questions/request` | 原生问卷组件 → `userQuestions/answer` |
| 终端 | `terminal/create` + `terminal/attach` | libghostty-vt + GPUI 自绘（见 04） |
| 文件 / 变更 | `workspaceFiles/list|read|stat|changes` | 树 + diff 视图 |
| 会话侧栏 | `session/list`、`session/fork|rename|search`、`workspace/follow` | 列表 + 搜索 + 归档/置顶 |
| 模型 / effort | `session/modelCatalog`、`session/selectModel`、`llm/*` | 下拉 + 预设 |
| 计划 / 目标 / 待办 | `goals/*` + 事件卡片 | 面板 + 卡片 |
| 子代理 | `subagents/*` + 事件 | 树/时间线 |
| 任务（jobs） | `job/list|follow|kill` | 列表 + 进度 |
| 用量 / context | `session/projections` | 计量条/明细 |
| 设置 / 凭据 | `settings/*`、`credentials/*` | 表单（动态 schema 驱动） |
| 插件 | `pluginManager/*`、`pluginMarket/*`、`pluginInventory/list` | 见 06 |
| 账号 | `account/*` | 系统浏览器 OAuth + 回环回调 |
| 附件 | `fileUploads/upload`、`session/attachment` | 拖拽/粘贴 |

## 输入与快捷键

- 全局：命令面板（复用 `commands/list`）、会话切换、分屏。
- 终端块：Kitty 键盘协议协商（libghostty-vt 负责编解码）。
- 审批：键盘可达（Enter/Esc）。

## 安全

- 引擎只绑 loopback；禁用 `--public-url`；token 只在内存。
- 插件安装：见 06 的供应链对策。
- Agent 的工具执行发生在引擎侧（本地进程），App 不额外扩大权限。

## 目录建议

```
omd/
├─ crates/
│  ├─ proto/      # 契约类型（codegen 产物）
│  ├─ transport/  # unary/stream/events 适配层
│  ├─ host/       # 引擎进程管理、token/cookie、健康检查
│  ├─ term/       # libghostty-vt 绑定 + cell 渲染
│  └─ app/        # GPUI 界面、blocks
└─ contracts/endpoints.json
```
