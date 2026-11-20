# 07 · 风险与路线图

## 风险与对策

| 风险 | 影响 | 对策 |
|---|---|---|
| 网关是内部 API，无 wire-stable 承诺 | 升级 dsh 可能破坏客户端 | ① 锁定 dsh 版本（profile `package.json` pin）；② `contracts/endpoints.json` 快照 + CI diff；③ 端点级兼容层（旧名映射表） |
| 严格参数校验 | 未知字段直接报错 | 优点（错误可见）；用 Schema 生成 Rust 类型避免手写漂移 |
| 插件安装 = 代码执行 | 供应链风险 | 见 06：inspect 预览、registry 白名单、脚本默认禁用、可取消/回滚 |
| 认证细节（token → cookie、Host/Origin） | 连接失败难排查 | 只绑 loopback；启动日志打印握手各步；cookie 只存内存 |
| GPUI 生态相对新 | 组件缺口 | gpui-kit 已覆盖 dock/markdown/diff/DataTable/编辑器；缺的用 `gpui-base` 自建 |
| libghostty-vt 是 C ABI | FFI 维护成本 | 薄封装 + 固定版本；出问题可退回 alacritty_terminal（牺牲 Kitty 图形） |
| 会话规模（长会话/大回滚） | 内存/卡顿 | 虚拟列表（gpui-kit）+ 回滚分页 + core 侧压缩（若自建终端 core） |
| 引擎进程生命周期 | App 重启丢会话 | 守护进程模式（可选）；否则冷启动重连 |

## 评审（2026-11-09）

原路线把「dsh 全功能 GUI」和「herdr 多路复用器」捆在一起，估时不可信。修正：

- v1 不接 herdr-core。ANSI 快照不能当终端字节流用。见 [08](08-build-on-herdr.md)。
- 不先 codegen 144 个端点。M0 只手写用到的几个；schema 快照留着做升级 diff。
- M2 的「一周跑通 yazi + Kitty 图」不成立。herdr 的终端渲染是数千行。v1 终端只要求 vim 能用。
- 对话渲染的未知数不是 GPUI，是 `session/follow` 的事件 payload（契约里没有）。M0 必须落一份真实流。

## 里程碑（修订）

| 阶段 | 内容 | 验收 | 估时 |
|---|---|---|---|
| **M0** ✅ | spawn host；token→cookie；`session/list`；`$events`；一次 `session/prompt`；把 follow 流存盘 | 终端能打出真实回复；事件样本入库 | 已完成 |
| **M1** 🚧 | GPUI：会话列表 + 消息流 + IME 输入 + 增量流 + 新会话（已完成）；待办：审批弹窗、停止按钮 | 能在窗口里做完一轮带审批的对话 | 进行中 |
| **M2** | `terminal/attach` + `alacritty_terminal`，能输入、resize | vim 能用。不承诺 Kitty 图 | 1–2 周 |
| **M3** 🚧 | 文件树 + diff；插件列表/安装/卸载/市场；消息反馈；会话 fork/置顶/归档；slash 命令面板 | 日常不用开浏览器 | 进行中 |
| **以后** | Kitty 图形、子代理/目标/日程、账号、多窗口；若要多路复用再评估 fork herdr | 替代 web UI | 不估 |

M0–M3 单人大约 **6–10 周**到「能不用 web UI」。原「1.5 个月全功能、一周终端」作废。

## 验收清单（功能完整性）

- [x] 会话：列表/新建/重命名/搜索/fork/归档/置顶（分页读取仍是整段 follow）
- [x] 对话：流式消息、reasoning、工具卡片、图片、附件、反馈
- [x] 审批：允许/拒绝（始终允许尚未单独做 UI）
- [x] 提问问卷：问答回传
- [ ] 终端：PTY 字节渲染、输入、resize、滚动、搜索、Kitty 图形、粘贴图片
- [ ] 文件：树、查看、只读/统计、变更 diff
- [x] 模型：目录、切换（effort 未做）
- [x] 设置/凭据：动态表单、掩码、凭据增删
- [x] 目标：goal 横幅（create/edit/pause/resume/complete/clear）
- [x] 日程（schedule/*）：列表/历史/删除（创建走 agent 工具）
- [ ] 子代理/任务：列表、跟进、打断、kill（job 列表/kill 已做，subagent 未做）
- [x] 插件：已装/加载状态/预设组成、安装（含进度/取消）、卸载、启停、市场搜索（市场分页未接）
- [x] 账号：登录（系统浏览器）、状态、余额、登出
- [x] 命令：`commands/list` 驱动的命令面板
- [ ] 稳定性：重启 App 后会话/终端状态不丢（守护进程模式）

## 明确的非目标

- 不复刻浏览器前端的组件实现（`dsh-client-ui-*` 只是参考样本）。
- 不自建 TSP 兼容层（除非要做第三方终端富 UI 生态）。
- 不改 dsh 引擎（保持官方发行版；只读消费其协议）。
