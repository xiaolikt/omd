# 08 · 构建选项：herdr 壳 + dsh 内核

## 结论（评审后）

「herdr + dsh 内核」是**架构模式**（原生 GPU 壳 + 引擎子进程），不是「把 herdr-core 嵌进来」。

v1 **不接 herdr-core**。dsh 自己的终端已经有真 PTY 字节流（`terminal/attach`）。
herdr-core 的公开 API 只给 ANSI 快照，再喂给第二份 VT **不能**还原终端。

## 事实核查

| 事实 | 证据 |
|---|---|
| herdr / herdr-core / Herdr TUI **全部 Apache-2.0** | `github.com/herdrdev/herdr`、`herdrdev/herdr-core`；README「build your own app on top of herdr-core」 |
| herdr-core = 无界面服务器，管 PTY/进程/会话/多机 | herdr-core README：「Core draws nothing」 |
| herdr 终端仿真 = vendored `libghostty-vt` | 二进制符号；终端状态在 core，窗口拉 cell |
| herdr 公开 socket API = **protocol 22**，**文本级** | `pane.read` 的 `ReadFormat` 只有 `text\|ansi`；成功响应无 cell/grid/attr |
| herdr 应用层 = GPUI | `~/.config/herdr-app/AGENTS.md` |

## 三种形态（评审）

### O1 否决：herdr-core + 自研壳，ANSI 再解析

`pane.read` 是快照，不是 PTY 字节流。交替屏幕、Kitty 图形、鼠标跟踪、同步输出、光标、压缩回滚都在 VT 状态机里。把 ANSI 再喂给 `libghostty-vt` 画出来的**不是真终端**。要用 herdr 的终端，只能 fork 应用，或给 herdr-core 加 cell API。

### O2 以后再说：fork herdr 应用

白拿布局、终端渲染、多机、通知。代价是跟 ~23 万行上游走。只有在确认「还要做多路复用器」时才值得。v1 不做。

### O3 采用：自研 GPUI 壳，只消费 dsh

```
┌ GPUI 壳（我们）──────────────────────────┐
│ 对话 / 审批 / 文件 / 插件 / 终端视图      │
│ 终端视图 = 一份 VT，吃 terminal/attach   │
└──────────────────▲───────────────────────┘
                   │ Typert gateway
┌──────────────────┴───────────────────────┐
│ dsh --profile web（Node，headless）       │
│ 会话 · 工具 · PTY · 插件 · ~/.dsh         │
└──────────────────────────────────────────┘
```

dsh 已经持有 PTY。我们不另管进程，只渲染 `terminal/attach` 的字节。
App 重启不丢会话：不杀 host 即可，日志在 `~/.dsh`。不需要 herdr-core。

## v1 责任边界

| 能力 | 归属 |
|---|---|
| 会话、消息、审批、提问、工具卡片、插件 | gateway 客户端 |
| dsh shell 的进程与 PTY | dsh `terminal/*` |
| 像素渲染 | 我们。先 `alacritty_terminal`（能跑 vim）；Kitty 图形再换 `libghostty-vt` |
| 任意 shell 多路复用、多机 | 不做。那是 herdr |

## 待验证（只跟 dsh 有关）

- token → cookie 握手，以及 `$events` / `session/follow` 的真实帧形状（契约 JSON 没有事件 payload）。
- `terminal/attach` 下行是原始 PTY 字节还是已封装帧（决定 VT 怎么喂）。
- host 子进程在 App 退出后能否被重新 attach（决定要不要 daemon 化）。
