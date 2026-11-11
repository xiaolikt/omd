# 01 · dsh 架构与能力面

## dsh 是什么

- 包名：`@deepseek-ai/dsh`，仓库 `github.com/deepseek-ai/deepseek-harness`，**MIT**。
- 版本：实测 `0.2.1-rc.5`。
- 实现：TypeScript + Node.js，**cordis** 依赖注入 + 插件 bundle 架构，monorepo 307 个包（`@deepseek-ai/dsh-*`）。
- 运行模型：`dsh [--profile] <name>` 启动一个「profile」= 按顺序叠 patch 层的插件栈。
  每个 profile 是 `$DSH_HOME/profiles/<name>/` 下的一份 `cordis.patch.yml` + `package.json`。

## 分层

```
┌─ 引擎层（Node，必须存在）───────────────────────────┐
│ agent loop · 工具 · 会话日志(JSONL) · 策略 · 模型路由 │
└───────────────┬─────────────────────────────────────┘
┌─ 能力面层（4 条出口，决定「完整度」）───────────────┐
│ ① in-process public exports（TS）                   │  dsh-TUI 走这条
│ ② Typert gateway（HTTP + WS，144 端点）             │  web UI 走这条 ← 我们选
│ ③ ACP（标准协议，automation-only）                  │  Zed / 通用 ACP 客户端
│ ④ SDK JSON-RPC stdio（事件流，无审批/终端/插件）    │  脚本/测试
└───────────────┬─────────────────────────────────────┘
┌─ 渲染层（谁画都行）─────────────────────────────────┐
│ 浏览器 / 终端文本 / 原生 GPU                        │  ← 我们替换这一层
└─────────────────────────────────────────────────────┘
```

## 四个能力面对比

| | ① in-process | ② Typert gateway | ③ ACP | ④ SDK stdio |
|---|---|---|---|---|
| 会话/消息流 | ✅ | ✅ `session/follow` | ✅ 标准 `session/update` | ✅ `session.event` |
| 审批（approval） | ✅ | ✅ 事件 `approval/request` | ✅ `session/request_permission` | ❌ |
| 提问问卷（user questions） | ✅ | ✅ `user-questions/request` → `userQuestions/answer` | ❌ | ❌ |
| 实时终端（PTY） | ✅ | ✅ `terminal/attach`（字节 + write/resize） | ⚠️ 仅工具输出流 | ❌ |
| 工作区/文件面板 | ✅ | ✅ `workspace/*` `workspaceFiles/*` | ❌ | ❌ |
| 插件安装/市场 | ✅ | ✅ `pluginManager/*` `pluginMarket/*` | ❌ | ❌ |
| 设置/凭据 | ✅ | ✅ `settings/*` `credentials/*` | ❌ | ❌ |
| 模型切换 | ✅ | ✅ `session/modelCatalog` `session/selectModel` | ✅ 会话级 | ❌（initialize 固定） |
| 计划/目标/待办/日程 | ✅ | ✅ `goals/*` `schedule/*` + 事件卡片 | ❌ | 部分（事件） |
| 子代理 | ✅ | ✅ `subagents/*` + 事件 | ⚠️ 独立会话 | ✅ `subagent.*` 通知 |
| 图像输入 | ✅ | ✅ 内容块 + `fileUploads/upload` | ✅（image 能力已声明） | ✅ base64 内容块 |
| 稳定性承诺 | 无（内部导出） | 无（内部 RPC，但描述符机器可读） | **有**（ACP v1 标准） | **有**（README 声明 wire-stable） |
| UI 层技术约束 | 必须 TS/Node | 任意（HTTP/WS） | 任意（JSON-RPC stdio） | 任意（JSON-RPC stdio） |

**结论：要「全功能 + 原生 UI」，只有 ②。** ACP 明确是 automation-only
（README 原文：*intentionally omits DSH-specific presentation data and interactive UI features*）；
SDK 协议面只有 6 个请求 + 4 个通知，没有审批/终端/插件。

## profile 与插件体系（决定 GUI 的插件页）

- 启动：`dsh --profile <name>`，例：`dsh web`、`dsh tui`、`dsh headless "task"`。
- 插件安装：`dsh plugin --profile <name> <pnpm 参数…>`（把参数原样转发给 profile 目录里的 pnpm：
  `add <pkg>` / `remove <pkg>` / `why <pkg>`）。
- 配置树自省：`dsh --dump-config`、`--dump-config-schema`、`--dump-default-config`、
  `--patch <path>`（叠加 patch 层）、`--from-default-profile <name>`（重置）。
- 运行期插件状态：网关 `pluginInventory/list`（只读快照，含每个 agent preset 的组成）。
- 运行期安装/启停：网关 `pluginManager/*`（installBundle / removeBundle / setBundleEnabled /
  setPluginEnabled / cancelInstall / waitForInstall / registries …）。
- 市场：网关 `pluginMarket/listIndexes|search|setIndexes`（Discover 页的数据源）。

## 可借鉴的第三方先例

| 项目 | 说明 |
|---|---|
| **dsh-TUI**（`@deepseek-harness-tui/dsh-tui`，MIT，社区，官方点名推荐） | 挂在 dsh 进程内的 Cordis 插件，终端文本渲染；证明「浏览器之外可达到全量能力」；支持 OSC 7501 状态协议，可在 herdr 里直接跑 |
| **@dsh-std/***（同作者） | 它自建的客户端抽象层（connection/presentation/approval/notification），非官方协议，不建议作为 Rust 侧契约 |
| **Zed（ACP）** | 证明「引擎 Node、UI 原生」的通用模式（Claude Code/Codex/Gemini 都这么接） |

## 相关但不同

- `dsh-web-frontend` / `dsh-client-ui-*`：浏览器端组件，是**消费者**，不是能力。
- `dsh-cordis-client-runner`：在 host 内运行「client 插件」的 runner（web 前端插件也走它），
  提供 `dynamicCordisRunner/*` 端点（invoke/inventory/runHostHalf…），属于插件运行时，不暴露 UI。
