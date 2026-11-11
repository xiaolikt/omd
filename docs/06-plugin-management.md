# 06 · 插件管理（GUI 内完成安装）

## 结论

插件安装/卸载/启停/市场发现**全部在网关 RPC 里**，GUI 不需要设计「shell 里敲 pnpm」的体验；
只有高级 pnpm 操作（`why`、自定义 registry 参数等）才回退到子进程。

## 三层接口

### 1. 运行期状态（只读）

`pluginInventory/list`（`dsh-host-plugin-inventory`）
→ 当前 Loader 的插件快照：每行的 entry id、模块说明符、生效开关（含被祖先 group 禁用的情况）、
  根 Fiber 阶段（`pending|loading|active|failed|unloading|null`）、可展示文本；
  以及每个 agent preset 的元数据、健康度和扁平化组成。

用途：插件页的「已安装 / 已加载 / 失败原因 / 预设组成」。

### 2. 安装与启停（写）

`pluginManager/*`（`dsh-plugin-manager`，12 个方法）：

| 方法 | 用途 |
|---|---|
| `pluginManager/listPlugins` | profile 里的插件 |
| `pluginManager/listBundles` | 已安装 bundle |
| `pluginManager/inspect` | 安装前检查（来源/版本/依赖） |
| `pluginManager/installBundle` | 安装 bundle（走 profile 的包管理器调用） |
| `pluginManager/cancelInstall` / `waitForInstall` | 取消 / 等待安装完成 |
| `pluginManager/removeBundle` | 卸载 |
| `pluginManager/setBundleEnabled` / `setPluginEnabled` | 启/停（写回 profile patch） |
| `pluginManager/registries` | registry 列表 |
| `pluginManager/listVersionExemptions` / `setVersionExemption` | 版本豁免（兼容性例外） |

### 3. 市场（发现）

`pluginMarket/listIndexes` / `pluginMarket/search` / `pluginMarket/setIndexes`
→ 读取用户配置的市场索引（JSON 格式，`dsh-plugin-market-index` 校验），
  返回条目 + 安装状态 + DSH 兼容性 + **精确安装参数**。
官方 web 的 Discover 页和 `plugin_market` agent 工具都读它，安装最终仍走 `pluginManager/installBundle`。

### 4. 兜底：CLI 转发

`dsh plugin --profile <name> <pnpm 参数…>` —— 本质是把参数转发给
`$DSH_HOME/profiles/<name>/` 里的 pnpm。GUI 里做一个「终端面板」显示子进程输出，
仅在用户需要 `why`/`add -D`/自定义参数时使用。

## 插件系统的底层事实

- profile 目录：`$DSH_HOME/profiles/<name>/`（`cordis.patch.yml` + `package.json` + `node_modules`）。
- 配置树可自省：`dsh --dump-config`、`--dump-config-schema`、`--dump-default-config`。
- 首次使用 `dsh plugin --profile x` 会自动初始化 profile（首次需要 pnpm）。
- pnpm ≥ 11 默认阻止 install 脚本（`ERR_PNPM_IGNORED_BUILDS`）——这正好是我们要的安全默认。

## 供应链安全（GUI 侧必须做的）

| 风险 | 对策 |
|---|---|
| 任意 npm 包 = 远程代码执行 | 安装前 `inspect` 展示：包名、版本、来源 registry、依赖树、是否含 install 脚本 |
| 恶意 registry | registry 白名单（`pluginManager/registries` + 本地策略），非白名单需二次确认 |
| 未知版本 | 展示版本豁免状态（`listVersionExemptions`），默认不放宽 |
| 安装中无法中断 | 用 `cancelInstall`；进度用 `waitForInstall` 轮询 |
| 装完起不来 | 安装后读 `pluginInventory/list` 的 `failed` 阶段与错误文本，给出回滚入口（`removeBundle`） |
| 供应链投毒（包名近似） | 市场条目只从用户配置的索引来；手输包名时显示 registry+发布者 |

## GUI 交互设计（建议）

1. **已安装页**：`pluginInventory/list` 树；按 preset 分组；`failed` 行直接显示原因 + 卸载按钮。
2. **发现页**：`pluginMarket/search` 列表，卡片显示兼容性/安装状态/安装参数。
3. **安装向导**：确认页（来源、版本、依赖、脚本策略）→ 进度（`waitForInstall`）→ 结果（成功/失败原因）。
4. **高级页**：内嵌终端面板跑 `dsh plugin --profile <name> <args>`，输出实时显示。
5. **配置树**：只读展示 `--dump-config`，帮助排查 patch 叠加问题。

## 待确认（升级 dsh 时需要复核）

- `pluginManager/installBundle` 的安装参数形态（bundle id vs npm spec）——以 `contracts/endpoints.json`
  里的 JSON Schema 为准。
- 安装是否支持「重启后生效」语义（当前 patch 层在启动时应用），GUI 需提示用户。
