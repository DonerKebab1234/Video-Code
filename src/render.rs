//! Draws character cells (and on-screen text) into a 0x00RRGGBB pixel buffer.

use rayon::prelude::*;

use crate::{
    ascii::{convert, Cell, Look},
    font::GlyphSet,
};

pub const WHITE: u32 = 0xF2F2F2;
pub const GRAY: u32 = 0x9A9A9A;
pub const ACCENT: u32 = 0x4FD8FF;

/// Where the character grid sits inside the pixel buffer.
#[derive(Clone, Copy, Debug, Default)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    pub off_x: usize,
    pub off_y: usize,
}

impl Grid {
    /// The biggest grid with the picture's aspect ratio that fits in `bw` x `bh`, centred.
    pub fn fit(aspect: f64, bw: usize, bh: usize, cw: usize, ch: usize) -> Grid {
        let (mut w, mut h) = (bw as f64, bh as f64);
        if w / h > aspect {
            w = h * aspect;
        } else {
            h = w / aspect;
        }
        let cols = ((w / cw as f64) as usize).max(1);
        let rows = ((h / ch as f64) as usize).max(1);
        Grid {
            cols,
            rows,
            off_x: bw.saturating_sub(cols * cw) / 2,
            off_y: bh.saturating_sub(rows * ch) / 2,
        }
    }
}

pub struct Renderer {
    pub glyphs: GlyphSet,
    cells: Vec<Cell>,
    /// Grid used for the last frame (shown in the stats overlay).
    pub grid: Grid,
}

impl Renderer {
    pub fn new(glyphs: GlyphSet) -> Self {
        Self { glyphs, cells: Vec::new(), grid: Grid::default() }
    }

    /// Draws `frame` as big as fits in the buffer, keeping its aspect ratio.
    pub fn draw_fit(&mut self, frame: &[u8], fw: usize, fh: usize, look: &Look, buf: &mut [u32], bw: usize, bh: usize) {
        let grid = Grid::fit(fw as f64 / fh as f64, bw, bh, self.glyphs.cell_w, self.glyphs.cell_h);
        self.draw_grid(frame, fw, fh, look, grid, buf, bw, bh);
    }

    pub fn draw_grid(
        &mut self,
        frame: &[u8],
        fw: usize,
        fh: usize,
        look: &Look,
        grid: Grid,
        buf: &mut [u32],
        bw: usize,
        bh: usize,
    ) {
        convert(frame, fw, fh, grid.cols, grid.rows, &self.glyphs, look, &mut self.cells);
        buf.par_chunks_mut(bw.max(1) * 16).for_each(|c| c.fill(0));
        render_cells(&self.cells, &grid, &self.glyphs, buf, bw, bh);
        self.grid = grid;
    }
}

fn render_cells(cells: &[Cell], grid: &Grid, glyphs: &GlyphSet, buf: &mut [u32], bw: usize, bh: usize) {
    let (cw, ch) = (glyphs.cell_w, glyphs.cell_h);
    if grid.off_y >= bh || grid.off_x >= bw {
        return;
    }
    let rows = grid.rows.min((bh - grid.off_y) / ch);
    let cols = grid.cols.min((bw - grid.off_x) / cw);
    if rows == 0 || cols == 0 {
        return;
    }

    let start = grid.off_y * bw;
    buf[start..start + rows * ch * bw]
        .par_chunks_mut(ch * bw)
        .enumerate()
        .for_each(|(r, band)| {
            let line = &cells[r * grid.cols..r * grid.cols + cols];
            for (c, cell) in line.iter().enumerate() {
                let bitmap = glyphs.bitmap(cell.glyph as usize);
                let x0 = grid.off_x + c * cw;
                for y in 0..ch {
                    let dst = &mut band[y * bw + x0..y * bw + x0 + cw];
                    let src = &bitmap[y * cw..(y + 1) * cw];
                    for (d, &a) in dst.iter_mut().zip(src) {
                        *d = blend(cell.bg, cell.fg, a);
                    }
                }
            }
        });
}

/// Mixes `fg` over `bg` with coverage `a` (0..255).
#[inline(always)]
pub fn blend(bg: u32, fg: u32, a: u8) -> u32 {
    match a {
        0 => bg,
        255 => fg,
        _ => {
            let a = a as u32;
            let ia = 255 - a;
            let mix = |shift: u32| ((((fg >> shift) & 0xFF) * a + ((bg >> shift) & 0xFF) * ia + 127) / 255) << shift;
            mix(16) | mix(8) | mix(0)
        }
    }
}

// ---------------------------------------------------------------------------
// On-screen text (HUD, messages)
// ---------------------------------------------------------------------------

pub fn shade_rect(buf: &mut [u32], bw: usize, bh: usize, x: usize, y: usize, w: usize, h: usize) {
    let (x0, x1) = (x.min(bw), (x + w).min(bw));
    for yy in y.min(bh)..(y + h).min(bh) {
        for p in &mut buf[yy * bw + x0..yy * bw + x1] {
            *p = (*p >> 2) & 0x3F3F3F;
        }
    }
}

pub fn fill_rect(buf: &mut [u32], bw: usize, bh: usize, x: usize, y: usize, w: usize, h: usize, color: u32) {
    let (x0, x1) = (x.min(bw), (x + w).min(bw));
    for yy in y.min(bh)..(y + h).min(bh) {
        buf[yy * bw + x0..yy * bw + x1].fill(color);
    }
}

pub fn draw_text(buf: &mut [u32], bw: usize, bh: usize, g: &GlyphSet, x: usize, y: usize, text: &str, color: u32) {
    let fallback = g.index_of('?').unwrap_or(0);
    for (i, c) in text.chars().enumerate() {
        let gx = x + i * g.cell_w;
        if gx + g.cell_w > bw {
            break;
        }
        if c == ' ' {
            continue;
        }
        let bitmap = g.bitmap(g.index_of(c).unwrap_or(fallback));
        for yy in 0..g.cell_h {
            let py = y + yy;
            if py >= bh {
                break;
            }
            let row = &mut buf[py * bw + gx..py * bw + gx + g.cell_w];
            for (d, &a) in row.iter_mut().zip(&bitmap[yy * g.cell_w..(yy + 1) * g.cell_w]) {
                *d = blend(*d, color, a);
            }
        }
    }
}

/// Size of a shaded text box holding `lines`.
pub fn text_box_size(g: &GlyphSet, lines: &[(String, u32)]) -> (usize, usize) {
    let longest = lines.iter().map(|(s, _)| s.chars().count()).max().unwrap_or(0);
    let pad = g.cell_w;
    (longest * g.cell_w + 2 * pad, lines.len() * (g.cell_h + 2) + 2 * pad)
}

pub fn text_box(buf: &mut [u32], bw: usize, bh: usize, g: &GlyphSet, x: usize, y: usize, lines: &[(String, u32)]) {
    let (w, h) = text_box_size(g, lines);
    shade_rect(buf, bw, bh, x, y, w, h);
    let pad = g.cell_w;
    for (i, (text, color)) in lines.iter().enumerate() {
        draw_text(buf, bw, bh, g, x + pad, y + pad + i * (g.cell_h + 2), text, *color);
    }
}
