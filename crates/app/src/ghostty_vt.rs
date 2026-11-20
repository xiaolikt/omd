//! libghostty-vt FFI（vt-1.0.0）。
//!
//! 渲染路径走 render-state：`render_state_update`（吃掉脏状态）→
//! `row_iterator_next_dirty`（只遍历脏行）→ `row_get(CELLS)` →
//! `row_cells_get(RAW/GRAPHEMES/STYLE/…)`。
//! VT 写入：`terminal_vt_write`。Kitty 图形：render-state image iterator
//! + `ghostty_kitty_graphics_image` 读已解码像素。

use std::ffi::c_void;
use std::os::raw::c_int;

pub type GhosttyTerminal = *mut c_void;
pub type GhosttyRenderState = *mut c_void;
pub type GhosttyRenderStateRowIterator = *mut c_void;
pub type GhosttyRenderStateRowCells = *mut c_void;
pub type GhosttyCell = u64; // packed value（GhosttyCell，64 位打包描述符）
pub type GhosttyRow = u64;

pub const GHOSTTY_SUCCESS: c_int = 0;
pub type GhosttyKeyEncoder = *mut c_void;
pub type GhosttyKeyEvent = *mut c_void;
pub type GhosttyKittyGraphics = *mut c_void;
pub type GhosttyKittyGraphicsImage = *mut c_void;
pub type GhosttySearch = *mut c_void;
pub type GhosttyRenderStateImageIterator = *mut c_void;
// GHOSTTY_MODS_* bits
pub const GHOSTTY_MODS_SHIFT: u16 = 1;
pub const GHOSTTY_MODS_CTRL: u16 = 2;
pub const GHOSTTY_MODS_ALT: u16 = 4;
pub const GHOSTTY_MODS_SUPER: u16 = 8;

// GhosttyRenderStateData（render.h 真实枚举值）
pub const RS_DATA_COLS: c_int = 1;
pub const RS_DATA_ROWS: c_int = 2;
pub const RS_DATA_DIRTY: c_int = 3;
pub const RS_DATA_ROW_ITERATOR: c_int = 4;
pub const RS_DATA_CURSOR_VISIBLE: c_int = 11;
pub const RS_DATA_CURSOR_VIEWPORT_X: c_int = 15;
pub const RS_DATA_CURSOR_VIEWPORT_Y: c_int = 16;
pub const RS_DATA_IMAGE_ITERATOR: c_int = 22;

pub const RS_IMAGE_PROTOCOL: c_int = 1;
pub const RS_IMAGE_ID: c_int = 2;
pub const RS_IMAGE_GENERATION: c_int = 3;
pub const RS_IMAGE_COL: c_int = 6;
pub const RS_IMAGE_ROW: c_int = 7;
pub const RS_IMAGE_OFFSET_X: c_int = 10;
pub const RS_IMAGE_OFFSET_Y: c_int = 11;
pub const RS_IMAGE_PIXEL_W: c_int = 12;
pub const RS_IMAGE_PIXEL_H: c_int = 13;
pub const RS_IMAGE_SRC_X: c_int = 14;
pub const RS_IMAGE_SRC_Y: c_int = 15;
pub const RS_IMAGE_SRC_W: c_int = 16;
pub const RS_IMAGE_SRC_H: c_int = 17;

pub const TERM_DATA_KITTY_GRAPHICS: c_int = 30;
pub const KITTY_IMAGE_WIDTH: c_int = 3;
pub const KITTY_IMAGE_HEIGHT: c_int = 4;
pub const KITTY_IMAGE_FORMAT: c_int = 5;
pub const KITTY_IMAGE_PIXELS: c_int = 7;
pub const KITTY_FMT_RGB: i32 = 0;
pub const KITTY_FMT_RGBA: i32 = 1;
pub const KITTY_FMT_GRAY_ALPHA: i32 = 3;
pub const KITTY_FMT_GRAY: i32 = 4;
pub const GHOSTTY_NO_VALUE: c_int = -4;

// GhosttyRenderStateRowData
pub const RS_ROW_DATA_DIRTY: c_int = 1;
pub const RS_ROW_DATA_CELLS: c_int = 3;
pub const RS_ROW_DATA_CELLS_RAW: c_int = 5;
pub const RS_ROW_DATA_VIEWPORT_Y: c_int = 6;

// GhosttyRenderStateRowCellsData
pub const RS_CELLS_DATA_RAW: c_int = 1;
pub const RS_CELLS_DATA_GRAPHEMES_LEN: c_int = 3;
pub const RS_CELLS_DATA_GRAPHEMES_BUF: c_int = 4;

// GhosttyCellData（screen.h）
pub const CELL_DATA_CODEPOINT: c_int = 1;

// GhosttyRenderStateRowCellsData::STYLE
pub const RS_CELLS_DATA_STYLE: c_int = 2;

// GhosttySearchOption / GhosttySearchData
pub const SEARCH_OPT_NEEDLE: c_int = 1;
pub const SEARCH_DATA_STATUS: c_int = 1;
pub const SEARCH_DATA_TOTAL_MATCHES: c_int = 3;
pub const SEARCH_DATA_SELECTED_INDEX: c_int = 4;

// GhosttyTerminalData::MODE（查询终端模式；GhosttyTerminalModeConfig）
pub const TERM_DATA_MODE: c_int = 37;
/// ghostty_mode_new(2004, false)：DEC private 的 bracketed paste
pub const MODE_BRACKETED_PASTE: u16 = 2004;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyModeConfig {
    pub mode: u16,
    pub value: bool,
}

// GhosttyRenderStateData::COLOR_PALETTE（调色板查询）
pub const RS_DATA_COLOR_PALETTE: c_int = 9;

// GhosttyStyleColorTag
pub const STYLE_COLOR_NONE: u8 = 0;
pub const STYLE_COLOR_PALETTE: u8 = 1;
pub const STYLE_COLOR_RGB: u8 = 2;

// GhosttyStyleFlags
pub const STYLE_FLAG_BOLD: u16 = 1 << 0;
pub const STYLE_FLAG_INVERSE: u16 = 1 << 4;

#[repr(C)]
#[derive(Clone, Copy)]
pub union GhosttyStyleColorValue {
    pub palette: u8,
    pub rgb: GhosttyColorRgb,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyStyleColor {
    pub tag: u8,
    pub value: GhosttyStyleColorValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// 16 字节固定布局（style.h 承诺不变）
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyStyle {
    pub fg_color: GhosttyStyleColor,
    pub bg_color: GhosttyStyleColor,
    pub underline_color: GhosttyStyleColor,
    pub flags: u16,
    pub reserved: u16,
}

impl Default for GhosttyStyle {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GhosttyCellsView {
    pub ptr: *const GhosttyCell,
    pub len: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyString {
    pub ptr: *const u8,
    pub len: usize,
}

/// ghostty_terminal_scroll_viewport 的行为（tag + 值联合体）
pub const SCROLL_VIEWPORT_TOP: i32 = 0;
pub const SCROLL_VIEWPORT_BOTTOM: i32 = 1;
pub const SCROLL_VIEWPORT_DELTA: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub union GhosttyScrollViewportValue {
    pub delta: isize,
    pub row: usize,
    pub _padding: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyScrollViewport {
    pub tag: i32,
    pub value: GhosttyScrollViewportValue,
}

extern "C" {
    // terminal
    pub fn ghostty_terminal_new(
        allocator: *const c_void,
        terminal: *mut GhosttyTerminal,
        cols: u16,
        rows: u16,
    ) -> c_int;
    pub fn ghostty_terminal_free(terminal: GhosttyTerminal);
    pub fn ghostty_terminal_resize(
        terminal: GhosttyTerminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> c_int;
    pub fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);
    pub fn ghostty_terminal_get(terminal: GhosttyTerminal, data: c_int, out: *mut c_void) -> c_int;

    // search（含 scrollback；select 会滚动视口）
    pub fn ghostty_search_new(
        allocator: *const c_void,
        out_search: *mut GhosttySearch,
        terminal: GhosttyTerminal,
    ) -> c_int;
    pub fn ghostty_search_free(search: GhosttySearch);
    pub fn ghostty_search_run(search: GhosttySearch) -> c_int;
    pub fn ghostty_search_select_next(search: GhosttySearch) -> c_int;
    pub fn ghostty_search_select_prev(search: GhosttySearch) -> c_int;
    pub fn ghostty_search_set(
        search: GhosttySearch,
        option: c_int,
        value: *const c_void,
    ) -> c_int;
    pub fn ghostty_search_get(search: GhosttySearch, data: c_int, out: *mut c_void) -> c_int;

    // 视口滚动（scrollback）
    pub fn ghostty_terminal_scroll_viewport(
        terminal: GhosttyTerminal,
        behavior: GhosttyScrollViewport,
    ) -> c_int;

    // 文本粘贴编码（按 bracketed paste 模式）
    pub fn ghostty_paste_encode(
        data: *mut u8,
        data_len: usize,
        bracketed: bool,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> c_int;

    // render state
    pub fn ghostty_render_state_new(
        allocator: *const c_void,
        state: *mut GhosttyRenderState,
    ) -> c_int;
    pub fn ghostty_render_state_free(state: GhosttyRenderState);
    pub fn ghostty_render_state_update(state: GhosttyRenderState, terminal: GhosttyTerminal) -> c_int;
    pub fn ghostty_render_state_clean(state: GhosttyRenderState) -> c_int;
    pub fn ghostty_render_state_get(state: GhosttyRenderState, data: c_int, out: *mut c_void) -> c_int;

    pub fn ghostty_render_state_row_iterator_new(
        allocator: *const c_void,
        out: *mut GhosttyRenderStateRowIterator,
    ) -> c_int;
    pub fn ghostty_render_state_row_iterator_free(iterator: GhosttyRenderStateRowIterator);
    pub fn ghostty_render_state_row_iterator_next_dirty(
        iterator: GhosttyRenderStateRowIterator,
        out_y: *mut u16,
    ) -> bool;
    pub fn ghostty_render_state_row_get(
        iterator: GhosttyRenderStateRowIterator,
        data: c_int,
        out: *mut c_void,
    ) -> c_int;

    pub fn ghostty_render_state_row_cells_new(
        allocator: *const c_void,
        out: *mut GhosttyRenderStateRowCells,
    ) -> c_int;
    pub fn ghostty_render_state_row_cells_free(cells: GhosttyRenderStateRowCells);
    pub fn ghostty_render_state_row_cells_select(
        cells: GhosttyRenderStateRowCells,
        x: u16,
    ) -> c_int;
    pub fn ghostty_render_state_row_cells_next(cells: GhosttyRenderStateRowCells) -> bool;
    pub fn ghostty_render_state_row_cells_get(
        cells: GhosttyRenderStateRowCells,
        data: c_int,
        out: *mut c_void,
    ) -> c_int;

    // cell 值解码（对 GhosttyCell packed value）
    pub fn ghostty_cell_get(cell: GhosttyCell, data: c_int, out: *mut c_void) -> c_int;

    // 键盘编码：按终端当前模式（cursor keys / kitty / modifyOtherKeys）产生 PTY 字节
    pub fn ghostty_key_encoder_new(alloc: *const c_void, out: *mut GhosttyKeyEncoder) -> c_int;
    pub fn ghostty_key_encoder_free(e: GhosttyKeyEncoder);
    pub fn ghostty_key_encoder_set_from_terminal(e: GhosttyKeyEncoder, t: GhosttyTerminal) -> c_int;
    pub fn ghostty_key_encoder_set(e: GhosttyKeyEncoder, opt: c_int, value: *const c_void) -> c_int;
    pub fn ghostty_key_encoder_encode(
        e: GhosttyKeyEncoder,
        ev: GhosttyKeyEvent,
        buf: *mut u8,
        cap: usize,
        len: *mut usize,
    ) -> c_int;
    pub fn ghostty_key_event_new(alloc: *const c_void, out: *mut GhosttyKeyEvent) -> c_int;
    pub fn ghostty_key_event_free(ev: GhosttyKeyEvent);
    pub fn ghostty_key_event_set_action(ev: GhosttyKeyEvent, action: c_int) -> c_int;
    pub fn ghostty_key_event_set_key(ev: GhosttyKeyEvent, key: c_int) -> c_int;
    pub fn ghostty_key_event_set_mods(ev: GhosttyKeyEvent, mods: u16) -> c_int;
    pub fn ghostty_key_event_set_utf8(ev: GhosttyKeyEvent, utf8: GhosttyString) -> c_int;
    pub fn ghostty_key_event_set_unshifted_codepoint(ev: GhosttyKeyEvent, cp: u32) -> c_int;

    pub fn ghostty_render_state_image_iterator_new(
        allocator: *const c_void,
        out: *mut GhosttyRenderStateImageIterator,
    ) -> c_int;
    pub fn ghostty_render_state_image_iterator_free(iterator: GhosttyRenderStateImageIterator);
    pub fn ghostty_render_state_image_iterator_next(iterator: GhosttyRenderStateImageIterator) -> bool;
    pub fn ghostty_render_state_image_get(
        iterator: GhosttyRenderStateImageIterator,
        data: c_int,
        out: *mut c_void,
    ) -> c_int;

    pub fn ghostty_kitty_graphics_image(
        graphics: GhosttyKittyGraphics,
        image_id: u32,
    ) -> GhosttyKittyGraphicsImage;
    pub fn ghostty_kitty_graphics_image_get(
        image: GhosttyKittyGraphicsImage,
        data: c_int,
        out: *mut c_void,
    ) -> c_int;
}
