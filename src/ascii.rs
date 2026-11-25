//! Turns an RGB frame into a grid of coloured characters.

use rayon::prelude::*;

use crate::font::{zone_row, GlyphSet, Shape, ZONES};

#[derive(Clone, Copy, Default, Debug)]
pub struct Cell {
    pub glyph: u16,
    /// 0x00RRGGBB
    pub fg: u32,
    pub bg: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorMode {
    /// Characters take the colour of the video.
    Color,
    /// Like `color`, plus a dim copy of the video behind the characters.
    ColorBg,
    /// Green terminal.
    Matrix,
    /// Amber monitor.
    Amber,
    /// White on black.
    Mono,
}

impl ColorMode {
    const ALL: [ColorMode; 5] = [
        ColorMode::Color,
        ColorMode::ColorBg,
        ColorMode::Matrix,
        ColorMode::Amber,
        ColorMode::Mono,
    ];

    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    pub fn name(self) -> &'static str {
        match self {
            ColorMode::Color => "color",
            ColorMode::ColorBg => "color-bg",
            ColorMode::Matrix => "matrix",
            ColorMode::Amber => "amber",
            ColorMode::Mono => "mono",
        }
    }
}

/// How characters are picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Style {
    /// Brightness ramp in flat areas, shape-matched characters along edges.
    Hybrid,
    /// Every cell gets the character whose shape best matches it.
    Shape,
    /// Old-school ASCII: characters picked by brightness only.
    Ramp,
}

impl Style {
    pub fn next(self) -> Self {
        match self {
            Style::Hybrid => Style::Shape,
            Style::Shape => Style::Ramp,
            Style::Ramp => Style::Hybrid,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Style::Hybrid => "hybrid",
            Style::Shape => "shape",
            Style::Ramp => "ramp",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Look {
    pub mode: ColorMode,
    pub style: Style,
    /// Exponent that exaggerates brightness differences inside a cell, so edges pick `/ | - _`.
    pub contrast: f32,
    pub brightness: f32,
    pub saturation: f32,
}

/// How far character colours are pushed towards full brightness. The character's
/// ink coverage already encodes how bright the cell is; colouring it with the
/// raw (dark) colour too would darken the picture twice.
const GLOW: f32 = 0.85;

/// In hybrid style, a cell whose zones differ by more than this counts as an edge.
const EDGE: f32 = 0.14;

/// Dithering spreads flat areas over neighbouring ramp characters, which gives
/// smoother gradients and that busy "wall of code" texture. The pattern is fixed
/// per cell, so still areas don't flicker.
const DITHER: f32 = 0.06;

/// Even the densest character only inks about half its cell, so midtones are
/// lifted to denser characters to keep the picture bright.
const GAMMA: f32 = 0.8;

/// A fixed pseudo-random value in -0.5..0.5 for each cell (integer hash, so no visible repeats).
#[inline(always)]
fn dither_at(row: usize, col: usize) -> f32 {
    let mut h = (row as u32).wrapping_mul(0x9E37_79B1) ^ (col as u32).wrapping_mul(0x85EB_CA77);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^= h >> 15;
    (h >> 8) as f32 / (1u32 << 24) as f32 - 0.5
}

/// Converts `frame` (RGB24, `fw` x `fh`) into `cols` x `rows` cells.
pub fn convert(
    frame: &[u8],
    fw: usize,
    fh: usize,
    cols: usize,
    rows: usize,
    glyphs: &GlyphSet,
    look: &Look,
    out: &mut Vec<Cell>,
) {
    out.clear();
    out.resize(cols * rows, Cell::default());
    if fw == 0 || fh == 0 || cols == 0 || rows == 0 || frame.len() < fw * fh * 3 {
        return;
    }

    out.par_chunks_mut(cols).enumerate().for_each(|(row, cells)| {
        let y0 = (row * fh / rows).min(fh - 1);
        let y1 = ((row + 1) * fh / rows).clamp(y0 + 1, fh);
        let ch = y1 - y0;

        for (col, cell) in cells.iter_mut().enumerate() {
            let x0 = (col * fw / cols).min(fw - 1);
            let x1 = ((col + 1) * fw / cols).clamp(x0 + 1, fw);
            let cw = x1 - x0;
            // First pixel of the right-hand zone column; same split as `zone_col`.
            let split = cw.div_ceil(2);

            let mut lum = [0u32; ZONES];
            let mut cnt = [0u32; ZONES];
            let (mut sr, mut sg, mut sb) = (0u32, 0u32, 0u32);

            for y in y0..y1 {
                let band = zone_row(y - y0, ch);
                let start = (y * fw + x0) * 3;
                for (dx, p) in frame[start..start + cw * 3].as_chunks::<3>().0.iter().enumerate() {
                    let (r, g, b) = (p[0] as u32, p[1] as u32, p[2] as u32);
                    let k = band + (dx >= split) as usize;
                    lum[k] += (r * 54 + g * 183 + b * 19) >> 8;
                    cnt[k] += 1;
                    sr += r;
                    sg += g;
                    sb += b;
                }
            }

            *cell = shade(&lum, &cnt, [sr, sg, sb], dither_at(row, col), glyphs, look);
        }
    });
}

fn shade(
    lum: &[u32; ZONES],
    cnt: &[u32; ZONES],
    rgb_sum: [u32; 3],
    dither: f32,
    glyphs: &GlyphSet,
    look: &Look,
) -> Cell {
    let n = cnt.iter().sum::<u32>().max(1) as f32;
    let mean = lum.iter().sum::<u32>() as f32 / (n * 255.0);
    let tone = |v: f32| (v * look.brightness).min(1.0).powf(GAMMA);
    let light = tone(mean);

    let mut zones: Shape = std::array::from_fn(|k| {
        tone(if cnt[k] > 0 { lum[k] as f32 / (cnt[k] as f32 * 255.0) } else { mean })
    });
    let peak = zones.iter().copied().fold(0.0f32, f32::max);
    let low = zones.iter().copied().fold(1.0f32, f32::min);

    let use_shape = match look.style {
        Style::Shape => true,
        Style::Ramp => false,
        Style::Hybrid => peak - low > EDGE,
    };
    let glyph = if use_shape {
        if peak > 1e-3 && look.contrast != 1.0 {
            for z in &mut zones {
                *z = (*z / peak).powf(look.contrast) * peak;
            }
        }
        glyphs.best_match(&zones)
    } else {
        glyphs.ramp(light + dither * DITHER)
    };

    let [r, g, b] = rgb_sum.map(|s| s as f32 / n);
    let (fg, bg) = colorize(r, g, b, light, look);
    Cell { glyph, fg, bg }
}

fn colorize(r: f32, g: f32, b: f32, light: f32, look: &Look) -> (u32, u32) {
    match look.mode {
        ColorMode::Color | ColorMode::ColorBg => {
            let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
            let s = look.saturation;
            let (r, g, b) = (
                (y + (r - y) * s).max(0.0),
                (y + (g - y) * s).max(0.0),
                (y + (b - y) * s).max(0.0),
            );
            let peak = r.max(g).max(b).max(24.0);
            let gain = 1.0 + GLOW * (255.0 / peak - 1.0);
            let fg = pack(r * gain, g * gain, b * gain);
            let bg = if look.mode == ColorMode::ColorBg {
                let k = 0.35 * look.brightness;
                pack(r * k, g * k, b * k)
            } else {
                0
            };
            (fg, bg)
        }
        ColorMode::Matrix => (
            pack(30.0 + 170.0 * light * light, 120.0 + 135.0 * light, 50.0 + 130.0 * light * light),
            0,
        ),
        ColorMode::Amber => (pack(210.0 + 45.0 * light, 120.0 + 90.0 * light, 10.0 + 60.0 * light * light), 0),
        ColorMode::Mono => {
            let v = 150.0 + 105.0 * light;
            (pack(v, v, v), 0)
        }
    }
}

#[inline(always)]
fn pack(r: f32, g: f32, b: f32) -> u32 {
    let c = |v: f32| v.clamp(0.0, 255.0) as u32;
    (c(r) << 16) | (c(g) << 8) | c(b)
}
