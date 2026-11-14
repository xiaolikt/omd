//! X11 嵌入宿主窗口。
//!
//! CEF 在 Linux 上只能把 browser 子窗口挂到一个 X11 窗口上，而 GPUI 主窗口是
//! 32 位 ARGB visual。直接把主窗口 id 交给 CEF 时，Chromium 在自己的连接上
//! CreateWindow 会 BadMatch/BadDrawable（child 的 depth/visual 跟 parent 不匹配，
//! 也没有 colormap）。
//!
//! 所以在中间插一层：一个和屏幕根窗口同 depth/visual 的宿主窗口，父是 GPUI
//! 窗口；CEF 建在宿主里并铺满它。宿主负责位置、尺寸、显隐，CEF 自己的窗口
//! 始终保持 (0,0,size)，这样 CefWindowX11 收到的 ConfigureNotify 就是 resize。

use std::sync::{Mutex, OnceLock};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ColormapAlloc, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, InputFocus,
    Rectangle, StackMode, Window, WindowClass,
};
use x11rb::protocol::xinput as x11;
use x11rb::protocol::xinput::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

struct X11 {
    conn: RustConnection,
    root: Window,
    root_depth: u8,
    root_visual: u32,
    colormap: u32,
}

fn x11() -> Option<&'static Mutex<X11>> {
    static X11: OnceLock<Option<Mutex<X11>>> = OnceLock::new();
    X11.get_or_init(|| {
        let (conn, screen_index) = x11rb::connect(None).ok()?;
        let screen = &conn.setup().roots[screen_index];
        let root = screen.root;
        let root_depth = screen.root_depth;
        let root_visual = screen.root_visual;
        let colormap = conn.generate_id().ok()?;
        conn.create_colormap(ColormapAlloc::NONE, colormap, root, root_visual)
            .ok()?;
        let _ = conn.flush();
        Some(Mutex::new(X11 {
            conn,
            root,
            root_depth,
            root_visual,
            colormap,
        }))
    })
    .as_ref()
}

/// 在 `parent`（GPUI 的 X11 窗口）里建一个宿主窗口，返回窗口 id。
pub fn create_host(parent: u32, x: i16, y: i16, width: u16, height: u16) -> Option<u32> {
    let lock = x11()?;
    let x11 = lock.lock().ok()?;
    let host = x11.conn.generate_id().ok()?;
    let aux = CreateWindowAux::new()
        .background_pixel(0)
        .border_pixel(0)
        .colormap(x11.colormap);
    x11.conn
        .create_window(
            x11.root_depth,
            host,
            parent,
            x,
            y,
            width.max(1),
            height.max(1),
            0,
            WindowClass::INPUT_OUTPUT,
            x11.root_visual,
            &aux,
        )
        .ok()?;
    let _ = x11.conn.map_window(host);
    // CEF 在另一条 X 连接上建子窗口：只 flush 不够，服务器可能先处理它的
    // CreateWindow 从而 BadWindow（XWayland 上实测浏览器拿不到窗口）。
    // 一次往返保证宿主已经在服务器端存在。
    if let Ok(cookie) = x11.conn.get_input_focus() {
        let _ = cookie.reply();
    }
    let _ = x11.conn.flush();
    Some(host)
}

pub fn set_bounds(host: u32, x: i16, y: i16, width: u16, height: u16) {
    let Some(lock) = x11() else { return };
    let Ok(x11) = lock.lock() else { return };
    let aux = ConfigureWindowAux::new()
        .x(i32::from(x))
        .y(i32::from(y))
        .width(u32::from(width.max(1)))
        .height(u32::from(height.max(1)));
    let _ = x11.conn.configure_window(host, &aux);
    let _ = x11.conn.flush();
}

/// CEF 的子窗口跟随宿主尺寸；宿主移动后要把它的 ConfigureNotify 也补上。
pub fn set_child_bounds(cef_window: u32, width: u16, height: u16) {
    let Some(lock) = x11() else { return };
    let Ok(x11) = lock.lock() else { return };
    let aux = ConfigureWindowAux::new()
        .x(0)
        .y(0)
        .width(u32::from(width.max(1)))
        .height(u32::from(height.max(1)));
    let _ = x11.conn.configure_window(cef_window, &aux);
    let _ = x11.conn.flush();
}

pub fn set_visible(host: u32, visible: bool) {
    let Some(lock) = x11() else { return };
    let Ok(x11) = lock.lock() else { return };
    if visible {
        let _ = x11.conn.map_window(host);
        let _ = x11.conn.configure_window(
            host,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        );
    } else {
        let _ = x11.conn.unmap_window(host);
    }
    let _ = x11.conn.flush();
}

pub fn focus(cef_window: u32) {
    let Some(lock) = x11() else { return };
    let Ok(x11) = lock.lock() else { return };
    let _ = x11
        .conn
        .set_input_focus(InputFocus::PARENT, cef_window, x11rb::CURRENT_TIME);
    let _ = x11.conn.flush();
}

/// 销毁宿主前先摘出去：Chromium 的 GPU 表面还挂在它上面，直接销毁会崩。
pub fn detach(host: u32) {
    let Some(lock) = x11() else { return };
    let Ok(x11) = lock.lock() else { return };
    let _ = x11.conn.unmap_window(host);
    let _ = x11.conn.reparent_window(host, x11.root, 0, 0);
    if let Ok(cookie) = x11.conn.get_input_focus() {
        let _ = cookie.reply();
    }
    let _ = x11.conn.flush();
}

pub fn destroy(host: u32) {
    let Some(lock) = x11() else { return };
    let Ok(x11) = lock.lock() else { return };
    let _ = x11.conn.destroy_window(host);
    let _ = x11.conn.flush();
}

// ---------------------------------------------------------------------------
// 点击 → X 输入焦点
// ---------------------------------------------------------------------------

/// 一个嵌入的浏览器：点击落在它的矩形里就把 X 焦点交给 CEF 的子窗口，
/// 否则交回 GPUI 的主窗口。X11 不会像 Wayland 那样替我们做这件事，而
/// Chromium 只有收到 X 焦点才会把键盘事件送进页面。
struct Target {
    host: Window,
    cef: Window,
    parent: Window,
}

static TARGETS: Mutex<Vec<Target>> = Mutex::new(Vec::new());

/// 登记一个嵌入的浏览器，并确保点击观察线程已经起来。
pub fn watch(host: u32, cef_window: u32, parent: u32) {
    if let Ok(mut targets) = TARGETS.lock() {
        targets.retain(|t| t.host != host);
        targets.push(Target {
            host,
            cef: cef_window,
            parent,
        });
    }
    start_click_watcher();
}

pub fn unwatch(host: u32) {
    if let Ok(mut targets) = TARGETS.lock() {
        targets.retain(|t| t.host != host);
    }
}

fn start_click_watcher() {
    static STARTED: OnceLock<()> = OnceLock::new();
    STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("x11-click-watcher".into())
            .spawn(|| {
                let Ok((conn, screen_index)) = x11rb::connect(None) else {
                    return;
                };
                let root = conn.setup().roots[screen_index].root;
                // XI2 raw 事件在根窗口上选，子窗口里的点击也能看到。
                let _ = conn.xinput_xi_query_version(2, 4);
                let masks = [x11::EventMask {
                    deviceid: 0, // XIAllDevices
                    mask: vec![x11::XIEventMask::RAW_BUTTON_PRESS],
                }];
                if conn.xinput_xi_select_events(root, &masks).is_err() {
                    eprintln!("[browser] XInput2 raw 事件不可用，点击不会切换焦点");
                    return;
                }
                let _ = conn.flush();
                loop {
                    let Ok(event) = conn.wait_for_event() else { break };
                    // raw 事件本身不带坐标，按下瞬间的指针位置就是点击点。
                    let x11rb::protocol::Event::XinputRawButtonPress(_press) = event else {
                        continue;
                    };
                    let Ok(cookie) = conn.query_pointer(root) else { continue };
                    let Ok(pointer) = cookie.reply() else { continue };
                    let (x, y) = (pointer.root_x, pointer.root_y);
                    let focus = {
                        let targets = match TARGETS.lock() {
                            Ok(t) => t,
                            Err(_) => continue,
                        };
                        let hit = targets.iter().find(|t| {
                            screen_rect(&conn, root, t.host)
                                .is_some_and(|(rx, ry, rw, rh)| {
                                    x >= rx && x < rx + rw as i16 && y >= ry && y < ry + rh as i16
                                })
                        });
                        hit.map(|t| t.cef)
                            .or_else(|| targets.first().map(|t| t.parent))
                    };
                    if let Some(window) = focus {
                        let _ = conn.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME);
                        let _ = conn.flush();
                    }
                }
            })
            .ok();
    });
}

/// 窗口在根坐标里的矩形。
fn screen_rect(conn: &RustConnection, root: Window, window: Window) -> Option<(i16, i16, u16, u16)> {
    let geometry = conn.get_geometry(window).ok()?.reply().ok()?;
    let translated = conn
        .translate_coordinates(window, root, 0, 0)
        .ok()?
        .reply()
        .ok()?;
    Some((translated.dst_x, translated.dst_y, geometry.width, geometry.height))
}

/// 宿主窗口在屏幕上的矩形，用于把 GPUI 的鼠标坐标换算成页面坐标。
pub fn geometry(host: u32) -> Option<Rectangle> {
    let lock = x11()?;
    let x11 = lock.lock().ok()?;
    let cookie = x11.conn.get_geometry(host).ok()?;
    let geometry = cookie.reply().ok()?;
    Some(Rectangle {
        x: geometry.x,
        y: geometry.y,
        width: geometry.width,
        height: geometry.height,
    })
}
