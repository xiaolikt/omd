# 启动时间：GPUI 首帧与 UI 构建

本机（ThinkBook 16 G5+ APH：AMD Radeon 780M 核显 + RTX 4050 独显，GNOME/Wayland，
release 构建）的实测结果与四个可复现的优化点。数字都来自代码里的 `stamp()` 打点
（`[t + xx.xxms] ...`，写到 stderr）。

## 结论（热启动，release）

| 阶段 | 优化前 | 现在 |
|---|---|---|
| 字体系统 + `App::new` | 87ms | **1.7–3.5ms** |
| `gpui_kit::init` | 9ms | 2ms |
| 窗口创建（surface + wgpu instance/device/pipelines） | 184ms | **60–75ms** |
| `Root::new` + `render #1` | 5ms | 5ms |
| 首帧上屏（`open_window` 返回） | 97ms | 10–20ms |
| **合计到首帧** | **~390ms** | **~80–115ms（中位 ~100ms）** |
| 到 transcript 就绪（25 块） | ~440ms | **~190–240ms** |

真正刺眼的是"歇一会儿再启动"：独显处于 runtime suspend 时，优化前首帧要
**1.1–1.4s**；现在同一场景 **~100ms**。

## 四个优化点

### 1. Vulkan ICD 钉到直连屏幕的 GPU（最大的一个：1.1s → 0.1s）

`gpui-pre-wgpu` 的适配器排序是「Discrete > Integrated」，所以笔记本上会选独显。
但更早一步就会踩坑：**`wgpu::Instance::new` 枚举所有 Vulkan ICD 时，loader 会把
NVIDIA ICD 加载起来，而 runtime-suspended 的独显被唤醒要 ~1s**（
`/sys/bus/pci/devices/0000:01:00.0/power/runtime_status: suspended -> active`）。
只设 `ZED_DEVICE_ID` 没用——那时候独显已经被唤醒了。

`setup_gpu_preference()` 做两件事（都在 wgpu 初始化之前）：

1. 读 `/sys/bus/pci/devices/*`，找 `class=0x0300*` 且 `boot_vga=1` 的那块（直连屏幕的
   GPU），把它的 4 位十六进制 `device` 塞进 `ZED_DEVICE_ID`；
2. 按它的 `vendor` 把 `VK_ICD_FILENAMES` / `VK_DRIVER_FILES` 钉到对应 ICD
   （0x1002→`radeon_icd.json`、0x8086→`intel_icd.json`、virtio→`virtio_icd.json`；
   NVIDIA 自己当 boot VGA 时不钉，避免无路可退）。

附带好处：渲染和合成器同卡，省掉跨设备 dmabuf 拷贝；独显保持休眠，省电。
两个环境变量用户显式设过就不覆盖。

### 2. fontconfig 只收 15 个字体文件（87ms → 2ms）

`cosmic-text` 的 `FontSystem::new()` 走 fontconfig 枚举全机字体：这台机 4178 个 face，
release 下 87ms，冷启动还要建缓存。UI 只需要「拉丁 sans + mono + CJK + emoji」。

`setup_fontconfig()` 探测固定几个系统目录里的目标文件名，软链到
`~/.local/share/omd/fonts`，写 `~/.local/share/omd/fontconfig.xml`（私有 `<cachedir>`
在 `~/.cache/omd/fontconfig`），再把 `FONTCONFIG_FILE` 指过去。

- 探测不到 CJK/sans/mono 任意一项 → 什么都不做，回落系统全量扫描；
- `FONTCONFIG_FILE` 已被外部设置 → 不碰；
- 字形覆盖：CJK 常用字、全角标点、`「」→★①😀🎉` 均有 face；中文落在
  Noto Sans/Serif CJK（`.ttc` 内含 SC/TC/JP/KR/HK），如需强制简体可把主题
  `font_family` 钉成 `Noto Sans CJK SC`。

### 3. 首帧只铺最后 40 块 + 事件合批（UI 构建）

`render()` 本身很便宜（400 块 7.7ms）；贵的是每块 markdown 的解析 + `measure_all`
布局，**约 0.5ms/块**，全落在首帧上。

- `FIRST_PAINT_BLOCKS = 40`：首帧只渲染尾部 40 块；`start_reveal()` 每帧（16ms）补
  `REVEAL_STEP = 40` 块，铺完自停，顺手 `scroll_to_bottom`。
  400 块合成压测：render 完 → 上屏 **299ms → 168ms**（三次中位数 315 → 168）。
- 事件循环先排干队列，再单次 `apply + notify + refresh_windows`：baseline 回放不再
  逐条重建整份 transcript（日志里是 `event received x6`）。

### 4. 窗口创建：只开 Vulkan，别初始化 EGL/GLES（152ms → 72ms）

用本地补丁把 `gpui-pre` 各阶段打点之后，窗口创建的 152ms 是这么分的（release，热）：

```
[gpu +  0.00ms] renderer new start
[gpu +108.67ms] surface created        ← wgpu::Instance::new：Vulkan loader + EGL/GLES 初始化
[gpu +113.84ms] adapters enumerated    （2 个 adapter：RADV + llvmpipe）
[gpu +120.11ms] surface caps
[gpu +126.38ms] device created
[gpu +136.43ms] surface configured
[gpu +152.71ms] pipelines created      （~15ms：naga 编译 WGSL → SPIR-V）
```

其中 **~84ms 纯粹是 `Backends::VULKAN | GL` 里 GL 那一半**（EGL/EGL_nvidia 初始化）。
Wayland 这边走的是 Vulkan，所以补丁改成：

- 先 `Backends::VULKAN` 建 instance；
- 拿不到可用 device 时**自动退回** `VULKAN | GL`（兜底不变，失败不会黑窗）；
- 环境变量 `OMD_WGPU_BACKENDS=vulkan|gl|all` 可显式覆盖。

结果：`renderer ready` 152ms → **72ms**，窗口创建整体 184ms → **60–75ms**。

## 本地补丁（vendor/gpui-pre）

gpui 上游没有窗口/GPU 初始化的计时钩子，所以这三个 crate 现在 vendored 在仓库里：

```toml
[patch.crates-io]
gpui-pre = { path = "vendor/gpui-pre/gpui-pre" }
gpui-pre-wgpu = { path = "vendor/gpui-pre/gpui-pre-wgpu" }
gpui-pre-linux = { path = "vendor/gpui-pre/gpui-pre-linux" }
```

改动只有两类：

1. **功能**：`WgpuContext::preferred_backends()`（默认 Vulkan）+ `WgpuRenderer::new` 的
   Vulkan→GL 兜底重试。这是 84ms 的来源。
2. **打点**：`gstamp`（wgpu 阶段）/`lstamp`（Wayland 窗口）/`ostamp`（gpui open_window→
   first draw）。默认静默，`OMD_TRACE_GPU=1` 才输出。

不想要补丁：删掉 `Cargo.toml` 里那三行 `[patch.crates-io]` 即可回到 registry 版本
（功能与计时一起消失，其余优化不受影响）。

## 怎么测

```sh
cargo build --release -p omd
LD_LIBRARY_PATH=vendor/libghostty-vt/lib ./target/release/omd 2>/tmp/omd.log
grep '^\[t +' /tmp/omd.log          # 启动瀑布
```

- `OMD_STRESS_BLOCKS=400`：启动时注入 400 个合成 markdown 块，压 UI 构建；
- `OMD_STRESS_FULL=1`：对照用，一次全铺（不走按帧补铺）；
- 独显是否被唤醒：`cat /sys/bus/pci/devices/*/power/runtime_status`；
- GPU/窗口阶段：`OMD_TRACE_GPU=1 ./target/release/omd`（打的是 vendored 补丁的点）。

注意：debug 构建同一段代码慢约 8×（字体 712ms、首帧 2.06s），压启动只看 release。
另外"第一次跑新二进制 / 刚开机"仍有 1s 级的一次性开销（Mesa / fontconfig 缓存构建），
第二次起消失。

## 剩下的

- 窗口创建 60–75ms：其中 pipelines ~15ms（naga 编译）、device ~5ms、instance ~16ms、
  surface configure ~6ms、surface caps 波动较大（4–25ms，可能含 compositor 首次交互）。
  wgpu 30 的 pipeline cache 能省掉驱动侧编译，但 naga 仍要跑，收益预计 <10ms，先不做。
- 首帧后 190–240ms 才出现 transcript，这段是 dsh host 挂载 + `session/follow` 的往返，
  不是 UI 的事。
- 输入框光标闪烁每 500ms 触发一次重绘（0.2ms/次），可忽略。
