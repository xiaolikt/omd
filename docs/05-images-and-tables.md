# 05 · 图片与表格

## 一、图片：三层机制

| 层 | 机制 | 实现要点 |
|---|---|---|
| 终端 block 内的图片（yazi 预览、`catimg`、kitty 协议程序） | **Kitty 图形协议 / Sixel** | libghostty-vt 自带解析与状态（`kitty_gfx`、图片逐出、动画帧、Unicode placeholder）；App 负责把图片对象合成到 cell 层 |
| App 内容里的图片（agent 消息、附件、头像） | **原生图片解码 + GPUI 渲染** | dsh 侧图片 = `session/attachment`（按 ID 取字节）+ 消息内容块 base64 + `fileUploads/upload`；GPUI 原生支持 PNG/JPEG/WebP/AVIF/GIF/SVG（herdr 的 `image()` 即此） |
| 富 UI 节点里的图片（如果将来做 TSP 兼容） | Surface Protocol 的 **blob** + `attachment` 节点 | 二进制资源单独上传，节点里引用；见下节 |

结论：**图片能力分两条独立管线**——终端管线（Kitty）和 App 管线（原生解码），互不干扰。

## 二、tern 的表格是怎么实现的

### 不是终端能力，是 Tern Surface Protocol（TSP）

终端里不可能用 ANSI 画出一个「可排序、可选择、列宽自适应」的表格。
Tern 的做法：**程序不再往 PTY 里画字符，而是往 PTY 里发一棵语义节点树**，
Tern 自己把它渲染成原生控件。

```
程序 ──APC 转义序列 + JSON 分块 + credits 流控──▶ Tern
   { k: "table",
     p: { cols: [{ id, head, align, priority, grow, truncate, mono, sortable }],
          rows: [{ id, cells: {…}, disabled, menu, title }],
          selectable: true, selected: "r3", sort: { col, dir } },
     c: [] }
◀──────────────── 事件回传：activate / select / change / sort / action
```

关键设计：

- **节点种类**：`text`（span 列表：文本/样式/链接）、`card`、`table`、`badge`、`chart`
  （柱/折线/热力/环形）、`meter`、`tabs`、`picker`、`overlay`、`editor`、`calendar`、
  `carousel`、`pager`、`kv`、`stats`、`progress`、`attachment`…
- **表格细节**（证明它是原生控件）：
  - 列 `priority`：窄窗时低优先列先隐藏；`grow`：一列吃剩余宽度；
    `truncate: end|start|middle`；`mono`：数字用等宽 + tabular figures。
  - 行按 `id` 做 **patch**（`reconcile` 按节点 id diff，只重绘变化行）。
  - 单元格类型：`TextCell`（可带状态点/图标）、`BadgeCell`、`ButtonCell`、
    `SparkCell`、`MeterCell`。
  - `selectable` / `selected` / `multi` / `picked` / `bar`（选择条批量动作）；
    表头排序回传 `sort`；行右键菜单回传 `action`（`item` 是行）。
- **传输**：APC 转义序列（普通终端不可见，自动降级为纯文本）、credits 背压、
  blobs 传二进制、by-node diff 做增量。
- **本质**：TSP ≈ 在 PTY 上跑的「原生 UI 协议」，Tern 是唯一渲染器；
  表格只是它的 20+ 节点之一。

### 对我们的意义

**不需要 TSP 也能有表格**——dsh 的表格数据来自结构化事件，不是终端文本：

| 数据 | 来源 | 渲染 |
|---|---|---|
| 助手消息里的 Markdown 表格 | `session/follow` 事件（Markdown） | Markdown 渲染器（gpui-kit 自带） |
| 工具结果（JSON/CSV/列表） | `tool/result` 事件 | 判定后渲染为 DataTable / 代码 / 键值 |
| diff | `tool/result`（编辑类工具） | diff 视图（gpui-kit 有） |
| plan / todo / deliverables | 事件里的展示卡片 | 原生卡片 |
| 用量 / context | `session/projections` | 图表/进度条 |

终端 block 里第三方 TUI 的「表格」（lazygit、htop 那类）**永远只是字符网格**——
除非那个程序自己也说 TSP。这是物理限制。

### 什么情况下才需要 TSP

- 想让**第三方终端程序**在我们的 App 里画出原生卡片/表格/图表。
- 代价：协议 + 20 个节点渲染器 + 输入事件路由，独立项目量级。
- 抄的话：tern 的协议文档公开（docs.stencil.so/tern/protocol），SDK 是 MIT
  （Rust/Python/Go/TS 四套）。

## 三、事件 → 渲染器映射（agent block 的判定清单）

```
tool/result
 ├─ 结构化 JSON，键为行数组 → DataTable
 ├─ 结构化 JSON，键为对象  → 键值表（kv）
 ├─ 文本且像 diff          → diff 视图
 ├─ 文本且含 Markdown 表格 → Markdown 渲染
 ├─ 图片块/附件 ID         → 原生图片
 └─ 其他长文本              → 代码块（可折叠、带语法高亮）
```

判定放在 Rust 侧的一次性分类函数里，规则可配置；未知类型回退为代码块。
