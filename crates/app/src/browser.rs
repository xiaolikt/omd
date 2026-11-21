//! 浏览器块：CEF 离屏渲染到 GPUI，CDP 端口供 agent 控制。
//!
//! Linux 原生 Wayland 使用 `on_paint` 像素上传；X11 可使用 DMA-BUF 共享纹理。
//! 原生子窗口只用于 X11，Wayland 不支持嵌入另一个客户端的窗口。
//!
//! 这里只管：CEF 进程生命周期、离屏 browser、帧的存取、CDP 端口。
//! 页面内的标注层由注入的 JS 提供；可信输入走 BrowserHost::send_*（比
//! CDP Input 还近一层）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use cef::*;

/// 一帧 GPUI 纹理（来自 DMA-BUF 导入或像素上传）。
#[derive(Clone, Copy)]
pub struct AccelFrame {
    pub width: u32,
    pub height: u32,
    pub seq: u64,
}

#[derive(Default)]
struct Slot {
    browser: Option<Browser>,
    /// CEF browser id，回调里靠它找到对应 slot。
    id: Option<i32>,
    view: (i32, i32),
    upload_texture: Option<wgpu::Texture>,
    accel: Option<AccelFrame>,
    /// 是否走 windowless（OSR）渲染。子窗口模式由 CEF 自己跟 vsync，不能再推 begin frame。
    windowless: bool,
    /// 待裁剪的元素截图请求：(pin_id, x, y, w, h)，下一帧 on_paint 时兑现。
    capture_req: Vec<(u64, u32, u32, u32, u32)>,
    /// 已落盘的元素截图：(pin_id, png 路径)。
    captured: Vec<(u64, String)>,
    /// 上传纹理的对齐暂存缓冲（按行补 256 对齐用，复用避免每帧 ~3MB 分配）。
    staging: Vec<u8>,
    /// 子窗口模式的 X11 宿主窗口（32 位 GPUI 窗口和 24 位 CEF 子窗口之间的桥）。
    host: Option<u32>,
}


/// 每个 browser 一份；index 对应 UI 侧的 browser pane。
static SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());
/// 标注层回传：on_console_message 收到 `[omd-pins]` 前缀的消息就排队。
static PIN_QUEUE: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());
/// 标注层注入成功（页面里执行到末尾）的 browser id。
static ANNOTATOR_READY: Mutex<Vec<i32>> = Mutex::new(Vec::new());
/// 页内标注层（hover 高亮 / 点击 pin）。
const ANNOTATOR_JS: &str = include_str!("annotator.js");
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);
/// CEF 是否已在本进程初始化（shutdown 守卫）。
static CEF_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 当前进程是否 windowless。Chromium 的 Ozone 平台开关据此决定。
static WINDOWLESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 初始化前选定 Ozone 后端，避免 CEF 修改环境后改变 GPUI 的会话选择。
static WAYLAND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// UI 侧当前展示的浏览器槽位（None = 没有浏览器块）。
static PRIMARY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

pub fn set_primary(index: Option<usize>) {
    PRIMARY.store(index.unwrap_or(usize::MAX), Ordering::Relaxed);
}

pub fn primary() -> Option<usize> {
    match PRIMARY.load(Ordering::Relaxed) {
        usize::MAX => None,
        index => Some(index),
    }
}

/// CEF 请求的下一次泵延迟（毫秒）。i32::MAX = 暂时没活，不要空转。
static PUMP_DELAY_MS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(i32::MAX as i64);
/// 唤醒泵循环的通道：CEF 调度时立刻叫醒 UI 线程，而不是等定时器。
static PUMP_WAKE: std::sync::OnceLock<async_channel::Sender<()>> = std::sync::OnceLock::new();

/// CEF 通过 `on_schedule_message_pump_work` 告诉我们多久以后需要再泵一次。
pub fn note_pump_work(delay_ms: i64) {
    PUMP_DELAY_MS.store(delay_ms, Ordering::Relaxed);
    if let Some(tx) = PUMP_WAKE.get() {
        let _ = tx.try_send(());
    }
}

pub fn pump_delay_ms() -> i64 {
    PUMP_DELAY_MS.load(Ordering::Relaxed)
}

/// 泵循环等待用：CEF 调度时立即返回。
pub fn pump_channel() -> async_channel::Receiver<()> {
    let (tx, rx) = async_channel::unbounded();
    let _ = PUMP_WAKE.set(tx);
    rx
}

/// 上一次同步给 X11 的矩形，避免每帧都发 ConfigureWindow。
static LAST_RECT: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);
/// 外部纹理在 gpui-wgpu 注册表里的 key（单浏览器固定 0）。
/// 外部纹理在 gpui-wgpu 表里的 key：每个 browser 一份，
/// 否则多个浏览器 pane 会抢同一张纹理，全部显示最后画的那一个。
/// `create` 时决定。为真时 CEF 只走 `on_accelerated_paint`，像素上传不能再盖掉导入的纹理。
static SHARED_TEXTURE: AtomicBool = AtomicBool::new(false);

fn slots() -> std::sync::MutexGuard<'static, Vec<Slot>> {
    SLOTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// CEF 的 renderer/gpu/utility 子进程会用 `--type=xxx` 重新执行本程序。
/// 命中时必须在做任何事之前调用 `execute_process` 并带着退出码返回。
pub fn run_subprocess_if_requested() -> Option<i32> {
    if !std::env::args().any(|arg| arg.starts_with("--type=")) {
        return None;
    }
    // CEF 133+ 要求进程内第一次 CEF 调用是 api_hash，否则后续对象包装
    // 会报 "CToCpp called with invalid version -1" 直接 FATAL。
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let args = args::Args::new();
    Some(execute_process(
        Some(args.as_main_args()),
        None,
        std::ptr::null_mut(),
    ))
}

/// vendored 的 CEF 二进制目录（cef-dll-sys 在编译期把它写进了 CEF_DIR）。
pub fn cef_dir() -> Option<PathBuf> {
    cef::sys::get_cef_dir()
}

fn executable_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    // target/<profile>/omd -> 仓库根
    Some(dir.parent()?.parent()?.to_path_buf())
}

/// 启动 CEF 主进程。`remote_port` 给 0 时由 CEF 随机选端口，
/// 真实端口从 `<cache>/DevToolsActivePort` 读。
pub fn init(remote_port: i32, cache_dir: &std::path::Path, windowless: bool) -> bool {
    // 同上：api_hash 必须先于一切 CEF 调用。
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    static INITIALIZED: Mutex<bool> = Mutex::new(false);
    if *INITIALIZED.lock().unwrap() {
        return true;
    }
    let Some(dir) = cef_dir().or_else(|| {
        let root = executable_dir()?;
        let cef = root.join("vendor/cef");
        // 目录名跟 cef-dll-sys 的 OsAndArch 一致（如 cef_linux_x86_64），不写死。
        std::fs::read_dir(&cef)
            .ok()?
            .filter_map(|e| e.ok())
            .flat_map(|version| {
                std::fs::read_dir(version.path())
                    .map(|it| it.filter_map(|e| e.ok()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .map(|platform| platform.path())
            .find(|platform| platform.join("Release/libcef.so").exists())
    }) else {
        eprintln!("[browser] 找不到 CEF 二进制目录，浏览器块不可用");
        return false;
    };

    let mut settings = Settings::default();
    // chrome-sandbox 需要 SUID root；omd 是本机开发工具，直接关沙箱。
    settings.no_sandbox = 1;
    // Linux 上不支持多线程消息循环，改用外部泵。
    settings.external_message_pump = 1;
    // 子窗口模式让 CEF 自己建窗口；OSR 模式画进内存缓冲。
    settings.windowless_rendering_enabled = i32::from(windowless);
    WINDOWLESS.store(windowless, std::sync::atomic::Ordering::Relaxed);
    WAYLAND.store(
        windowless
            && std::env::var_os("OMD_FORCE_X11").is_none()
            && std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()),
        Ordering::Relaxed,
    );
    settings.remote_debugging_port = remote_port;
    settings.log_severity = LogSeverity::INFO;
    settings.resources_dir_path = CefString::from(dir.join("Resources").to_string_lossy().as_ref());
    settings.locales_dir_path = CefString::from(dir.join("locales").to_string_lossy().as_ref());
    settings.cache_path = CefString::from(cache_dir.to_string_lossy().as_ref());
    settings.root_cache_path = CefString::from(cache_dir.to_string_lossy().as_ref());
    // 和 GPUI 面板底色一致，避免白闪。
    settings.background_color = 0xff10_1319;

    let args = args::Args::new();
    let mut app = OmdApp::new();
    let rc = initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    // CefInitialize 返回 1 表示成功（cef_dll_sys 原样透传）。
    if rc == 0 {
        eprintln!("[browser] CefInitialize 失败");
        return false;
    }
    *INITIALIZED.lock().unwrap() = true;
    CEF_READY.store(true, Ordering::Relaxed);
    write_cdp_endpoint(remote_port);
    true
}

/// 写 CDP 端点描述，供 Playwright / 任一 CDP 客户端直连。
fn write_cdp_endpoint(port: i32) {
    let dir = crate::data_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let value = serde_json::json!({
        "port": port,
        "http": format!("http://127.0.0.1:{port}/json"),
        "targets": format!("http://127.0.0.1:{port}/json/list"),
        "version": format!("http://127.0.0.1:{port}/json/version"),
        "pid": std::process::id(),
    });
    let _ = std::fs::write(
        dir.join("browser.cdp.json"),
        serde_json::to_string_pretty(&value).unwrap_or_default(),
    );
}

/// 当前 CEF 的 Wayland 后端无法分配共享捕获缓冲，使用像素上传而非 DMA-BUF。
/// Windows 的共享纹理是 D3D11，导入路径还没接，同样走像素上传。
fn accel_available() -> bool {
    cfg!(target_os = "linux")
        && !WAYLAND.load(Ordering::Relaxed)
        && std::env::var_os("OMD_BROWSER_ACCEL").is_some()
        && gpui_wgpu::shared_gpu().is_some_and(|(device, _)| {
            device
                .features()
                .contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF)
        })
}

/// CEF 的 devtools 监听 socket 不是 close-on-exec 的，dsh 的 node 子进程会继承它，
/// omd 退出后端口一直被占。这里在 init 之后把它标成 CLOEXEC。
#[cfg(target_os = "linux")]
pub fn close_on_exec_cdp_port(port: u16) {
    unsafe extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    const F_SETFD: i32 = 2;
    const FD_CLOEXEC: i32 = 1;

    // /proc/net/tcp{,6} 里找监听该端口的 socket inode。
    let want = format!("{port:04X}");
    let mut inodes = Vec::new();
    for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 10 || fields[3] != "0A" {
                continue; // 只看 LISTEN
            }
            let Some(local_port) = fields[1].rsplit(':').next() else {
                continue;
            };
            if !local_port.eq_ignore_ascii_case(&want) {
                continue;
            }
            if let Ok(inode) = fields[9].parse::<u64>() {
                inodes.push(inode);
            }
        }
    }
    if inodes.is_empty() {
        return;
    }

    let Ok(entries) = std::fs::read_dir("/proc/self/fd") else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let Ok(target) = std::fs::read_link(entry.path()) else {
            continue;
        };
        let target = target.to_string_lossy();
        if inodes.iter().any(|inode| target == format!("socket:[{inode}]")) {
            unsafe {
                fcntl(fd, F_SETFD, FD_CLOEXEC);
            }
        }
    }
}

/// 创建原生子窗口：CEF 直接画进父窗口，零拷贝、无需 GPUI 合成。
/// 要求父窗口是 24 位深 X11 窗口（32 位深时 X11 不允许子窗口）。
#[cfg(target_os = "linux")]
pub fn create_child(
    parent: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    url: &str,
) -> Option<usize> {
    let index = {
        let mut slots = slots();
        let index = slots.len();
        slots.push(Slot {
            view: (width, height),
            ..Default::default()
        });
        index
    };

    /*
    父窗口是 GPUI 的 32 位 ARGB 窗口，CEF 的子窗口是 24 位：直接挂上去
    X 会 BadMatch/BadDrawable。这里插一层和根窗口同 depth/visual 的宿主，
    位置和尺寸都由宿主承担，CEF 窗口自己永远是 (0,0,w,h)。
    */
    #[cfg(target_os = "linux")]
    let host = crate::x11_embed::create_host(
        parent as u32,
        x as i16,
        y as i16,
        width.max(1) as u16,
        height.max(1) as u16,
    )?;
    #[cfg(not(target_os = "linux"))]
    let host = parent as u32;

    let mut window_info = WindowInfo::default();
    window_info.runtime_style = RuntimeStyle::ALLOY;
    let bounds = Rect {
        x: 0,
        y: 0,
        width,
        height,
    };
    let window_info = window_info.set_as_child(u64::from(host) as _, &bounds);

    let url = CefString::from(url);
    let mut settings = BrowserSettings::default();
    let mut client = OmdClient::new();
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&url),
        Some(&settings),
        None,
        None,
    )?;

    let id = browser.identifier();
    // 这里只是尽早声明一次；真正生效的是 on_after_created 里的同一组调用。
    if let Some(host) = browser.host() {
        host.was_hidden(0);
    }
    #[cfg(target_os = "linux")]
    if let Some(window) = browser.host().map(|host| host.window_handle()) {
        crate::x11_embed::watch(host, window as usize as u32, parent as u32);
    }
    let slot = &mut slots()[index];
    slot.id = Some(id);
    slot.browser = Some(browser);
    slot.host = Some(host);
    Some(index)
}

/// 子窗口跟随 GPUI 布局：移动/缩放宿主，再把同样的尺寸补给 CEF 的窗口，
/// 让它收到 ConfigureNotify（CefWindowX11 靠这个 resize 浏览器 widget）。
pub fn child_bounds(index: usize, x: i32, y: i32, width: i32, height: i32) {
    {
        let mut last = LAST_RECT.lock().unwrap_or_else(|e| e.into_inner());
        if *last == Some((x, y, width, height)) {
            return;
        }
        *last = Some((x, y, width, height));
    }
    let (host, cef_window) = {
        let slots = slots();
        let Some(slot) = slots.get(index) else { return };
        (
            slot.host,
            slot.browser.as_ref().and_then(|b| b.host()).map(|h| h.window_handle()),
        )
    };
    #[cfg(target_os = "linux")]
    if let Some(host) = host {
        crate::x11_embed::set_bounds(
            host,
            x as i16,
            y as i16,
            width.max(1) as u16,
            height.max(1) as u16,
        );
        if let Some(window) = cef_window {
            crate::x11_embed::set_child_bounds(
                window as usize as u32,
                width.max(1) as u16,
                height.max(1) as u16,
            );
        }
    }
    let _ = (host, cef_window);
}

/// 面板折叠/切标签时整块隐藏，不销毁浏览器（保留会话和滚动位置）。
pub fn set_visible(index: usize, visible: bool) {
    let host = slots().get(index).and_then(|slot| slot.host);
    #[cfg(target_os = "linux")]
    if let Some(host) = host {
        crate::x11_embed::set_visible(host, visible);
    }
    let _ = host;
}

/// 点击落在浏览器区域时把 X11 输入焦点交给 CEF 的子窗口。
pub fn focus(index: usize) {
    let window = slots()
        .get(index)
        .and_then(|slot| slot.browser.as_ref().and_then(|b| b.host()))
        .map(|host| host.window_handle());
    #[cfg(target_os = "linux")]
    if let Some(window) = window {
        crate::x11_embed::focus(window as usize as u32);
    }
    let _ = window;
}

/// 拆掉浏览器时先把宿主摘出 GPUI 窗口：Chromium 的 GPU 表面还挂在它上面，
/// 跟着父窗口一起销毁会让 ANGLE 拿到空 visual（三次崩溃后 WebGL 全挂）。
pub fn detach_native_view(index: usize) {
    let host = slots().get_mut(index).and_then(|slot| slot.host.take());
    #[cfg(target_os = "linux")]
    if let Some(host) = host {
        crate::x11_embed::unwatch(host);
        crate::x11_embed::detach(host);
    }
    let _ = host;
}

pub fn create(width: u32, height: u32, url: &str) -> Option<usize> {
    let mut window_info = WindowInfo::default();
    window_info.runtime_style = RuntimeStyle::ALLOY;
    // windowless 也需要一个父窗口句柄给弹窗/显示器定位；0 = 无。
    #[cfg(target_os = "windows")]
    let null_parent: cef::sys::cef_window_handle_t = cef::sys::HWND(std::ptr::null_mut());
    #[cfg(not(target_os = "windows"))]
    let null_parent: cef::sys::cef_window_handle_t = 0;
    let mut window_info = window_info.set_as_windowless(null_parent);
    // OSR 由我们推 begin frame（外部时钟），否则 CEF 内部定时器只在部分场景出帧。
    // 实测：靠我们外部推 begin frame 时 CEF 从不出画（pump 在 GPUI 线程，
    // CEF 认为不是它的 UI 线程）；改用 CEF 自己的合成时钟后首帧立刻到。
    window_info.external_begin_frame_enabled = i32::from(std::env::var_os("OMD_EXT_BF").is_some());
    // Wayland 使用 on_paint；X11 可显式启用 DMA-BUF 导入。
    let shared = accel_available();
    SHARED_TEXTURE.store(shared, Ordering::Relaxed);
    window_info.shared_texture_enabled = i32::from(shared);
    if width == 0 || height == 0 {
        return None;
    }
    let index = {
        let mut slots = slots();
        let index = slots.len();
        slots.push(Slot {
            view: (width as i32, height as i32),
            windowless: true,
            ..Default::default()
        });
        index
    };
    // （window_info 见上）
    let url = CefString::from(url);
    let mut settings = BrowserSettings::default();
    settings.windowless_frame_rate = 60;
    let mut client = OmdClient::new();
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&url),
        Some(&settings),
        None,
        None,
    )?;
    let id = browser.identifier();
    // 不主动声明可见时 Chromium 按后台标签节流，rAF 会掉到 1fps。
    if let Some(host) = browser.host() {
        host.was_hidden(0);
        host.was_resized();
        host.invalidate(PaintElementType::VIEW);
    }
    let slot = &mut slots()[index];
    slot.id = Some(id);
    slot.browser = Some(browser);
    Some(index)
}

/// pane 尺寸变化：改 view_rect 并通知 CEF 重新布局。
pub fn resize(index: usize, width: u32, height: u32) {
    if std::env::var("OMD_BROWSER_DEBUG").is_ok() {
        static LAST: Mutex<(u32, u32)> = Mutex::new((0, 0));
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if *last != (width, height) {
            *last = (width, height);
            eprintln!("[resize] index={index} {width}x{height}");
        }
    }
    let host = {
        let mut slots = slots();
        let Some(slot) = slots.get_mut(index) else { return };
        let next = (width as i32, height as i32);
        if slot.view == next {
            return;
        }
        slot.view = next;
        slot.browser.as_ref().and_then(|b| b.host())
    };
    if let Some(host) = host {
        host.was_resized();
    }
}

/// 取最新的 GPU 帧（accelerated 路径）。纹理本身在 gpui-wgpu 的注册表里，
/// 这里只给尺寸和序号。
pub fn latest_accel(index: usize) -> Option<AccelFrame> {
    slots().get(index).and_then(|slot| slot.accel)
}


/// 某槽位的标注层是否已就绪（页面回过 ack）。
pub fn annotator_ready(index: usize) -> bool {
    let slots = slots();
    let Some(id) = slots.get(index).and_then(|s| s.id) else {
        return false;
    };
    ANNOTATOR_READY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&id)
}

/// 导航会换执行环境，就绪状态作废。
pub fn annotator_reset(index: usize) {
    let id = slots().get(index).and_then(|s| s.id);
    if let Some(id) = id {
        ANNOTATOR_READY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|&other| other != id);
    }
}

/// 注入标注层（幂等，页面里自带 __omdAnnotator 防重入）。
pub fn inject_annotator(index: usize) -> bool {
    execute_js(index, ANNOTATOR_JS)
}

/// 开/关拾取模式。
pub fn set_pick(index: usize, on: bool) -> bool {
    execute_js(
        index,
        &format!("window.__omdSetPick && window.__omdSetPick({on})"),
    )
}

/// 取走标注层回传的 pin 列表（CEF browser id -> JSON 文本）。
pub fn drain_pins() -> Vec<(i32, String)> {
    std::mem::take(&mut *PIN_QUEUE.lock().unwrap_or_else(|e| e.into_inner()))
}

/// 请求对某个 pin 的元素截图：下一帧 on_paint 时从像素缓冲裁剪落盘。
pub fn request_capture(index: usize, pin_id: u64, x: u32, y: u32, w: u32, h: u32) {
    let host = {
        let mut slots = slots();
        match slots.get_mut(index) {
            Some(slot) => {
                slot.capture_req.push((pin_id, x, y, w, h));
                slot.browser.as_ref().and_then(|b| b.host())
            }
            None => None,
        }
    };
    // 静态页面不会自己重绘；推一次 invalidate 让下一帧带着像素来。
    if let Some(host) = host {
        host.invalidate(PaintElementType::VIEW);
    }
}

/// 关闭一个浏览器实例（pane 被关掉时调用；异步完成，槽位保留以稳定下标）。
pub fn close(index: usize) {
    let mut slots = slots();
    if let Some(slot) = slots.get_mut(index) {
        slot.capture_req.clear();
        if let Some(host) = slot.browser.as_ref().and_then(|b| b.host()) {
            host.close_browser(1);
        }
    }
}

/// 取走已落盘的元素截图（槽位, pin_id, 路径）。
pub fn drain_captures() -> Vec<(usize, u64, String)> {
    let mut slots = slots();
    let mut out = Vec::new();
    for (index, slot) in slots.iter_mut().enumerate() {
        for (pin_id, path) in slot.captured.drain(..) {
            out.push((index, pin_id, path));
        }
    }
    out
}

/// 每个槽位的拾取模式开关（UI 与快捷键共享的唯一状态）。
static PICK: Mutex<Vec<(usize, bool)>> = Mutex::new(Vec::new());

/// 拾取模式是否开着。
pub fn pick_on(index: usize) -> bool {
    PICK.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|(i, on)| *i == index && *on)
}

/// 切换拾取模式并同步进页面，返回新状态。
pub fn toggle_pick(index: usize) -> bool {
    let mut pick = PICK.lock().unwrap_or_else(|e| e.into_inner());
    let entry = pick.iter_mut().find(|(i, _)| *i == index);
    let next = match entry {
        Some((_, on)) => {
            *on = !*on;
            *on
        }
        None => {
            pick.push((index, true));
            true
        }
    };
    drop(pick);
    set_pick(index, next);
    next
}

/// 最近一次回传的 pin 集合（槽位 -> JSON 文本），供控制 socket 读取。
static PINS_LAST: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());

/// 记录某槽位最新的 pin 集合（pump 循环里调用）。
pub fn store_pins(index: usize, json: &str) {
    let mut last = PINS_LAST.lock().unwrap_or_else(|e| e.into_inner());
    match last.iter_mut().find(|(i, _)| *i == index) {
        Some(entry) => entry.1 = json.to_string(),
        None => last.push((index, json.to_string())),
    }
}

/// 清空 pin 快照（随消息发出后调用）。
pub fn clear_pins_snapshot() {
    PINS_LAST.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// 全部槽位的 pin 快照。
pub fn pins_snapshot() -> Vec<(usize, String)> {
    PINS_LAST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// CEF browser id -> 槽位号。
pub fn slot_of_browser_id(id: i32) -> Option<usize> {
    slots().iter().position(|s| s.id == Some(id))
}

/// 页面内执行的 JS（标注层、AI 读取 DOM 都走这条路）。
pub fn execute_js(index: usize, code: &str) -> bool {
    let frame = slots().get(index).and_then(|s| {
        s.browser
            .as_ref()
            .and_then(|browser| browser.main_frame())
    });
    let Some(frame) = frame else { return false };
    let code = CefString::from(code);
    let url = CefString::from("");
    frame.execute_java_script(Some(&code), Some(&url), 0);
    true
}

// ---------------------------------------------------------------------------
// OSR 输入转发
// ---------------------------------------------------------------------------

// CEF 的 EVENTFLAG_* 位
pub const EVF_SHIFT: u32 = 1 << 3;
pub const EVF_CTRL: u32 = 1 << 1;
pub const EVF_ALT: u32 = 1 << 0;
pub const EVF_META: u32 = 1 << 2;

/// 这个进程是否走 windowless（OSR）渲染。OSR 下没有原生子窗口，
/// 鼠标和键盘都由 GPUI 收下来再转发给 Chromium。
pub fn osr_mode() -> bool {
    WINDOWLESS.load(Ordering::Relaxed)
}

/// 外部纹理在 gpui-wgpu 表里的 key（按浏览器槽位区分）。
pub fn external_key(index: usize) -> u32 {
    index as u32
}

/// 最新一帧的尺寸；没有帧时返回 None（此时不画）。
pub fn accel_size(index: usize) -> Option<(u32, u32)> {
    slots()
        .get(index)
        .and_then(|slot| slot.accel)
        .filter(|frame| frame.width > 0 && frame.height > 0)
        .map(|frame| (frame.width, frame.height))
}

fn host_at(index: usize) -> Option<BrowserHost> {
    slots()
        .get(index)
        .and_then(|slot| slot.browser.as_ref())
        .and_then(|browser| browser.host())
}

pub fn osr_mouse_move(index: usize, x: i32, y: i32, modifiers: u32) {
    if let Some(host) = host_at(index) {
        host.send_mouse_move_event(Some(&MouseEvent { x, y, modifiers }), 0);
    }
}

pub fn osr_mouse_button(
    index: usize,
    x: i32,
    y: i32,
    button: MouseButtonType,
    up: bool,
    click_count: i32,
    modifiers: u32,
) {
    if let Some(host) = host_at(index) {
        host.send_mouse_click_event(
            Some(&MouseEvent { x, y, modifiers }),
            button,
            i32::from(up),
            click_count,
        );
    }
}

pub fn osr_mouse_wheel(index: usize, x: i32, y: i32, delta_x: i32, delta_y: i32, modifiers: u32) {
    if let Some(host) = host_at(index) {
        host.send_mouse_wheel_event(
            Some(&MouseEvent { x, y, modifiers }),
            delta_x,
            delta_y,
        );
    }
}

/// 页面当前是否该收键盘（鼠标最后点在里面）。
static OSR_FOCUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 当前持有键盘焦点的浏览器槽位（usize::MAX = 无）。
static FOCUSED_SLOT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(usize::MAX);

/// 点进某个浏览器 pane:它拿焦点,其它实例全部让出（多 pane 并存的关键）。
pub fn osr_focus_slot(index: usize, focused: bool) {
    if focused {
        FOCUSED_SLOT.store(index, Ordering::Relaxed);
        OSR_FOCUSED.store(true, Ordering::Relaxed);
        let mut all = Vec::new();
        {
            let slots = slots();
            for (i, slot) in slots.iter().enumerate() {
                all.push((i == index, slot.browser.as_ref().and_then(|b| b.host())));
            }
        }
        for (is_this, host) in all {
            if let Some(host) = host {
                host.set_focus(i32::from(is_this));
            }
        }
    } else {
        FOCUSED_SLOT.store(usize::MAX, Ordering::Relaxed);
        OSR_FOCUSED.store(false, Ordering::Relaxed);
        let mut all = Vec::new();
        {
            let slots = slots();
            for slot in slots.iter() {
                all.push(slot.browser.as_ref().and_then(|b| b.host()));
            }
        }
        for host in all.into_iter().flatten() {
            host.set_focus(0);
        }
    }
}

/// 键盘焦点当前落在哪个浏览器 pane。
pub fn focused_slot() -> Option<usize> {
    match FOCUSED_SLOT.load(Ordering::Relaxed) {
        usize::MAX => None,
        index => Some(index),
    }
}

pub fn osr_focused() -> bool {
    OSR_FOCUSED.load(Ordering::Relaxed)
}

/// GPUI 的 key 名 → Windows 虚拟键码（CEF 的 send_key_event 要这个）。
pub fn vk_for(key: &str) -> Option<i32> {
    let special = [
        ("enter", 13),
        ("tab", 9),
        ("backspace", 8),
        ("escape", 27),
        ("space", 32),
        ("delete", 46),
        ("insert", 45),
        ("left", 37),
        ("up", 38),
        ("right", 39),
        ("down", 40),
        ("home", 36),
        ("end", 35),
        ("pageup", 33),
        ("pagedown", 34),
    ];
    if let Some((_, vk)) = special.iter().find(|(name, _)| *name == key) {
        return Some(*vk);
    }
    if let Some(rest) = key.strip_prefix('f') {
        if let Ok(n) = rest.parse::<i32>() {
            if (1..=12).contains(&n) {
                return Some(111 + n);
            }
        }
    }
    let mut chars = key.chars();
    let (first, rest) = (chars.next()?, chars.next());
    if rest.is_none() {
        return Some(first.to_ascii_uppercase() as i32);
    }
    None
}

/// 无窗口模式的按键：CEF 要求 RAWKEYDOWN → CHAR → KEYUP 三步，
/// 只发 KEYDOWN 页面收不到 keydown（实测 0 次）。
pub fn osr_key(index: usize, windows_key_code: i32, character: u16, modifiers: u32, up: bool) {
    let Some(host) = host_at(index) else { return };
    let send = |type_: KeyEventType, character: u16| {
        let event = KeyEvent {
            size: std::mem::size_of::<KeyEvent>(),
            type_,
            modifiers,
            windows_key_code,
            native_key_code: 0,
            is_system_key: 0,
            character,
            unmodified_character: character,
            focus_on_editable_field: 0,
        };
        host.send_key_event(Some(&event));
    };
    if up {
        send(KeyEventType::KEYUP, 0);
        return;
    }
    send(KeyEventType::RAWKEYDOWN, 0);
    if character != 0 {
        send(KeyEventType::CHAR, character);
    }
}

/// OSR 下没有原生焦点可给，只能显式告诉 Chromium。
pub fn osr_focus(index: usize, focused: bool) {
    if let Some(host) = host_at(index) {
        host.set_focus(i32::from(focused));
    }
}

/// 取这个槽位的 Browser 句柄。
fn browser_at(index: usize) -> Option<Browser> {
    slots().get(index).and_then(|slot| slot.browser.clone())
}

/// 每个浏览器最新一次页面标题（on_title_change 维护，id = CEF browser id）。
static TITLES: Mutex<Vec<(i32, String)>> = Mutex::new(Vec::new());

/// 页面标题（下拉菜单头/pane 标题用）；没有就 None。
pub fn title_of(index: usize) -> Option<String> {
    let id = slots().get(index)?.id?;
    let titles = TITLES.lock().unwrap_or_else(|e| e.into_inner());
    titles
        .iter()
        .rev()
        .find(|(slot_id, _)| *slot_id == id)
        .map(|(_, title)| title.clone())
}

/// 能否后退（工具条 ← 按钮的可用态）。
pub fn can_go_back(index: usize) -> bool {
    browser_at(index)
        .map(|browser| browser.can_go_back() != 0)
        .unwrap_or(false)
}

/// 能否前进（工具条 → 按钮的可用态）。
pub fn can_go_forward(index: usize) -> bool {
    browser_at(index)
        .map(|browser| browser.can_go_forward() != 0)
        .unwrap_or(false)
}

pub fn go_back(index: usize) {
    if let Some(browser) = browser_at(index) {
        browser.go_back();
    }
}

pub fn go_forward(index: usize) {
    if let Some(browser) = browser_at(index) {
        browser.go_forward();
    }
}

pub fn reload(index: usize) {
    if let Some(browser) = browser_at(index) {
        browser.reload();
    }
}

/// 当前页面地址（同步可用，header 每帧读）。
pub fn current_url(index: usize) -> Option<String> {
    let browser = browser_at(index)?;
    let frame = browser.main_frame()?;
    let userfree = frame.url();
    let url = CefString::from(&userfree).to_string();
    (!url.is_empty()).then_some(url)
}

pub fn navigate(index: usize, url: &str) -> bool {
    let frame = slots().get(index).and_then(|s| {
        s.browser
            .as_ref()
            .and_then(|browser| browser.main_frame())
    });
    let Some(frame) = frame else { return false };
    let url = CefString::from(url);
    frame.load_url(Some(&url));
    true
}

/*
CDXC:CefRuntime:
CEF 的 external message pump 只排空 Chromium 自己的任务队列。Linux 上 UI 线程
的 X11 事件源、fd watcher 都挂在 thread-default GMainContext 上，由 embedder 的
glib 主循环派发（cefclient 有 GTK 循环白拿）。GPUI 跑的是 calloop，不派发 glib
source 的话：CreateWindow 的错误没人收、MapNotify/ConfigureNotify 永远不来、
合成器不出帧——窗口黑的，但页面 DOM 一切正常。

glib 符号从已加载的 libglib-2.0 解析（libcef.so 硬依赖它），不增加链接依赖。
*/
#[cfg(target_os = "linux")]
fn glib_dispatch() -> u32 {
    struct Glib {
        context: *mut std::ffi::c_void,
        pending: unsafe extern "C" fn(*mut std::ffi::c_void) -> i32,
        iteration: unsafe extern "C" fn(*mut std::ffi::c_void, i32) -> i32,
    }
    // 只在 GPUI 主线程用，值不跨线程；这里为了放进 OnceLock 才声明。
    unsafe impl Send for Glib {}
    unsafe impl Sync for Glib {}

    static GLIB: std::sync::OnceLock<Option<Glib>> = std::sync::OnceLock::new();
    let Some(glib) = GLIB.get_or_init(|| unsafe {
        unsafe extern "C" {
            fn dlopen(filename: *const std::ffi::c_char, flag: i32) -> *mut std::ffi::c_void;
            fn dlsym(handle: *mut std::ffi::c_void, symbol: *const std::ffi::c_char)
                -> *mut std::ffi::c_void;
        }
        const RTLD_NOW: i32 = 2;
        let handle = dlopen(c"libglib-2.0.so.0".as_ptr(), RTLD_NOW);
        if handle.is_null() {
            eprintln!("[browser] libglib-2.0 未加载，子窗口可能黑屏");
            return None;
        }
        let default_context = dlsym(handle, c"g_main_context_default".as_ptr());
        let pending = dlsym(handle, c"g_main_context_pending".as_ptr());
        let iteration = dlsym(handle, c"g_main_context_iteration".as_ptr());
        if default_context.is_null() || pending.is_null() || iteration.is_null() {
            return None;
        }
        let get_context: unsafe extern "C" fn() -> *mut std::ffi::c_void =
            std::mem::transmute(default_context);
        Some(Glib {
            context: get_context(),
            pending: std::mem::transmute(pending),
            iteration: std::mem::transmute(iteration),
        })
    }) else {
        return 0;
    };
    let mut n = 0;
    unsafe {
        // 上限防止 pending 一直为真时卡死 UI 线程。
        for _ in 0..256 {
            if (glib.pending)(glib.context) == 0 {
                break;
            }
            (glib.iteration)(glib.context, 0);
            n += 1;
        }
    }
    n
}

/// Windows 上没有 glib 主循环要派发；CEF 在那边用自己的内部泵。
#[cfg(not(target_os = "linux"))]
fn glib_dispatch() -> u32 {
    0
}

pub fn pump() {
    // external begin frame：CEF 不自己出帧，由我们按 60Hz 推合成器。
    // 不推的话页面的 rAF 照跑但 on_paint 永远不来。
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let now = std::time::Instant::now();
    let due = {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        match *last {
            Some(t) if now.duration_since(t).as_micros() < 16_666 => false,
            _ => {
                *last = Some(now);
                true
            }
        }
    };
    if due && std::env::var_os("OMD_EXT_BF").is_some() {
        let hosts: Vec<_> = slots()
            .iter()
            .filter(|s| s.windowless)
            .filter_map(|s| s.browser.as_ref().and_then(|b| b.host()))
            .collect();
        for host in hosts {
            host.send_external_begin_frame();
        }
    }
    let before = glib_dispatch();
    do_message_loop_work();
    let after = glib_dispatch();
    {
        static N: AtomicU64 = AtomicU64::new(0);
        static T: Mutex<Option<std::time::Instant>> = Mutex::new(None);
        N.fetch_add(u64::from(before + after), Ordering::Relaxed);
        let mut t = T.lock().unwrap_or_else(|e| e.into_inner());
        let start = *t.get_or_insert_with(std::time::Instant::now);
        if start.elapsed().as_secs() >= 1 {
            eprintln!("[pump] glib iterations/s = {}", N.swap(0, Ordering::Relaxed));
            *t = Some(std::time::Instant::now());
        }
    }
    if std::env::var("OMD_BROWSER_DEBUG").is_ok() {
        static C: AtomicU64 = AtomicU64::new(0);
        static T: Mutex<Option<std::time::Instant>> = Mutex::new(None);
        let n = C.fetch_add(1, Ordering::Relaxed) + 1;
        let mut t = T.lock().unwrap_or_else(|e| e.into_inner());
        let start = *t.get_or_insert_with(std::time::Instant::now);
        if start.elapsed().as_secs() >= 1 {
            eprintln!("[pump] {n} ticks/s");
            *t = Some(std::time::Instant::now());
            C.store(0, Ordering::Relaxed);
        }
    }
}

pub fn shutdown() {
    if !CEF_READY.swap(false, Ordering::Relaxed) {
        return;
    }
    // 端点描述只在本进程存活期间有效。
    if let Some(home) = std::env::var_os("HOME") {
        let _ = std::fs::remove_file(
            std::path::PathBuf::from(home).join(".local/share/omd/browser.cdp.json"),
        );
    }
    slots().clear();
    cef::shutdown();
}


// ---------------------------------------------------------------------------
// GPU 共享纹理探测（Linux DMA-BUF）
// ---------------------------------------------------------------------------

/// 自建一个 Vulkan wgpu 设备，专门用来导入 CEF 的 DMA-BUF。
/// 正式实现里应该复用 GPUI 的 device/queue（同一物理 GPU 即可）。
#[cfg(target_os = "linux")]
fn gpu_probe() -> Option<(wgpu::Device, wgpu::Queue)> {
    // 必须和 GPUI 用同一个设备的 device，否则导入出来的纹理画不上去。
    gpui_wgpu::shared_gpu()
}

/// 把导入的纹理读回内存并存成 PNG，验证内容是否正确。
#[cfg(target_os = "linux")]
fn dump_texture(device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture, path: &str) {
    let (width, height) = (texture.width(), texture.height());
    let unpadded = width * 4;
    let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("cursor-readback"),
        size: u64::from(padded) * u64::from(height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("cursor-readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    let submission = queue.submit(std::iter::once(encoder.finish()));
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: Some(submission),
        timeout: Some(std::time::Duration::from_secs(10)),
    });
    if rx.recv_timeout(std::time::Duration::from_secs(10)).is_err() {
        eprintln!("[accel] readback 超时");
        return;
    }
    let mapped = match buffer.slice(..).get_mapped_range() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[accel] readback map 失败: {e}");
            return;
        }
    };
    let mut rgba = Vec::with_capacity((unpadded * height) as usize);
    for row in mapped.chunks_exact(padded as usize) {
        let mut px = row[..unpadded as usize].to_vec();
        for p in px.chunks_exact_mut(4) {
            p.swap(0, 2); // BGRA -> RGBA
        }
        rgba.extend_from_slice(&px);
    }
    if let Some(img) = image::RgbaImage::from_raw(width, height, rgba) {
        match img.save(path) {
            Ok(()) => eprintln!("[accel] 已写出 {path} ({width}x{height})"),
            Err(e) => eprintln!("[accel] 写 PNG 失败: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// CEF App：命令行开关
// ---------------------------------------------------------------------------

cef::wrap_browser_process_handler! {
    struct OmdBrowserProcess {}
    impl BrowserProcessHandler {
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            note_pump_work(delay_ms);
        }
    }
}

cef::wrap_app! {
    struct OmdApp {}
    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            use cef::ImplCommandLine as _;
            let Some(cmd) = command_line else { return };
            #[cfg(target_os = "linux")]
            {
                let platform = if WAYLAND.load(Ordering::Relaxed) {
                    "wayland"
                } else {
                    "x11"
                };
                cmd.append_switch_with_value(
                    Some(&CefString::from("ozone-platform")),
                    Some(&CefString::from(platform)),
                );
            }
            // 没有 GPU 的环境（Xvfb、远程会话）跑 OSR：让 Chromium 退到软件合成，
            // 否则 GPU 进程反复崩掉。
            if std::env::var("OMD_BROWSER_CPU").is_ok() {
                cmd.append_switch(Some(&CefString::from("disable-gpu")));
                cmd.append_switch(Some(&CefString::from("disable-gpu-compositing")));
            }
            // 少留一个空闲 renderer（Chromium 会提前起一个备用进程，约 100MB）。
            cmd.append_switch_with_value(
                Some(&CefString::from("disable-features")),
                Some(&CefString::from("SpareRendererForSitePerProcess")),
            );
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(OmdBrowserProcess::new())
        }
    }
}

/// 从 BGRA 帧里裁一块存成 PNG，返回路径（元素截图，附在 pin 上）。
fn crop_png(
    pixels: &[u8],
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    index: usize,
    pin_id: u64,
) -> Option<String> {
    let x = x.min(width.saturating_sub(1));
    let y = y.min(height.saturating_sub(1));
    let w = w.min(width - x).max(1);
    let h = h.min(height - y).max(1);
    // 拾取时元素带着 2px 高亮框，往里缩一圈截到干净内容。
    let (x, y, w, h) = if w > 8 && h > 8 {
        (x + 2, y + 2, w - 4, h - 4)
    } else {
        (x, y, w, h)
    };
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for row in y..y + h {
        let start = ((row * width + x) * 4) as usize;
        let end = start + (w * 4) as usize;
        let line = pixels.get(start..end)?;
        for px in line.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 0xff]);
        }
    }
    let img = image::RgbaImage::from_raw(w, h, rgba)?;
    let dir = std::path::PathBuf::from(
        std::env::var("HOME").ok()?,
    )
    .join(".local/share/omd/pins");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(format!("pin-{index}-{pin_id}.png"));
    img.save(&path).ok()?;
    Some(path.display().to_string())
}

/// 把 CEF 的帧写进纹理。脏区合计不足 80% 时按脏区分块上传（动画页
/// 大多数帧只动一小块），否则整帧上传。暂存缓冲复用,行宽按 256 对齐。
#[allow(clippy::too_many_arguments)]
fn upload_frame(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    pixels: &[u8],
    width: u32,
    height: u32,
    dirty: Option<&[Rect]>,
    staging: &mut Vec<u8>,
) {
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_row = |row: u32| row.div_ceil(align) * align;
    let rects: Vec<(u32, u32, u32, u32)> = dirty
        .map(|list| {
            list.iter()
                .filter_map(|r| {
                    let x = r.x.max(0) as u32;
                    let y = r.y.max(0) as u32;
                    let w = (r.width.max(0) as u32).min(width.saturating_sub(x));
                    let h = (r.height.max(0) as u32).min(height.saturating_sub(y));
                    (w > 0 && h > 0).then_some((x, y, w, h))
                })
                .collect()
        })
        .unwrap_or_default();
    let dirty_area: u64 = rects
        .iter()
        .map(|&(_, _, w, h)| w as u64 * h as u64)
        .sum();
    let full_frame = rects.is_empty()
        || dirty_area * 5 >= width as u64 * height as u64 * 4;
    let target = |origin: wgpu::Origin3d| wgpu::TexelCopyTextureInfo {
        texture,
        mip_level: 0,
        origin,
        aspect: wgpu::TextureAspect::All,
    };
    if full_frame {
        let unpadded = width * 4;
        let padded = padded_row(unpadded);
        if padded == unpadded {
            queue.write_texture(
                target(wgpu::Origin3d::ZERO),
                pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(unpadded),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        } else {
            staging.clear();
            staging.reserve(padded as usize * height as usize);
            for row in pixels.chunks_exact(unpadded as usize) {
                staging.extend_from_slice(row);
                staging.resize(staging.len() + (padded - unpadded) as usize, 0);
            }
            queue.write_texture(
                target(wgpu::Origin3d::ZERO),
                staging,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            );
        }
        return;
    }
    for (dx, dy, dw, dh) in rects {
        let row = dw * 4;
        let row_padded = padded_row(row);
        staging.clear();
        staging.reserve(row_padded as usize * dh as usize);
        for r in dy..dy + dh {
            let start = ((r * width + dx) * 4) as usize;
            let end = start + row as usize;
            let Some(line) = pixels.get(start..end) else { break };
            staging.extend_from_slice(line);
            if row_padded > row {
                staging.resize(staging.len() + (row_padded - row) as usize, 0);
            }
        }
        queue.write_texture(
            target(wgpu::Origin3d { x: dx, y: dy, z: 0 }),
            staging,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row_padded),
                rows_per_image: Some(dh),
            },
            wgpu::Extent3d { width: dw, height: dh, depth_or_array_layers: 1 },
        );
    }
}

// ---------------------------------------------------------------------------
// CEF 回调
// ---------------------------------------------------------------------------

cef::wrap_client! {
    struct OmdClient {}
    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(OmdRender::new())
        }

        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(OmdDisplay::new())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(OmdLifeSpan::new())
        }
    }
}

cef::wrap_life_span_handler! {
    struct OmdLifeSpan {}
    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            // 必须等 browser 真正创建完再声明可见，否则 Chromium 仍按后台标签节流
            // （rAF 掉到 1fps，界面看起来"冻住"）。
            if let Some(host) = browser.host() {
                host.was_hidden(0);
                host.was_resized();
                host.invalidate(PaintElementType::VIEW);
            }
        }

        fn on_before_close(&self, browser: Option<&mut Browser>) {
            let Some(id) = browser.as_ref().map(|b| b.identifier()) else {
                return;
            };
            let mut slots = slots();
            let Some(index) = slots.iter().position(|s| s.id == Some(id)) else {
                return;
            };
            let slot = &mut slots[index];
            slot.browser = None;
            slot.upload_texture = None;
            slot.accel = None;
            slot.capture_req.clear();
            slot.captured.clear();
            slot.staging.clear();
            slot.staging.shrink_to_fit();
            let next = slots.iter().position(|s| s.browser.is_some());
            drop(slots);
            gpui_wgpu::unregister_external_texture(index as u32);
            PICK.lock().unwrap_or_else(|e| e.into_inner()).retain(|(i, _)| *i != index);
            if FOCUSED_SLOT.load(Ordering::Relaxed) == index {
                FOCUSED_SLOT.store(usize::MAX, Ordering::Relaxed);
                OSR_FOCUSED.store(false, Ordering::Relaxed);
            }
            if primary() == Some(index) {
                set_primary(next);
            }
        }
    }
}

cef::wrap_display_handler! {
    struct OmdDisplay {}
    impl DisplayHandler {
        fn on_title_change(
            &self,
            browser: Option<&mut Browser>,
            title: Option<&CefString>,
        ) {
            let (Some(browser), Some(title)) = (browser, title) else {
                return;
            };
            let id = browser.identifier();
            let text = title.to_string();
            if text.is_empty() {
                return;
            }
            let mut titles = TITLES.lock().unwrap_or_else(|e| e.into_inner());
            match titles.iter_mut().rev().find(|(slot_id, _)| *slot_id == id) {
                Some(entry) => entry.1 = text,
                None => titles.push((id, text)),
            }
        }

        fn on_console_message(
            &self,
            browser: Option<&mut Browser>,
            _level: LogSeverity,
            message: Option<&CefString>,
            _source: Option<&CefString>,
            _line: std::os::raw::c_int,
        ) -> std::os::raw::c_int {
            let Some(message) = message else { return 0 };
            let text = message.to_string();
            let id = browser.as_ref().map(|b| b.identifier());
            if std::env::var("OMD_BROWSER_DEBUG").is_ok() && !text.is_empty() {
                eprintln!(
                    "[console] id={id:?} {}",
                    text.chars().take(80).collect::<String>()
                );
            }
            if text == "[omd-ready]" {
                if let Some(id) = id {
                    let mut ready = ANNOTATOR_READY.lock().unwrap_or_else(|e| e.into_inner());
                    if !ready.contains(&id) {
                        ready.push(id);
                    }
                }
            }
            if let Some(payload) = text.strip_prefix("[omd-pins]") {
                if let Some(id) = id {
                    PIN_QUEUE
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((id, payload.to_string()));
                }
            }
            0
        }
    }
}

cef::wrap_render_handler! {
    struct OmdRender {}
    impl RenderHandler {
        fn view_rect(
            &self,
            browser: Option<&mut Browser>,
            rect: Option<&mut Rect>,
        ) {
            let Some(rect) = rect else { return };
            let id = browser.map(|b| b.identifier());
            let slots = slots();
            let slot = id
                .and_then(|id| slots.iter().find(|s| s.id == Some(id)))
                .or_else(|| slots.last());
            let (w, h) = slot.map(|s| s.view).unwrap_or((1280, 800));
            if std::env::var("OMD_BROWSER_DEBUG").is_ok() {
                static LAST: Mutex<(i32, i32)> = Mutex::new((0, 0));
                static N: AtomicU64 = AtomicU64::new(0);
                let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
                if *last != (w, h) || N.fetch_add(1, Ordering::Relaxed) < 3 {
                    *last = (w, h);
                    eprintln!("[view_rect] id={:?} asked {w}x{h}", id);
                }
            }
            rect.width = w;
            rect.height = h;
        }

        fn on_accelerated_paint(
            &self,
            browser: Option<&mut Browser>,
            _type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed) + 1;
            let debug = std::env::var("OMD_BROWSER_DEBUG").is_ok();
            #[cfg(target_os = "linux")]
            if let Some((device, queue)) = gpu_probe() {
                match unsafe {
                    cef::osr_texture_import::SharedTextureHandle::new(info)
                        .import_texture(&device, &queue)
                } {
                    Ok(texture) => {
                        let (width, height) = (texture.width(), texture.height());
                        // 每次导入都是一张新的自有纹理。只在尺寸变化时登记的话，
                        // 画面会停在第一帧（通常还是空白）。
                        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                        let seq = FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
                        let id = browser.map(|b| b.identifier());
                        let mut slots = slots();
                        let (key, slot) = match id.and_then(|id| {
                            slots.iter_mut().enumerate().find(|(_, s)| s.id == Some(id))
                        }) {
                            Some((index, slot)) => (index as u32, Some(slot)),
                            None => match slots.len().checked_sub(1) {
                                Some(last) => (last as u32, Some(&mut slots[last])),
                                None => (0, None),
                            },
                        };
                        gpui_wgpu::register_external_texture(key, view);
                        if let Some(slot) = slot {
                            slot.accel = Some(AccelFrame { width, height, seq });
                            if n <= 3 && std::env::var("OMD_BROWSER_DEBUG").is_ok() {
                                eprintln!("[accel] slot updated {width}x{height} seq={seq}");
                            }
                        } else if n <= 3 && std::env::var("OMD_BROWSER_DEBUG").is_ok() {
                            eprintln!("[accel] no slot for id={id:?}");
                        }
                        drop(slots);
                        if std::env::var("OMD_BROWSER_DUMP").is_ok() && n % 30 == 0 {
                            dump_texture(&device, &queue, &texture, "/tmp/cef-accel-last.png");
                        }
                        if n <= 3 {
                            eprintln!("[accel] imported {width}x{height} {:?}", texture.format());
                        }
                    }
                    Err(e) => {
                        if n <= 3 {
                            eprintln!("[accel] import 失败: {e}");
                        }
                    }
                }
            }
            if !debug || (n > 3 && n % 30 != 0) {
                return;
            }
            // plane_count/planes/modifier 是 Linux DMA-BUF 路径的扩展字段。
            #[cfg(target_os = "linux")]
            {
                let count = info.plane_count.max(0) as usize;
                let planes: Vec<(i32, u32, u64)> = info.planes[..count.min(4)]
                    .iter()
                    .map(|p| (p.fd, p.stride, p.size))
                    .collect();
                eprintln!(
                    "[accel] n={n} planes={planes:?} fmt={:?} modifier=0x{:x} coded={:?} dirty={:?}",
                    info.format,
                    info.modifier,
                    info.extra.coded_size,
                    dirty_rects.map(|d| d.len())
                );
            }
        }

        fn on_paint(
            &self,
            browser: Option<&mut Browser>,
            _type_: PaintElementType,
            dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            if width <= 0 || height <= 0 || buffer.is_null() || SHARED_TEXTURE.load(Ordering::Relaxed)
            {
                return;
            }
            if std::env::var("OMD_BROWSER_DEBUG").is_ok() {
                static N: AtomicU64 = AtomicU64::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                if n <= 3 || n % 300 == 0 {
                    eprintln!(
                        "[paint] n={n} id={:?} {width}x{height}",
                        browser.as_ref().map(|b| b.identifier())
                    );
                }
            }
            let Some((device, queue)) = gpui_wgpu::shared_gpu() else { return };
            let (width, height) = (width as u32, height as u32);
            let id = browser.map(|b| b.identifier());
            let mut slots = slots();
            let (key, slot) = match id.and_then(|id| {
                slots.iter_mut().enumerate().find(|(_, s)| s.id == Some(id))
            }) {
                Some((index, slot)) => (index as u32, slot),
                None => match slots.len().checked_sub(1) {
                    Some(last) => (last as u32, &mut slots[last]),
                    None => return,
                },
            };
            // 纹理只在尺寸变化时重建。直接上传 CEF 借用的 BGRA 缓冲，不再复制一份 Vec。
            if slot.upload_texture.as_ref().is_none_or(|texture| {
                texture.width() != width || texture.height() != height
            }) {
                let texture = device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("browser OSR pixels"),
                    size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                    view_formats: &[],
                });
                gpui_wgpu::register_external_texture(
                    key,
                    texture.create_view(&wgpu::TextureViewDescriptor::default()),
                );
                slot.upload_texture = Some(texture);
            }
            // SAFETY: CEF 保证回调内 buffer 包含 width * height 个紧密排列的 BGRA 像素。
            // write_texture 在返回前复制数据；不保留 CEF 的缓冲指针。
            let pixels = unsafe {
                std::slice::from_raw_parts(buffer, width as usize * height as usize * 4)
            };
            let texture = slot.upload_texture.as_ref().unwrap();
            upload_frame(
                &queue,
                texture,
                pixels,
                width,
                height,
                dirty_rects,
                &mut slot.staging,
            );
            for (pin_id, cx0, cy0, cw, ch) in std::mem::take(&mut slot.capture_req) {
                if let Some(path) = crop_png(
                    pixels, width, height, cx0, cy0, cw, ch, key as usize, pin_id,
                ) {
                    slot.captured.push((pin_id, path));
                }
            }
            if std::env::var("OMD_BROWSER_DUMP").is_ok() && cfg!(target_os = "linux") {
                static D: AtomicU64 = AtomicU64::new(0);
                if D.fetch_add(1, Ordering::Relaxed) % 30 == 0 {
                    #[cfg(target_os = "linux")]
                    dump_texture(&device, &queue, &texture, "/tmp/cef-osr-last.png");
                }
            }
            slot.accel = Some(AccelFrame {
                width,
                height,
                seq: FRAME_SEQ.fetch_add(1, Ordering::Relaxed),
            });
        }
    }
}
