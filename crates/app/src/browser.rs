//! 浏览器块：CEF 以 windowless（OSR）方式渲染，帧经 `on_paint` 交给 GPUI，
//! 同时开远程调试端口给 agent。
//!
//! 为什么不用子窗口：GPUI 的 X11 窗口是 32 位深 visual，CEF 的子窗口无法
//! repaint/reparent 上去（实测创建即失败）；Wayland 下也没有子窗口协议。
//! OSR 帧流三端一致，CDP 控制路径完全相同。
//!
//! 这里只管：CEF 进程生命周期、离屏 browser、帧的存取、CDP 端口。
//! 页面内的标注层由注入的 JS 提供；可信输入走 BrowserHost::send_*（比
//! CDP Input 还近一层）。

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use cef::*;

/// 一帧页面内容（BGRA，自顶向下）。
#[derive(Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
    pub seq: u64,
}
#[derive(Default)]
/// 一帧 GPU 纹理（已注册到 gpui-wgpu 的外部纹理表）。
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
    frame: Option<std::sync::Arc<Frame>>,
    accel: Option<AccelFrame>,
    /// 是否走 windowless（OSR）渲染。子窗口模式由 CEF 自己跟 vsync，不能再推 begin frame。
    windowless: bool,
    /// 子窗口模式的 X11 宿主窗口（32 位 GPUI 窗口和 24 位 CEF 子窗口之间的桥）。
    host: Option<u32>,
}


/// 每个 browser 一份；index 对应 UI 侧的 browser pane。
static SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());
static FRAME_SEQ: AtomicU64 = AtomicU64::new(0);
/// 当前进程是否 windowless。Chromium 的 Ozone 平台开关据此决定。
static WINDOWLESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// 外部纹理在 gpui-wgpu 注册表里的 key（单浏览器固定 0）。
const EXTERNAL_KEY: u32 = 0;

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
    true
}

/// 只有 GPUI 的 device 支持 DMA-BUF 导入时才走 GPU 路径，否则保持软件渲染。
fn accel_available() -> bool {
    std::env::var("OMD_BROWSER_ACCEL").is_ok()
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
    let slot = &mut slots()[index];
    slot.id = Some(id);
    slot.browser = Some(browser);
    slot.host = Some(host);
    Some(index)
}

/// 子窗口跟随 GPUI 布局：移动/缩放宿主，再把同样的尺寸补给 CEF 的窗口，
/// 让它收到 ConfigureNotify（CefWindowX11 靠这个 resize 浏览器 widget）。
pub fn child_bounds(index: usize, x: i32, y: i32, width: i32, height: i32) {
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
        crate::x11_embed::detach(host);
    }
    let _ = host;
}

pub fn create(width: u32, height: u32, url: &str) -> Option<usize> {
    let mut window_info = WindowInfo::default();
    window_info.runtime_style = RuntimeStyle::ALLOY;
    // windowless 也需要一个父窗口句柄给弹窗/显示器定位；0 = 无。
    let mut window_info = window_info.set_as_windowless(0);
    // OSR 由我们推 begin frame（外部时钟），否则 CEF 内部定时器只在部分场景出帧。
    window_info.external_begin_frame_enabled = 1;
    // GPU 合成路径（DMA-BUF 共享纹理）已验证可用，但还没接 wgpu 导入，
    // 打开会没有 on_paint 也就没有画面，所以默认关，用 OMD_BROWSER_ACCEL=1 探测。
    window_info.shared_texture_enabled = if accel_available() { 1 } else { 0 };
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
    let host = {
        let mut slots = slots();
        let Some(slot) = slots.get_mut(index) else { return };
        slot.view = (width as i32, height as i32);
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

/// 取最新一帧。Arc 只加引用计数，UI 每帧调用也不会拷贝像素。
pub fn latest_frame(index: usize) -> Option<std::sync::Arc<Frame>> {
    slots().get(index).and_then(|slot| slot.frame.clone())
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
fn glib_dispatch() {
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
        return;
    };
    unsafe {
        // 上限防止 pending 一直为真时卡死 UI 线程。
        for _ in 0..256 {
            if (glib.pending)(glib.context) == 0 {
                break;
            }
            (glib.iteration)(glib.context, 0);
        }
    }
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
    if due {
        let hosts: Vec<_> = slots()
            .iter()
            .filter(|s| s.windowless)
            .filter_map(|s| s.browser.as_ref().and_then(|b| b.host()))
            .collect();
        for host in hosts {
            host.send_external_begin_frame();
        }
    }
    glib_dispatch();
    do_message_loop_work();
    glib_dispatch();
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
            if !WINDOWLESS.load(Ordering::Relaxed) {
                /*
                子窗口模式必须把 Chromium 的 Ozone 也钉在 X11：父窗口是 X11 的，
                它自己 auto 探测到 Wayland 就再也挂不上去。CEF 会把这个开关传给
                所有子进程，所以只需要在这里加一次。
                */
                cmd.append_switch_with_value(
                    Some(&CefString::from("ozone-platform")),
                    Some(&CefString::from("x11")),
                );
            }
            // 少留一个空闲 renderer（Chromium 会提前起一个备用进程，约 100MB）。
            cmd.append_switch_with_value(
                Some(&CefString::from("disable-features")),
                Some(&CefString::from("SpareRendererForSitePerProcess")),
            );
        }
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
                        // 池化的目标纹理尺寸固定，view 只在尺寸变化时重建。
                        {
                            static VIEWS: std::sync::Mutex<
                                Vec<((u32, u32), wgpu::TextureView)>,
                            > = std::sync::Mutex::new(Vec::new());
                            let mut views = VIEWS.lock().unwrap_or_else(|e| e.into_inner());
                            if !views.iter().any(|(k, _)| *k == (width, height)) {
                                let view = texture
                                    .create_view(&wgpu::TextureViewDescriptor::default());
                                gpui_wgpu::register_external_texture(EXTERNAL_KEY, view.clone());
                                views.push(((width, height), view));
                            }
                        }
                        let seq = FRAME_SEQ.fetch_add(1, Ordering::Relaxed);
                        let id = browser.map(|b| b.identifier());
                        let mut slots = slots();
                        let slot = match id.and_then(|id| slots.iter_mut().find(|s| s.id == Some(id))) {
                            Some(slot) => Some(slot),
                            None => slots.last_mut(),
                        };
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

        fn on_paint(
            &self,
            browser: Option<&mut Browser>,
            _type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            if width <= 0 || height <= 0 || buffer.is_null() {
                return;
            }
            if std::env::var("OMD_BROWSER_DEBUG").is_ok() {
                static N: AtomicU64 = AtomicU64::new(0);
                let n = N.fetch_add(1, Ordering::Relaxed) + 1;
                if n % 30 == 1 {
                    eprintln!("[paint] n={n} {width}x{height} dirty={:?}", _dirty_rects.map(|d| d.len()));
                }
            }
            let len = (width as usize) * (height as usize) * 4;
            // CEF 的缓冲在这行返回后就会复用，必须拷走。
            let bgra = unsafe { std::slice::from_raw_parts(buffer, len) }.to_vec();
            let frame = Frame {
                width: width as u32,
                height: height as u32,
                bgra,
                seq: FRAME_SEQ.fetch_add(1, Ordering::Relaxed),
            };
            let id = browser.map(|b| b.identifier());
            let mut slots = slots();
            let slot = match id.and_then(|id| slots.iter_mut().find(|s| s.id == Some(id))) {
                Some(slot) => slot,
                None => match slots.last_mut() {
                    Some(slot) => slot,
                    None => return,
                },
            };
            slot.frame = Some(std::sync::Arc::new(frame));
        }
    }
}
