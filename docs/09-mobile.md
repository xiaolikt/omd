# 09 · 手机版评估

## 结论

**架构上是顺的，难点只在 UI 壳。** 现在的分层天然是「瘦客户端」形状：

```
手机 UI 壳（新写，任选技术）──┐
                            │ HTTP+WS（gateway 协议原样复用）
桌面 UI 壳（现在的 GPUI app）─┤
                            │
dsh host（Node，跑在电脑/服务器，不在手机上）
```

## 容易的部分（工作量大头）

- `crates/transport` + `crates/host` 纯 Rust、零 GUI 依赖，Android NDK / iOS 直接编译
- 会话/审批/流式/终端 attach 全是远程协议，手机是纯视图
- 模式与 tern 的 iPhone 客户端相同：daemon 在桌面，手机只是另一个 client

## 难的部分（都在 UI 壳）

1. **GPUI 移动端 backend 不成熟**：gpui-kit lib.rs 原话「Mobile applications supply
   their backend to `Application::with_platform`」——钩子在，Android/iOS 后端自己补，大活
2. 终端在手机上重做：软键盘、IME、手势滚动/缩放、小屏 cell 尺寸
3. 分发：iOS 开发者账号 + 审核；Android 侧载或商店

## 路线排序

| 路线 | 成本 | 说明 |
|---|---|---|
| 手机浏览器开 `dsh web` | 零 | 官方 web UI 本来就自适应，能用但不是我们的 app |
| UI 壳换 Tauri 2 / egui（mobile 支持） | 中：UI 重写，协议层零改动 | 若移动端成为刚需走这条 |
| 等 GPUI/gpui-kit 官方移动 backend | 等 | 届时桌面手机同一套代码 |

## 现在要守住的纪律

**UI 壳绝不直接碰协议。** 当前已做到：worker 线程独占 transport，UI 只收 `Ev` 消息、
只发 `Cmd`。这条守住，未来换壳（移动/Web）就是纯 UI 工程。

## 相关事实（已核实）

- dsh host 支持多 client 同时连接（`--public-url` + TLS 部署模式，见 `dsh-web-app` README
  的 public deployments 一节）——手机连远程 host 是官方支持的用法
- `terminal/attach` 是「最新 attach 持有输入权」，多端切换输入权自动转移
  （terminal-controller README：「The latest attachment owns input and resize」）
- 会话是单写者：手机和桌面不能同时 prompt 同一会话（`session/writer-held`），
  但可以同时**看**（follow 是只读的）
