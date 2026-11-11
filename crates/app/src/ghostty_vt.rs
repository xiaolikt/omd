//! libghostty-vt FFI（vt-1.0.0）。
//!
//! 渲染路径走 render-state：`render_state_update`（吃掉脏状态）→
//! `row_iterator_next_dirty`（只遍历脏行）→ `row_get(CELLS)` →
//! `row_cells_get(RAW/GRAPHEMES/STYLE/…)`。
//! VT 写入：`terminal_vt_write`。Kitty 图形：`image_iterator`（后续）。

use std::ffi::c_void;
use std::os::raw::c_int;

pub type GhosttyTerminal = *mut c_void;
pub type GhosttyRenderState = *mut c_void;
pub type GhosttyRenderStateRowIterator = *mut c_void;
pub type GhosttyRenderStateRowCells = *mut c_void;
pub type GhosttyCell = u64; // packed value（GhosttyCell，64 位打包描述符）
pub type GhosttyRow = u64;

pub const GHOSTTY_SUCCESS: c_int = 0;

// GhosttyRenderStateData（render.h 真实枚举值）
pub const RS_DATA_COLS: c_int = 1;
pub const RS_DATA_ROWS: c_int = 2;
pub const RS_DATA_DIRTY: c_int = 3;
pub const RS_DATA_ROW_ITERATOR: c_int = 4;
pub const RS_DATA_CURSOR_VISIBLE: c_int = 11;
pub const RS_DATA_CURSOR_VIEWPORT_X: c_int = 15;
pub const RS_DATA_CURSOR_VIEWPORT_Y: c_int = 16;

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

    // （保留 set 仅作他用；iterator 用 get 填充，不用 set）
}
