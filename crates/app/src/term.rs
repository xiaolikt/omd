//! Terminal block：libghostty-vt 包装。
//!
//! feed() 进 VT 字节；render_rows() 出视口（每行 = 样式化 span 列表）。
//! 颜色：RGB 直接用；palette 索引用标准 xterm 256 色算法展开；
//! NONE 用默认前景/背景。Kitty 图形（image_iterator）后续。

use crate::ghostty_vt::*;
use std::ffi::c_void;

/// 一个渲染 span：同色连续字符段。
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    /// None = 默认前景
    pub fg: Option<u32>,
    /// None = 默认背景
    pub bg: Option<u32>,
    pub bold: bool,
}

pub struct Term {
    terminal: GhosttyTerminal,
    state: GhosttyRenderState,
    cells: GhosttyRenderStateRowCells,
    cols: u16,
    rows: u16,
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
            Some(Self {
                terminal,
                state,
                cells,
                cols,
                rows,
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
            ghostty_terminal_resize(self.terminal, cols, rows, cw, ch);
            self.cols = cols;
            self.rows = rows;
        }
    }

    pub fn dims(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }

    /// 当前视口：每行一个 span 列表。
    pub fn render_rows(&mut self) -> Vec<Vec<Span>> {
        unsafe {
            let mut result: Vec<Vec<Span>> = vec![Vec::new(); self.rows as usize];
            ghostty_render_state_update(self.state, self.terminal);

            let mut iter: GhosttyRenderStateRowIterator = std::ptr::null_mut();
            if ghostty_render_state_row_iterator_new(std::ptr::null(), &mut iter) != GHOSTTY_SUCCESS {
                return result;
            }
            if ghostty_render_state_get(
                self.state,
                RS_DATA_ROW_ITERATOR,
                &mut iter as *mut _ as *mut c_void,
            ) != GHOSTTY_SUCCESS
            {
                ghostty_render_state_row_iterator_free(iter);
                return result;
            }

            let mut y: u16 = 0;
            while ghostty_render_state_row_iterator_next_dirty(iter, &mut y) {
                if y as usize >= result.len() {
                    continue;
                }
                if ghostty_render_state_row_get(
                    iter,
                    RS_ROW_DATA_CELLS,
                    &mut self.cells as *mut _ as *mut c_void,
                ) == GHOSTTY_SUCCESS
                {
                    result[y as usize] = self.render_row();
                }
            }
            ghostty_render_state_row_iterator_free(iter);
            ghostty_render_state_clean(self.state);
            result
        }
    }

    unsafe fn render_row(&mut self) -> Vec<Span> {
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
            let (fg, bg) = style_colors(&style);
            let bold = style.flags & STYLE_FLAG_BOLD != 0;

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
                    if last.fg == fg && last.bg == bg && last.bold == bold
            );
            if can_merge {
                spans.last_mut().unwrap().text.push_str(&text);
            } else {
                spans.push(Span {
                    text,
                    fg,
                    bg,
                    bold,
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

impl Drop for Term {
    fn drop(&mut self) {
        unsafe {
            ghostty_render_state_row_cells_free(self.cells);
            ghostty_render_state_free(self.state);
            ghostty_terminal_free(self.terminal);
        }
    }
}
