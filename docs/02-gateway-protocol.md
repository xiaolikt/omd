# 02 · Gateway 协议（Typert RPC）

> 数据来源：`~/.npm/_npx/<hash>/node_modules/@deepseek-ai/*/lib/typert.remote-client.js`
> 与 `dsh-api-gateway` / `dsh-typert-protocol` 的 README（实现见
> `packages/api/gateway`）。本文件记录「外部客户端需要实现的线协议」。

## 1. 传输

| 用途 | 通道 |
|---|---|
| 一元调用（unary） | `POST /api`，JSON，返回 `RemoteResult<T>` = `{ok:true,value}` 或 `{ok:false,error}` |
| 流（stream） | WebSocket **`/api/remote.mux`**（客户端插件激活时打开并保持，空闲也连） |
| 应用事件（全局） | 同一 WS 上的逻辑端点 **`$events`**（Host 注册的事件源；API Remotes 负责选择与校验） |
| 文件上传 | `POST`（`fileUploads/upload`，见 `dsh-client-file-upload`） |

### 帧

```
客户端 → Host： open / item / end / cancel
Host → 客户端： 逻辑流帧（按 stream id 复用同一 socket）
```

- 每个逻辑流带一个 **uplink**（客户端→Host 的 item 流），`end` 半关闭；
  超过 `streamInboxBytes`（默认 262144）→ `gateway/uplink-overflow`；
  在 `end` 之后发 item → `gateway/protocol`（不关 socket）。
- 心跳：Host 每 `websocketHeartbeatIntervalMs`（默认 2000ms）发 Ping；
  两个周期内无活动（Pong/收到帧/下发送出）→ 断开，由 Connection 的重试策略重连。
- 已结束流的 `item/end/cancel` 帧被丢弃；只有重复 `open` 会关 socket。

### 严格校验

- 参数按 Typert 编译的 descriptor 校验一次，不符即 `gateway/input-invalid`；
  未知参数名报错，**不静默忽略**（这是好事：错得大声）。
- 结果可能是 JSON，或带 `encode()/decode()` codec 的字节值（`Uint8Array`，参数/事件/流 item 仍是 JSON-only）。
- 业务错误统一为 `RemoteError`，码形如 `<domain>/<reason>`（如 `gateway/cancelled`、`remote-access/denied`）。

## 2. 鉴权

- `dsh web` 启动打印 `dsh web: http://127.0.0.1:<port>/?token=…`（进程级一次性 token）。
- 浏览器（或原生客户端）用该 URL 换取**签名 cookie**（HTTPS 时 `Secure`）；之后所有 API/WS 带 cookie。
- **Host/Origin 栅栏**：默认只接受本机 loopback；其他 authority 需 `--trusted-host`；
  `--public-url` 仅广告，不授予信任。
- 访问控制：每次 Remote 调用都有 Peer 身份；非 owner 调用走 `remoteAccess.decide(...)`
  （`dsh-api-remote-access`），拒绝为 `remote-access/denied`。会话资源按 `session`、
  工作区按 `workspace`、其余按 `global` 粒度。

## 3. 端点总表（144 个，29 个包）

完整参数/结果 JSON Schema 见 [`contracts/endpoints.json`](../contracts/endpoints.json)。

| 域 | 包 | 端点 |
|---|---|---|
| 会话 | `dsh-api-session-controller` (21) | `session/create` `session/list` `session/follow`(流) `session/prompt` `session/page` `session/projections` `session/control` `session/cancel` `session/fork` `session/rename` `session/search` `session/selectModel` `session/modelCatalog` `session/initializeDefaultModel` `session/attachment` `session/updateQueue` `session/workspacePathApplications` `session/canOpenWorkspacePath` `session/openWorkspacePath` `fileReferences/list` `skills/list` |
| 终端 | `dsh-api-terminal-controller` (8) | `terminal/create` `terminal/attach`(流) `terminal/list` `terminal/close` `terminal/retain` `terminal/rename` `terminal/environment` `terminal/shells` |
| 工作区 | `dsh-api-workspace-controller` (14) | `workspace/create` `workspace/delete` `workspace/rename` `workspace/follow` `workspace/pinSession` `workspace/unpinSession` `workspace/archiveSession` `workspace/unarchiveSession` `workspace/insertBefore` `workspace/insertSessionBefore` `workspace/initializeDefault` `directoryPicker/list` `directoryPicker/pick` `directoryPicker/createDirectory` |
| 文件 | `dsh-api-workspace-files` (5) | `workspaceFiles/list` `workspaceFiles/read` `workspaceFiles/readBytes` `workspaceFiles/stat` `workspaceFiles/changes` |
| 设置/凭据 | `dsh-api-settings-controller` (8) | `settings/describe` `settings/update` `settings/replace` `settings/mutate` `settings/openSettingsDocument` `credentials/describe` `credentials/set` `credentials/unset` |
| 任务 | `dsh-api-job-controller` (3) | `job/list` `job/follow` `job/kill` |
| 账号 | `dsh-api-account-controller` (11) | `account/getState` `account/getProfile` `account/getBalance` `account/startSignIn` `account/cancelSignIn` `account/signOut` `account/watch` `account/watchExpiry` `account/getUnnotifiedBonuses` `account/ackBonusNotified` `account/hasRunningAccountTasks` |
| 插件 | `dsh-plugin-manager` (12) | `pluginManager/listPlugins` `pluginManager/listBundles` `pluginManager/inspect` `pluginManager/installBundle` `pluginManager/removeBundle` `pluginManager/setBundleEnabled` `pluginManager/setPluginEnabled` `pluginManager/cancelInstall` `pluginManager/waitForInstall` `pluginManager/registries` `pluginManager/listVersionExemptions` `pluginManager/setVersionExemption` |
| 插件市场 | `dsh-plugin-market` (3) | `pluginMarket/listIndexes` `pluginMarket/search` `pluginMarket/setIndexes` |
| 插件清单 | `dsh-host-plugin-inventory` (1) | `pluginInventory/list` |
| 命令 | `dsh-commands` (2) | `commands/list` `commands/execute`（斜杠命令） |
| 目标/待办 | `dsh-goal` (7) | `goals/get` `goals/create` `goals/edit` `goals/complete` `goals/pause` `goals/resume` `goals/clear` |
| 日程 | `dsh-schedule` (5) | `schedule/list` `schedule/catalog` `schedule/update` `schedule/delete` `schedule/history` |
| 子代理 | `dsh-subagent` (2) | `subagents/prompt` `subagents/interruptByParent` |
| 提问 | `dsh-user-questions` (2) | `userQuestions/answer` `userQuestions/attachWait` |
| 权限预设 | `dsh-permission-presets` (1) | `permissionPresets/catalog` |
| 模型 | `dsh-llm` (3) | `llm/listProviders` `llm/listConfigurableProviders` `llm/discoverModels` |
| agent preset | `dsh-agent-preset-registry` (3) | `agentPresets/list` `agentPresets/read` `agentPresets/select` |
| 反馈 | `dsh-message-feedback` (3)、`dsh-command-feedback` (1) | `messageFeedback/list|put|delete`、`sessionFeedback/record` |
| 上传 | `dsh-client-file-upload` (1) | `fileUploads/upload` |
| 语音（实验） | `dsh-experimental-api-speech-to-text` (6) | `speech/catalog|configure|prepare|follow|transcribe|cancelPreparation` |
| Office | `dsh-office-to-pdf` (2) | `officeToPdf/render|generation` |
| 引用/翻译/分析 | `dsh-session-reference` (1)、`dsh-experimental-client-ui-cot-translation` (2)、`dsh-client-product-analytics` (3) | `sessionReferenceResolver/candidates`、`cotTranslation/translate|limits`、`productAnalytics/*` |
| 插件运行时 | `dsh-cordis-host-runner` (12) | `dynamicCordisRunner/invoke|inventory|runHostHalf|…` |
| 探测 | `dsh-client-ui-plugin-manager` (1) | `pluginRegistryProbe/fastest` |
| 远程访问 | `dsh-api-remote-access` (1) | `remoteAccess/follow` |

## 4. 事件（Host → Client）

通过 `$events` 逻辑流推送，客户端先收到 `{type:'ready', clientId, host:{home}}`。
已知事件（来自转发的 Host 事件源）：

| 事件 | 用途 |
|---|---|
| `approval/request` | 审批：需要用户允许/拒绝某个动作（答案经 `session/control` 回传） |
| `user-questions/request` | agent 提问问卷（答案经 `userQuestions/answer`） |
| 会话活动/状态 | 配合 `session/follow` 与 `session/projections` 驱动界面 |

> 事件的资源级鉴权：非 owner 客户端只有 `remoteAccess.decide` 允许时才收到帧。

## 5. 关键端点语义（实现必读）

- **`session/follow`（流）**：订阅一个会话的完整日志事件流（就等于 web UI 的对话流）。
  配合 `session/page`（分页读取历史）与 `session/projections`（派生投影：统计、大纲等）。
- **`session/prompt`**：投递用户消息（内容块 + 图片附件）。
- **`session/control`**：控制操作（含审批应答、取消等）。
- **`terminal/attach`（流）**：上下行帧
  ```json
  下行：屏幕/输出数据（字节）
  上行：[{"type":"write","sequence":N,"data":"…"},
         {"type":"resize","sequence":N,"cols":80,"rows":24}]
  ```
  必须实现 VT 仿真（见 04）。
- **`pluginManager/installBundle`**：装插件 bundle，配套 `cancelInstall`、`waitForInstall`。

## 附录：契约导出脚本

```js
// dump-endpoints.mjs —— 从生成的 typert.remote-client.js 导出全部端点与 JSON Schema
import fs from 'node:fs';
import path from 'node:path';
const base = path.join(process.env.HOME, '.npm/_npx/<hash>/node_modules/@deepseek-ai');
const pkgs = fs.readdirSync(base).filter((p) =>
  fs.existsSync(path.join(base, p, 'lib', 'typert.remote-client.js')));
const out = {};
for (const pkg of pkgs) {
  const mod = await import(path.join(base, pkg, 'lib', 'typert.remote-client.js'));
  const info = mod.TYPERT_REMOTE ?? mod.default?.TYPERT_REMOTE ?? mod.default;
  out[pkg] = Object.fromEntries(Object.entries(info.methods ?? {}).map(([ep, d]) => [ep, {
    service: d.service, description: d.description, invocation: d.invocation,
    mode: d.mode ?? 'unary',
    parameters: (d.parameters ?? []).map((p) => ({
      name: p.name, source: p.source, description: p.description,
      jsonSchema: p.codec?.jsonSchema })),
    result: d.result && { description: d.result.description, jsonSchema: d.result.jsonSchema },
  }]));
}
fs.writeFileSync('contracts/endpoints.json', JSON.stringify(out, null, 2));
```

> 该脚本已跑过：29 个包、144 个端点全部导出成功（见 `contracts/endpoints.json`）。
> 升级 dsh 后重跑即可 diff 出破坏性变更。

## 6. 需要人类回答的事件（审批 / 提问）

这一类事件**不是** `emit`，而是 `waterfall` 帧；只有声明了
`interactions=answer` 的 peer 才会收到。

### 鉴权差异（关键）

| 入口 | 得到的 peer | 能收 waterfall |
|---|---|---|
| `GET /?token=…` | 浏览器 peer（`interactions: none`） | ❌ |
| `GET /connect?token=…&interactions=answer` | API peer，302 → `./api/remote.mux` | ✅ |

`/connect` 会返回 **302**（Location 是 WebSocket 端点）；HTTP 客户端**不能跟随重定向**，
否则会挂在 WS 升级上。cookie 是 `version: 2` 的签名值。

### 帧

```jsonc
// $events 流上的 waterfall 帧
{
  "type": "waterfall",
  "event": "approval/request",
  "eventId": "<uuid>",
  "agentId": "session-…",
  "request": {
    "toolName": "write",
    "callId": "call_…",
    "reason": "escalate sandbox to danger-full-access: …"
  }
}
```

### 应答

```
POST /api/$events/result
{ "args": { "clientId": "<$events ready 帧给的 id>", "eventId": "<uuid>",
            "outcome": { "kind": "result", "value": "allowed-once" } } }
```

- `outcome.kind`：`result`（回答）、`next`（交给下一个 listener）、`rejected`
- `ApprovalOutcome = 'allowed-once' | 'rejected' | 'cancelled' | 'unavailable'`
  （`allowed-once` 是唯一的授权）
- 诊断：拿错 eventId 会得到 `typert gateway: Remote event result identifies no active event stream`；
  如果 peer 不是 answer，会先得到 `gateway/interactions-forbidden`

### 审计事件（会话日志里）

`approval/asked`（id/toolName/callId/reason）→ `approval/decided`（id/outcome）。
它们是**日志**，不是应答通道；应答必须走上面的 `$events/result`。
