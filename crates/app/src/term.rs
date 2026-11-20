//! Terminal block：libghostty-vt 包装。
//!
//! feed() 进 VT 字节；render_rows() 出视口（每行 = 样式化 span 列表）。
//! 颜色：RGB 直接用；palette 索引用标准 xterm 256 色算法展开；
//! NONE 用默认前景/背景。Kitty 图形走 render-state 的 image iterator，
//! 像素缓存在 `PlacedImage` 里，按 (image id, generation) 复用。

use crate::ghostty_vt::*;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::Arc;

use gpui_kit::RenderImage;

/// 和 `term_rows` 的 13px monospace 对齐的格子尺寸。ghostty 的放置像素也用它。
pub const CELL_W: u32 = 8;
pub const CELL_H: u32 = 16;

/// 一个要画在视口上的 kitty 图片。坐标是格子 + 格内像素偏移。
#[derive(Clone)]
pub struct PlacedImage {
    pub col: i32,
    pub row: i32,
    pub ox: u32,
    pub oy: u32,
    pub w: u32,
    pub h: u32,
    pub image: Arc<RenderImage>,
}

/// 一个渲染 span：同色连续字符段。
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    /// None = 默认前景
    pub fg: Option<u32>,
    /// None = 默认背景
    pub bg: Option<u32>,
    pub bold: bool,
    /// 这个 span 是光标格（渲染时强制反色，且不参与相邻合并）。
    pub cursor: bool,
}

pub struct Term {
    terminal: GhosttyTerminal,
    state: GhosttyRenderState,
    cells: GhosttyRenderStateRowCells,
    cols: u16,
    rows: u16,
    cursor: Option<(u16, u16)>,
    /// 上一帧的完整视口。render state 只给出脏行，清掉的行必须留在这里。
    grid: Vec<Vec<Span>>,
    images_iter: GhosttyRenderStateImageIterator,
    image_cache: HashMap<(u32, u64), Arc<RenderImage>>,
    placed: Vec<PlacedImage>,
    /// 回滚搜索（懒创建）
    search: GhosttySearch,
    /// 选区（视口坐标的锚点 + 当前端点；空 = 无选区）
    sel: Option<((u16, u32), (u16, u32))>,
    /// 选区变了：下一帧整屏重绘（vt 的脏行机制不感知我们的选区）
    repaint_all: bool,
}

unsafe impl Send for Term {}

impl Term {
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        unsafe {
            let mut terminal: GhosttyTerminal = std::ptr::null_mut();
            if ghostty_terminal_new(std::ptr::null(), &mut terminal, cols, rows) != GHOSTTY_SUCCESS {
                return None;
            }
            let mut state: GhosttyRenderState = std::ptr::null_mut();
            if ghostty_render_state_new(std::ptr::null(), &mut state) != GHOSTTY_SUCCESS {
                ghostty_terminal_free(terminal);
                return None;
            }
            let mut cells: GhosttyRenderStateRowCells = std::ptr::null_mut();
            if ghostty_render_state_row_cells_new(std::ptr::null(), &mut cells) != GHOSTTY_SUCCESS {
                ghostty_render_state_free(state);
                ghostty_terminal_free(terminal);
                return None;
            }
            let mut images_iter: GhosttyRenderStateImageIterator = std::ptr::null_mut();
            if ghostty_render_state_image_iterator_new(std::ptr::null(), &mut images_iter)
                != GHOSTTY_SUCCESS
            {
                ghostty_render_state_row_cells_free(cells);
                ghostty_render_state_free(state);
                ghostty_terminal_free(terminal);
                return None;
            }
            ghostty_terminal_resize(terminal, cols, rows, CELL_W, CELL_H);
            Some(Self {
                terminal,
                state,
                cells,
                cols,
                rows,
                cursor: None,
                grid: vec![Vec::new(); rows as usize],
                images_iter,
                image_cache: HashMap::new(),
                placed: Vec::new(),
                search: std::ptr::null_mut(),
                sel: None,
                repaint_all: false,
            })
        }
    }

    pub fn feed(&mut self, data: &str) {
        unsafe {
            ghostty_terminal_vt_write(self.terminal, data.as_ptr(), data.len());
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cw: u32, ch: u32) {
        unsafe {
            let cw = if cw == 0 { CELL_W } else { cw };
            let ch = if ch == 0 { CELL_H } else { ch };
            ghostty_terminal_resize(self.terminal, cols, rows, cw, ch);
            self.cols = cols;
            self.rows = rows;
            self.grid.resize(rows as usize, Vec::new());
        }
    }

    pub fn dims(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// Encode a key using the terminal's current modes (application cursor
    /// keys, Kitty keyboard protocol, modifyOtherKeys, ...).
    pub fn encode_key(&self, key: i32, mods: u16, text: &str, unshifted: u32) -> Vec<u8> {
        unsafe {
            let mut enc: GhosttyKeyEncoder = std::ptr::null_mut();
            let mut ev: GhosttyKeyEvent = std::ptr::null_mut();
            if ghostty_key_encoder_new(std::ptr::null(), &mut enc) != GHOSTTY_SUCCESS {
                return Vec::new();
            }
            if ghostty_key_event_new(std::ptr::null(), &mut ev) != GHOSTTY_SUCCESS {
                ghostty_key_encoder_free(enc);
                return Vec::new();
            }
            ghostty_key_encoder_set_from_terminal(enc, self.terminal);
            let option_as_alt: i32 = 1;
            ghostty_key_encoder_set(enc, 7, &option_as_alt as *const _ as *const c_void);
            ghostty_key_event_set_action(ev, 1);
            ghostty_key_event_set_key(ev, key);
            ghostty_key_event_set_mods(ev, mods);
            ghostty_key_event_set_utf8(
                ev,
                GhosttyString {
                    ptr: text.as_ptr(),
                    len: text.len(),
                },
            );
            ghostty_key_event_set_unshifted_codepoint(ev, unshifted);
            let mut buf = [0u8; 256];
            let mut len = 0usize;
            let rc = ghostty_key_encoder_encode(enc, ev, buf.as_mut_ptr(), buf.len(), &mut len);
            ghostty_key_event_free(ev);
            ghostty_key_encoder_free(enc);
            if rc == GHOSTTY_SUCCESS && len <= buf.len() {
                buf[..len].to_vec()
            } else {
                Vec::new()
            }
        }
    }

    /// 当前视口：每行一个 span 列表。只刷新脏行，其余行沿用上一帧。
    pub fn render_rows(&mut self) -> Vec<Vec<Span>> {
        unsafe {
            if self.grid.len() != self.rows as usize {
                self.grid.resize(self.rows as usize, Vec::new());
            }
            ghostty_render_state_update(self.state, self.terminal);
            // 选区变化要整屏重绘（在 update 之后设置才不会被覆盖）
            if self.repaint_all {
                self.repaint_all = false;
                let full = RS_DIRTY_FULL;
                let _ = ghostty_render_state_set(
                    self.state,
                    RS_OPT_DIRTY,
                    &full as *const i32 as *const c_void,
                );
            }

            let mut visible = false;
            let _ = ghostty_render_state_get(
                self.state,
                RS_DATA_CURSOR_VISIBLE,
                &mut visible as *mut _ as *mut c_void,
            );
            let mut cx: u16 = 0;
            let mut cy: u16 = 0;
            let has_x = ghostty_render_state_get(
                self.state,
                RS_DATA_CURSOR_VIEWPORT_X,
                &mut cx as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS;
            let has_y = ghostty_render_state_get(
                self.state,
                RS_DATA_CURSOR_VIEWPORT_Y,
                &mut cy as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS;
            self.cursor = if visible && has_x && has_y && cx < self.cols && cy < self.rows {
                Some((cx, cy))
            } else {
                None
            };

            let mut iter: GhosttyRenderStateRowIterator = std::ptr::null_mut();
            if ghostty_render_state_row_iterator_new(std::ptr::null(), &mut iter) != GHOSTTY_SUCCESS {
                return self.grid.clone();
            }
            if ghostty_render_state_get(
                self.state,
                RS_DATA_ROW_ITERATOR,
                &mut iter as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                ghostty_render_state_row_iterator_free(iter);
                return self.grid.clone();
            }

            let mut y: u16 = 0;
            while ghostty_render_state_row_iterator_next_dirty(iter, &mut y) {
                if y as usize >= self.grid.len() {
                    continue;
                }
                if ghostty_render_state_row_get(
                    iter,
                    RS_ROW_DATA_CELLS,
                    &mut self.cells as *mut _ as *mut c_void,
                ) == GHOSTTY_SUCCESS
                {
                    self.grid[y as usize] = self.render_row(y);
                }
            }
            ghostty_render_state_row_iterator_free(iter);
            self.collect_images();
            ghostty_render_state_clean(self.state);
            self.grid.clone()
        }
    }

    pub fn images(&self) -> &[PlacedImage] {
        &self.placed
    }

    /// 设置搜索词（空 = 清除）。不跑搜索，等 next/prev 或 run。
    pub fn search_set_needle(&mut self, needle: &str) {
        if self.search.is_null() {
            unsafe {
                let mut search: GhosttySearch = std::ptr::null_mut();
                if ghostty_search_new(std::ptr::null(), &mut search, self.terminal)
                    != GHOSTTY_SUCCESS
                {
                    return;
                }
                self.search = search;
            }
        }
        let value = if needle.is_empty() {
            GhosttyString {
                ptr: std::ptr::null(),
                len: 0,
            }
        } else {
            GhosttyString {
                ptr: needle.as_ptr(),
                len: needle.len(),
            }
        };
        unsafe {
            ghostty_search_set(self.search, SEARCH_OPT_NEEDLE, &value as *const _ as *const c_void);
        }
    }

    /// 跑完当前搜索（阻塞式；滚动区大时会慢，但我们的量级可接受）。
    pub fn search_run(&mut self) {
        if !self.search.is_null() {
            unsafe { ghostty_search_run(self.search) };
        }
    }

    /// 跳到下一个（更旧）匹配；会滚动视口。返回 (选中序号, 总数)，没有匹配返回 None。
    pub fn search_select(&mut self, forward: bool) -> Option<(usize, usize)> {
        if self.search.is_null() {
            return None;
        }
        unsafe {
            let rc = if forward {
                ghostty_search_select_next(self.search)
            } else {
                ghostty_search_select_prev(self.search)
            };
            if rc != GHOSTTY_SUCCESS {
                return None;
            }
            let mut total = 0usize;
            let _ = ghostty_search_get(
                self.search,
                SEARCH_DATA_TOTAL_MATCHES,
                &mut total as *mut _ as *mut c_void,
            );
            let mut index = 0usize;
            let got = ghostty_search_get(
                self.search,
                SEARCH_DATA_SELECTED_INDEX,
                &mut index as *mut _ as *mut c_void,
            );
            if got == GHOSTTY_SUCCESS {
                Some((index, total))
            } else {
                Some((0, total))
            }
        }
    }

    /// 当前搜索计数（不移动选择）。
    pub fn search_counts(&mut self) -> (usize, Option<usize>) {
        if self.search.is_null() {
            return (0, None);
        }
        unsafe {
            let mut total = 0usize;
            let _ = ghostty_search_get(
                self.search,
                SEARCH_DATA_TOTAL_MATCHES,
                &mut total as *mut _ as *mut c_void,
            );
            let mut index = 0usize;
            let got = ghostty_search_get(
                self.search,
                SEARCH_DATA_SELECTED_INDEX,
                &mut index as *mut _ as *mut c_void,
            );
            (total, if got == GHOSTTY_SUCCESS { Some(index) } else { None })
        }
    }

    /// 选中匹配的文本行区间转成视口坐标（用于高亮）。v1：靠视口滚动 + 行级提示即可。
    pub fn search_clear(&mut self) {
        if !self.search.is_null() {
            self.search_set_needle("");
        }
    }

    /// 滚动视口：正 = 向新内容，负 = 向回滚。
    pub fn scroll_rows(&mut self, delta: isize) {
        unsafe {
            ghostty_terminal_scroll_viewport(
                self.terminal,
                GhosttyScrollViewport {
                    tag: SCROLL_VIEWPORT_DELTA,
                    value: GhosttyScrollViewportValue { delta },
                },
            );
        }
    }

    /// 视口回到底部（活动区）。
    pub fn scroll_to_bottom(&mut self) {
        unsafe {
            ghostty_terminal_scroll_viewport(
                self.terminal,
                GhosttyScrollViewport {
                    tag: SCROLL_VIEWPORT_BOTTOM,
                    value: GhosttyScrollViewportValue { delta: 0 },
                },
            );
        }
    }

    /// 当前是否开了 bracketed paste（mode 2004）。
    pub fn bracketed_paste(&self) -> bool {
        unsafe {
            let mut config = GhosttyModeConfig {
                mode: MODE_BRACKETED_PASTE,
                value: false,
            };
            if ghostty_terminal_get(
                self.terminal,
                TERM_DATA_MODE,
                &mut config as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS
            {
                config.value
            } else {
                false
            }
        }
    }

    /// 文本粘贴：按当前 bracketed 模式编码成要写进 PTY 的字节。
    pub fn paste_text(&self, text: &str) -> Vec<u8> {
        if text.is_empty() {
            return Vec::new();
        }
        let bracketed = self.bracketed_paste();
        let mut data = text.as_bytes().to_vec();
        unsafe {
            // 两次调用：先问需要多大
            let mut needed = 0usize;
            let rc = ghostty_paste_encode(
                data.as_mut_ptr(),
                data.len(),
                bracketed,
                std::ptr::null_mut(),
                0,
                &mut needed,
            );
            let cap = if rc == 0 {
                needed
            } else {
                // 没给尺寸就按 2 倍 + 32 估（含括号前后缀）
                data.len() * 2 + 32
            };
            let mut buf = vec![0u8; cap + 1];
            let mut written = 0usize;
            let rc = ghostty_paste_encode(
                data.as_mut_ptr(),
                data.len(),
                bracketed,
                buf.as_mut_ptr(),
                buf.len(),
                &mut written,
            );
            if rc == 0 {
                buf.truncate(written);
                buf
            } else {
                // 兜底：原样发
                text.as_bytes().to_vec()
            }
        }
    }

    /// 图片粘贴：RGBA 像素走 kitty 图形协议喂给本地 VT（不进 PTY）。
    pub fn paste_image_rgba(&mut self, width: u32, height: u32, rgba: &[u8]) {
        use base64::Engine as _;
        if width == 0 || height == 0 || rgba.len() < (width as usize) * (height as usize) * 4 {
            return;
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(rgba);
        const CHUNK: usize = 4096;
        let total = b64.len();
        let mut pos = 0usize;
        let mut first = true;
        while pos < total {
            let end = (pos + CHUNK).min(total);
            let more = end < total;
            let m = if more { 1 } else { 0 };
            let keys = if first {
                format!("a=T,f=32,s={width},v={height},q=2,m={m}")
            } else {
                format!("m={m}")
            };
            let seq = format!("\x1b_G{keys};{}\x1b\\", &b64[pos..end]);
            self.feed(&seq);
            first = false;
            pos = end;
        }
    }

    /// 选区开始（按下）：锚点 = 当前端点 = 视口坐标。
    pub fn selection_anchor(&mut self, x: u16, y: u32) {
        self.sel = Some(((x, y), (x, y)));
        self.repaint_all = true;
    }

    /// 选区延伸（拖动）。
    pub fn selection_extend(&mut self, x: u16, y: u32) {
        if let Some((anchor, _)) = self.sel {
            self.sel = Some((anchor, (x, y)));
            self.repaint_all = true;
        }
    }

    /// 清空选区。
    pub fn selection_clear(&mut self) {
        if self.sel.take().is_some() {
            self.repaint_all = true;
        }
    }

    /// 有非空选区（锚点 ≠ 端点）。
    pub fn selection_active(&self) -> bool {
        matches!(self.sel, Some((a, b)) if a != b)
    }

    /// 视口格是否在选区内（线性范围）。row 是视口行号。
    pub fn selection_includes(&self, x: u16, row: u16) -> bool {
        let Some(((ax, ay), (bx, by))) = self.sel else {
            return false;
        };
        let ((sx, sy), (ex, ey)) = if (ay, ax) <= (by, bx) {
            ((ax, ay), (bx, by))
        } else {
            ((bx, by), (ax, ay))
        };
        let (x, row) = (x as u32, row as u32);
        (row > sy || (row == sy && x >= sx as u32))
            && (row < ey || (row == ey && x <= ex as u32))
    }

    /// 取选区文本（格式化器：plain + unwrap + trim）。
    pub fn selection_text(&mut self) -> Option<String> {
        let ((ax, ay), (bx, by)) = self.sel.filter(|(a, b)| a != b)?;
        unsafe {
            let viewport_point = |x: u16, y: u32| GhosttyPoint {
                tag: POINT_TAG_VIEWPORT,
                value: GhosttyPointValue {
                    coordinate: GhosttyPointCoordinate { x, y },
                },
            };
            let mut start = GhosttyGridRef {
                node: std::ptr::null_mut(),
                x: 0,
                y: 0,
            };
            let mut end = GhosttyGridRef {
                node: std::ptr::null_mut(),
                x: 0,
                y: 0,
            };
            if ghostty_terminal_grid_ref(self.terminal, viewport_point(ax, ay), &mut start)
                != GHOSTTY_SUCCESS
                || ghostty_terminal_grid_ref(self.terminal, viewport_point(bx, by), &mut end)
                    != GHOSTTY_SUCCESS
            {
                return None;
            }
            let selection = GhosttySelectionFFI {
                start,
                end,
                rectangle: false,
            };
            let mut formatter: GhosttyFormatter = std::ptr::null_mut();
            if ghostty_formatter_terminal_new(std::ptr::null(), &mut formatter, self.terminal)
                != GHOSTTY_SUCCESS
                || formatter.is_null()
            {
                return None;
            }
            let unwrap = true;
            let trim = true;
            let _ = ghostty_formatter_set(
                formatter,
                FORMATTER_OPT_UNWRAP,
                &unwrap as *const bool as *const c_void,
            );
            let _ = ghostty_formatter_set(
                formatter,
                FORMATTER_OPT_TRIM,
                &trim as *const bool as *const c_void,
            );
            let ok = ghostty_formatter_set(
                formatter,
                FORMATTER_OPT_SELECTION,
                &selection as *const _ as *const c_void,
            ) == GHOSTTY_SUCCESS;
            let mut out = None;
            if ok {
                let mut needed = 0usize;
                let rc = ghostty_formatter_format_buf(formatter, std::ptr::null_mut(), 0, &mut needed);
                if rc == 0 || needed > 0 {
                    let mut buf = vec![0u8; needed + 1];
                    let mut written = 0usize;
                    if ghostty_formatter_format_buf(
                        formatter,
                        buf.as_mut_ptr(),
                        buf.len(),
                        &mut written,
                    ) == GHOSTTY_SUCCESS
                    {
                        buf.truncate(written);
                        out = Some(String::from_utf8_lossy(&buf).into_owned());
                    }
                }
            }
            ghostty_formatter_free(formatter);
            out
        }
    }

    /// 读本帧所有可见 kitty 放置。像素按 generation 缓存，没变就不重新上传。
    unsafe fn collect_images(&mut self) {
        self.placed.clear();
        if ghostty_render_state_get(
            self.state,
            RS_DATA_IMAGE_ITERATOR,
            &mut self.images_iter as *mut _ as *mut c_void,
        ) != GHOSTTY_SUCCESS
        {
            return;
        }
        let mut graphics: GhosttyKittyGraphics = std::ptr::null_mut();
        if ghostty_terminal_get(
            self.terminal,
            TERM_DATA_KITTY_GRAPHICS,
            &mut graphics as *mut _ as *mut c_void,
        ) != GHOSTTY_SUCCESS
            || graphics.is_null()
        {
            return;
        }
        let mut seen: Vec<(u32, u64)> = Vec::new();
        while ghostty_render_state_image_iterator_next(self.images_iter) {
            if self.placed.len() >= 64 {
                break;
            }
            let mut protocol: i32 = -1;
            if ghostty_render_state_image_get(
                self.images_iter,
                RS_IMAGE_PROTOCOL,
                &mut protocol as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
                || protocol != 0
            {
                continue;
            }
            let mut id: u32 = 0;
            let mut generation: u64 = 0;
            let mut col: i32 = 0;
            let mut row: i32 = 0;
            let mut ox: u32 = 0;
            let mut oy: u32 = 0;
            let mut pw: u32 = 0;
            let mut ph: u32 = 0;
            let mut sx: u32 = 0;
            let mut sy: u32 = 0;
            let mut sw: u32 = 0;
            let mut sh: u32 = 0;
            let ok = [
                (RS_IMAGE_ID, &mut id as *mut u32 as *mut c_void),
                (RS_IMAGE_GENERATION, &mut generation as *mut u64 as *mut c_void),
                (RS_IMAGE_COL, &mut col as *mut i32 as *mut c_void),
                (RS_IMAGE_ROW, &mut row as *mut i32 as *mut c_void),
                (RS_IMAGE_OFFSET_X, &mut ox as *mut u32 as *mut c_void),
                (RS_IMAGE_OFFSET_Y, &mut oy as *mut u32 as *mut c_void),
                (RS_IMAGE_PIXEL_W, &mut pw as *mut u32 as *mut c_void),
                (RS_IMAGE_PIXEL_H, &mut ph as *mut u32 as *mut c_void),
                (RS_IMAGE_SRC_X, &mut sx as *mut u32 as *mut c_void),
                (RS_IMAGE_SRC_Y, &mut sy as *mut u32 as *mut c_void),
                (RS_IMAGE_SRC_W, &mut sw as *mut u32 as *mut c_void),
                (RS_IMAGE_SRC_H, &mut sh as *mut u32 as *mut c_void),
            ]
            .into_iter()
            .all(|(key, out)| {
                ghostty_render_state_image_get(self.images_iter, key, out) == GHOSTTY_SUCCESS
            });
            if !ok || pw == 0 || ph == 0 || pw > 8192 || ph > 8192 {
                continue;
            }
            let key = (id, generation);
            let image = if let Some(cached) = self.image_cache.get(&key) {
                cached.clone()
            } else if let Some(decoded) = decode_kitty_image(graphics, id, sx, sy, sw, sh) {
                let decoded = Arc::new(decoded);
                self.image_cache.insert(key, decoded.clone());
                decoded
            } else {
                continue;
            };
            seen.push(key);
            self.placed.push(PlacedImage {
                col,
                row,
                ox,
                oy,
                w: pw,
                h: ph,
                image,
            });
        }
        self.image_cache.retain(|key, _| seen.contains(key));
    }

    unsafe fn render_row(&mut self, row: u16) -> Vec<Span> {
        let mut spans: Vec<Span> = Vec::new();
        let mut x: u16 = 0;
        while x < self.cols {
            if ghostty_render_state_row_cells_select(self.cells, x) != GHOSTTY_SUCCESS {
                break;
            }
            // codepoint
            let mut raw: GhosttyCell = 0;
            let mut ch: Option<char> = None;
            if ghostty_render_state_row_cells_get(
                self.cells,
                RS_CELLS_DATA_RAW,
                &mut raw as *mut _ as *mut c_void,
            ) == GHOSTTY_SUCCESS
            {
                let mut cp: u32 = 0;
                if ghostty_cell_get(raw, CELL_DATA_CODEPOINT, &mut cp as *mut _ as *mut c_void)
                    == GHOSTTY_SUCCESS
                    && cp != 0
                {
                    ch = char::from_u32(cp);
                }
            }

            // style
            let mut style = GhosttyStyle::default();
            let _ = ghostty_render_state_row_cells_get(
                self.cells,
                RS_CELLS_DATA_STYLE,
                &mut style as *mut _ as *mut c_void,
            );
            let (mut fg, mut bg) = style_colors(&style);
            // 默认色（tag=NONE）不画的话会落到 GPUI 的黑字上，深色底上看不见。
            if fg.is_none() {
                fg = Some(0xe8ecf4);
            }
            let mut bold = style.flags & STYLE_FLAG_BOLD != 0;
            let is_cursor = self.cursor == Some((x, row));
            if is_cursor {
                fg = Some(0x101319);
                bg = Some(0xe8ecf4);
                bold = false;
            } else if self.selection_includes(x, row) {
                // 选区高亮（光标样式优先）
                bg = Some(0x1d4d66);
            }

            // 宽字符跳过 spacer
            let mut width = 1;
            if let Some(c) = ch {
                if is_wide(c) {
                    width = 2;
                }
            }

            let text: String = match ch {
                Some(c) => c.to_string(),
                None => " ".to_string(),
            };

            // 合并到上一个 span（同色且未断行）
            let can_merge = matches!(
                spans.last(),
                Some(last)
                    if last.fg == fg
                        && last.bg == bg
                        && last.bold == bold
                        && !last.cursor
                        && !is_cursor
            );
            if can_merge {
                spans.last_mut().unwrap().text.push_str(&text);
            } else {
                spans.push(Span {
                    text,
                    fg,
                    bg,
                    bold,
                    cursor: is_cursor,
                });
            }

            x += width;
        }

        // 宽字符占两格：render_row 里 wide 已跳过下一格（x += width），文本用单字符
        spans
    }
}

/// 解析 GhosttyStyle 的 fg/bg 为 RGB。palette 用 xterm 256 色算法。
fn style_colors(style: &GhosttyStyle) -> (Option<u32>, Option<u32>) {
    unsafe {
        let fg = style_color(&style.fg_color);
        let bg = style_color(&style.bg_color);
        // INVERSE 交换
        if style.flags & STYLE_FLAG_INVERSE != 0 {
            (bg, fg)
        } else {
            (fg, bg)
        }
    }
}

unsafe fn style_color(color: &GhosttyStyleColor) -> Option<u32> {
    match color.tag {
        STYLE_COLOR_RGB => Some(rgb_u32(color.value.rgb)),
        STYLE_COLOR_PALETTE => Some(xterm256(color.value.palette)),
        _ => None,
    }
}

fn rgb_u32(c: GhosttyColorRgb) -> u32 {
    (c.r as u32) << 16 | (c.g as u32) << 8 | (c.b as u32)
}

/// 标准 xterm 256 色展开（0-15 基础色用常见深色主题近似）。
fn xterm256(idx: u8) -> u32 {
    const BASE: [u32; 16] = [
        0x1b1b20, 0xc8433f, 0x43a047, 0xd6a032, 0x3f7fc8, 0xa855b8, 0x26a69a, 0xa8a8b0,
        0x555560, 0xe05c58, 0x5cc26a, 0xe8b64c, 0x5c9ce0, 0xc473d6, 0x46c8bc, 0xd8d8e0,
    ];
    match idx {
        0..=15 => BASE[idx as usize],
        16..=231 => {
            let i = (idx - 16) as u32;
            // 6x6x6 立方体，梯度 [0,95,135,175,215,255]
            const RAMP: [u32; 6] = [0, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
            let r = RAMP[((i / 36) % 6) as usize];
            let g = RAMP[((i / 6) % 6) as usize];
            let b = RAMP[(i % 6) as usize];
            r << 16 | g << 8 | b
        }
        _ => {
            // 232-255 灰度 8..232
            let gray = 8 + (idx as u32 - 232) * 10;
            gray << 16 | gray << 8 | gray
        }
    }
}

fn is_wide(ch: char) -> bool {
    let cp = ch as u32;
    (0x1100..=0x115F).contains(&cp)
        || (0x2E80..=0xA4CF).contains(&cp)
        || (0xAC00..=0xD7A3).contains(&cp)
        || (0xF900..=0xFAFF).contains(&cp)
        || (0xFE30..=0xFE6F).contains(&cp)
        || (0xFF00..=0xFF60).contains(&cp)
        || (0xFFE0..=0xFFE6).contains(&cp)
        || (0x20000..=0x3FFFD).contains(&cp)
}

/// 把 kitty 存储里的已解码像素裁成 GPUI 要的 BGRA。
unsafe fn decode_kitty_image(
    graphics: GhosttyKittyGraphics,
    id: u32,
    sx: u32,
    sy: u32,
    sw: u32,
    sh: u32,
) -> Option<RenderImage> {
    let image = ghostty_kitty_graphics_image(graphics, id);
    if image.is_null() {
        return None;
    }
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    let mut format: i32 = -1;
    let mut pixels = GhosttyString {
        ptr: std::ptr::null(),
        len: 0,
    };
    if ghostty_kitty_graphics_image_get(image, KITTY_IMAGE_WIDTH, &mut width as *mut _ as *mut c_void)
        != GHOSTTY_SUCCESS
        || ghostty_kitty_graphics_image_get(
            image,
            KITTY_IMAGE_HEIGHT,
            &mut height as *mut _ as *mut c_void,
        ) != GHOSTTY_SUCCESS
        || ghostty_kitty_graphics_image_get(
            image,
            KITTY_IMAGE_FORMAT,
            &mut format as *mut _ as *mut c_void,
        ) != GHOSTTY_SUCCESS
        || ghostty_kitty_graphics_image_get(
            image,
            KITTY_IMAGE_PIXELS,
            &mut pixels as *mut _ as *mut c_void,
        ) != GHOSTTY_SUCCESS
        || pixels.ptr.is_null()
        || width == 0
        || height == 0
    {
        return None;
    }
    let bpp = match format {
        KITTY_FMT_RGB => 3,
        KITTY_FMT_RGBA => 4,
        KITTY_FMT_GRAY_ALPHA => 2,
        KITTY_FMT_GRAY => 1,
        _ => return None,
    };
    let need = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(bpp)?;
    if pixels.len < need {
        return None;
    }
    let src = std::slice::from_raw_parts(pixels.ptr, need);
    let (cw, ch, cx, cy) = if sw == 0 || sh == 0 {
        (width, height, 0, 0)
    } else {
        (sw.min(width.saturating_sub(sx)), sh.min(height.saturating_sub(sy)), sx, sy)
    };
    if cw == 0 || ch == 0 {
        return None;
    }
    let mut bgra = Vec::with_capacity((cw as usize) * (ch as usize) * 4);
    for y in cy..cy + ch {
        for x in cx..cx + cw {
            let i = ((y as usize) * (width as usize) + (x as usize)) * bpp;
            let (r, g, b, a) = match bpp {
                4 => (src[i], src[i + 1], src[i + 2], src[i + 3]),
                3 => (src[i], src[i + 1], src[i + 2], 255),
                2 => (src[i], src[i], src[i], src[i + 1]),
                _ => (src[i], src[i], src[i], 255),
            };
            bgra.extend_from_slice(&[b, g, r, a]);
        }
    }
    let buffer = image::RgbaImage::from_raw(cw, ch, bgra)?;
    Some(RenderImage::new(vec![image::Frame::new(buffer)]))
}

impl Drop for Term {
    fn drop(&mut self) {
        unsafe {
            ghostty_search_free(self.search);
            ghostty_render_state_image_iterator_free(self.images_iter);
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_free(self.state);
            ghostty_terminal_free(self.terminal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Term;

    fn line(term: &mut Term) -> String {
        term.render_rows()
            .into_iter()
            .map(|row| row.into_iter().map(|span| span.text).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn default_text_is_visible_and_clean_rows_stay() {
        let mut term = Term::new(80, 24).expect("ghostty");
        term.feed("\x1b[01;32muser\x1b[00m$ hi\r\n");
        let rows = term.render_rows();
        let first: String = rows[0].iter().map(|span| span.text.as_str()).collect();
        assert!(first.contains("hi"), "{first}");
        assert!(
            rows[0]
                .iter()
                .any(|span| span.text.contains('h') && span.fg == Some(0xe8ecf4)),
            "default fg must be light, got {rows:?}"
        );
        term.feed("next\r\n");
        let painted = line(&mut term);
        assert!(painted.contains("hi"), "{painted}");
        assert!(painted.contains("next"), "{painted}");
    }

    #[test]
    fn caps_lock_letter_is_not_rewritten_to_lowercase() {
        let mut term = Term::new(80, 24).expect("ghostty");
        // bash 开 bracketed paste，sudo 再关掉。编码必须仍是该字符本身。
        term.feed("\x1b[?2004h\x1b[?2004l");
        let lower = term.encode_key(27, 0, "h", 'h' as u32);
        assert_eq!(lower, b"h");
        // Caps Lock：文本是大写，shift 修饰是关的，unshifted 仍是小写码位。
        let caps = term.encode_key(27, 0, "H", 'h' as u32);
        assert_eq!(caps, b"H", "got {caps:?}");
        let shift = term.encode_key(27, 1, "H", 'h' as u32);
        assert_eq!(shift, b"H", "got {shift:?}");
        assert_eq!(term.encode_key(58, 0, "", 0), b"\r");
    }

    #[test]
    fn kitty_graphics_placement_is_decoded() {
        use base64::Engine as _;
        let mut term = Term::new(40, 12).expect("ghostty");
        // 2×2 RGB 红块，直接显示（a=T, f=24）。
        let pixels = [255u8, 0, 0, 255, 0, 0, 255, 0, 0, 255, 0, 0];
        let payload = base64::engine::general_purpose::STANDARD.encode(pixels);
        term.feed(&format!("\x1b_Ga=T,f=24,s=2,v=2;{payload}\x1b\\"));
        let _ = term.render_rows();
        let images = term.images();
        assert_eq!(images.len(), 1, "expected one placement");
        assert!(images[0].w > 0 && images[0].h > 0);
    }

    #[test]
    fn search_finds_scrollback_and_scrolls_viewport() {
        let mut term = Term::new(40, 6).expect("ghostty");
        for i in 0..30 {
            term.feed(&format!("line-{i:02}\r\n"));
        }
        let _ = term.render_rows();
        term.search_set_needle("line-03");
        term.search_run();
        let (total, _) = term.search_counts();
        assert_eq!(total, 1, "expected exactly one match");
        let hit = term.search_select(true);
        assert_eq!(hit, Some((0, 1)), "match must be selected");
        // 视口必须滚到匹配处：渲染出来的行里能看到 line-03
        let rows = term.render_rows();
        let text: String = rows
            .iter()
            .map(|row| row.iter().map(|span| span.text.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("line-03"), "viewport did not scroll: {text}");
        // 清空搜索词后计数归零
        term.search_clear();
        let (total, _) = term.search_counts();
        assert_eq!(total, 0);
    }

    #[test]
    fn paste_text_follows_bracketed_mode() {
        let mut term = Term::new(40, 6).expect("ghostty");
        // 没开 bracketed：换行要变 CR，无包裹
        let plain = term.paste_text("a\nb");
        assert_eq!(plain, b"a\rb", "plain paste: {plain:?}");
        // 开了 bracketed（mode 2004）：加 xterm 标准前后缀（CSI 200~ / 201~）
        term.feed("\x1b[?2004h");
        let bracketed = term.paste_text("a\nb");
        assert_eq!(bracketed, b"\x1b[200~a\nb\x1b[201~", "bracketed: {bracketed:?}");
    }

    #[test]
    fn paste_image_places_kitty_graphics() {
        let mut term = Term::new(40, 12).expect("ghostty");
        // 3×2 RGBA 渐变块
        let mut rgba = Vec::new();
        for y in 0..2u8 {
            for x in 0..3u8 {
                rgba.extend_from_slice(&[x * 40, y * 80, 0x80, 0xff]);
            }
        }
        term.paste_image_rgba(3, 2, &rgba);
        let _ = term.render_rows();
        assert_eq!(term.images().len(), 1, "pasted image must be placed");
    }

    #[test]
    fn selection_text_extracts_dragged_range() {
        let mut term = Term::new(40, 6).expect("ghostty");
        term.feed("hello world\r\nsecond line\r\n");
        let _ = term.render_rows();
        // 选 "llo wor"（第 0 行 2..9）
        term.selection_anchor(2, 0);
        term.selection_extend(9, 0);
        assert!(term.selection_active());
        assert!(term.selection_includes(3, 0));
        assert!(!term.selection_includes(0, 0));
        assert!(!term.selection_includes(0, 1));
        let text = term.selection_text().expect("selection text");
        assert_eq!(text.trim(), "llo worl", "got {text:?}"); // 两端都包含
        // 跨行：第 0 行 6..第 1 行 6
        term.selection_anchor(6, 0);
        term.selection_extend(6, 1);
        let text = term.selection_text().expect("cross-line");
        assert!(text.contains("world"), "cross-line: {text:?}");
        assert!(text.contains("second"), "cross-line: {text:?}");
        term.selection_clear();
        assert!(!term.selection_active());
    }

    #[test]
    fn kitty_frame_cost() {
        use base64::Engine as _;
        use std::time::Instant;
        // 800×450 RGB ≈ 一张终端里的整页图（1.08MB 像素，base64 后约 1.44MB）。
        const W: usize = 800;
        const H: usize = 450;
        let pixels = vec![0x3cu8; W * H * 3];
        let payload = base64::engine::general_purpose::STANDARD.encode(&pixels);
        let esc = format!("\x1b_Ga=T,f=24,s={W},v={H},q=2;{payload}\x1b\\");
        let mut term = Term::new(120, 40).expect("ghostty");

        let t0 = Instant::now();
        term.feed(&esc);
        let _ = term.render_rows();
        let first_ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(term.images().len(), 1, "first frame produced no image");

        let t1 = Instant::now();
        for _ in 0..30 {
            let _ = term.render_rows();
        }
        let cached_ms = t1.elapsed().as_secs_f64() * 1000.0 / 30.0;

        let t2 = Instant::now();
        for i in 0..8u32 {
            let frame = format!("\x1b_Ga=T,f=24,s={W},v={H},i={i},q=2;{payload}\x1b\\");
            term.feed(&frame);
            let _ = term.render_rows();
        }
        let fresh_ms = t2.elapsed().as_secs_f64() * 1000.0 / 8.0;

        eprintln!(
            "[perf] kitty {W}x{H} rgb first={first_ms:.2}ms cached={cached_ms:.3}ms/frame fresh={fresh_ms:.2}ms/frame"
        );
    }
}
