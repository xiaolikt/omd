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
