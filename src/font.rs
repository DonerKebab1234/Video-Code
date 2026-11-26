//! Rasterises a monospace font into fixed-size character cells and measures
//! each character's "shape": how much ink it has in six zones of its cell.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, bail, Context, Result};
use fontdue::{Font, FontSettings};

/// Zones per cell: 2 columns x 3 rows.
pub const ZONES: usize = 6;

pub type Shape = [f32; ZONES];

/// Which zone (0..6) a pixel at (x, y) inside a w x h cell belongs to.
/// Used both for glyphs and for video pixels so the two line up.
#[inline(always)]
pub fn zone_row(y: usize, h: usize) -> usize {
    (y * 3 / h).min(2) * 2
}

#[inline(always)]
pub fn zone_col(x: usize, w: usize) -> usize {
    (x * 2 / w).min(1)
}

pub struct GlyphSet {
    pub cell_w: usize,
    pub cell_h: usize,
    /// One coverage bitmap (0..255) of `cell_w * cell_h` per glyph.
    bitmaps: Vec<u8>,
    /// Shape vectors, normalised so the densest zone of any glyph is 1.0.
    shapes: Vec<Shape>,
    /// Brightness (0..255) -> the glyph whose overall ink coverage matches it best.
    ramp: Vec<u16>,
    lookup: HashMap<char, u16>,
}

impl GlyphSet {
    pub fn new(font: &Font, px: f32, charset: &[char]) -> Self {
        let px = px.max(3.0);
        let (ascent, descent) = font
            .horizontal_line_metrics(px)
            .map(|m| (m.ascent, m.descent))
            .unwrap_or((px * 0.8, -px * 0.2));
        let cell_h = ((ascent - descent).round() as usize).max(2);
        let cell_w = (font.metrics('M', px).advance_width.round() as usize).max(1);
        let baseline = ascent.round() as i32;
        let area = cell_w * cell_h;

        let mut bitmaps = Vec::new();
        let mut shapes = Vec::new();
        let mut lookup = HashMap::new();

        for &c in charset {
            if lookup.contains_key(&c) || (c != ' ' && font.lookup_glyph_index(c) == 0) {
                continue;
            }
            let (m, bm) = font.rasterize(c, px);
            let mut cell = vec![0u8; area];
            let top = baseline - (m.ymin + m.height as i32);
            for gy in 0..m.height {
                let y = top + gy as i32;
                if y < 0 || y >= cell_h as i32 {
                    continue;
                }
                for gx in 0..m.width {
                    let x = m.xmin + gx as i32;
                    if x < 0 || x >= cell_w as i32 {
                        continue;
                    }
                    cell[y as usize * cell_w + x as usize] = bm[gy * m.width + gx];
                }
            }
            lookup.insert(c, shapes.len() as u16);
            shapes.push(shape_of(&cell, cell_w, cell_h));
            bitmaps.extend_from_slice(&cell);
        }

        if shapes.is_empty() {
            lookup.insert(' ', 0);
            shapes.push([0.0; ZONES]);
            bitmaps.resize(area, 0);
        }

        let max = shapes.iter().flatten().copied().fold(0.0f32, f32::max);
        if max > 0.0 {
            for v in shapes.iter_mut().flatten() {
                *v /= max;
            }
        }

        // Classic ASCII ramp, calibrated to this font: sort by how much ink each glyph has.
        let density: Vec<f32> = shapes.iter().map(|s| s.iter().sum::<f32>() / ZONES as f32).collect();
        let densest = density.iter().copied().fold(0.0f32, f32::max).max(1e-6);
        let ramp = (0..256)
            .map(|b| {
                let target = b as f32 / 255.0 * densest;
                let mut best = 0;
                for (i, d) in density.iter().enumerate() {
                    if (d - target).abs() < (density[best] - target).abs() {
                        best = i;
                    }
                }
                best as u16
            })
            .collect();

        Self { cell_w, cell_h, bitmaps, shapes, ramp, lookup }
    }

    /// The glyph whose ink coverage best matches brightness `light` (0..1).
    #[inline(always)]
    pub fn ramp(&self, light: f32) -> u16 {
        self.ramp[(light.clamp(0.0, 1.0) * 255.0).round() as usize]
    }

    #[inline(always)]
    pub fn bitmap(&self, index: usize) -> &[u8] {
        let n = self.cell_w * self.cell_h;
        &self.bitmaps[index * n..(index + 1) * n]
    }

    pub fn index_of(&self, c: char) -> Option<usize> {
        self.lookup.get(&c).map(|&i| i as usize)
    }

    /// Packs everything the browser needs to pick and draw the same characters,
    /// little-endian: `cell_w`, `cell_h`, `count` (u32 each), the 256-entry ramp
    /// (u16), `count` x 6 shape values (f32), then `count` coverage bitmaps (u8).
    pub fn to_bytes(&self) -> Vec<u8> {
        let count = self.shapes.len();
        let mut out = Vec::with_capacity(12 + 512 + count * ZONES * 4 + self.bitmaps.len());
        for v in [self.cell_w, self.cell_h, count] {
            out.extend_from_slice(&(v as u32).to_le_bytes());
        }
        for v in &self.ramp {
            out.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.shapes.iter().flatten() {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.bitmaps);
        out
    }

    /// The glyph whose shape is closest to `target` (squared distance).
    #[inline]
    pub fn best_match(&self, target: &Shape) -> u16 {
        let mut best = 0;
        let mut best_d = f32::MAX;
        for (i, s) in self.shapes.iter().enumerate() {
            let mut d = 0.0;
            for k in 0..ZONES {
                let e = s[k] - target[k];
                d += e * e;
            }
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best as u16
    }
}

fn shape_of(cell: &[u8], w: usize, h: usize) -> Shape {
    let mut sum = [0f32; ZONES];
    let mut count = [0f32; ZONES];
    for y in 0..h {
        let row = zone_row(y, h);
        for x in 0..w {
            let k = row + zone_col(x, w);
            sum[k] += cell[y * w + x] as f32 / 255.0;
            count[k] += 1.0;
        }
    }
    std::array::from_fn(|k| if count[k] > 0.0 { sum[k] / count[k] } else { 0.0 })
}

/// Every printable ASCII character, space included.
pub fn ascii_charset() -> Vec<char> {
    (32u8..=126).map(char::from).collect()
}

/// ASCII plus Latin-1 letters, for on-screen text such as file names.
pub fn ui_charset() -> Vec<char> {
    let mut v = ascii_charset();
    v.extend((0xA1u32..=0xFF).filter_map(char::from_u32));
    v
}

pub fn load_font(path: Option<&Path>) -> Result<Font> {
    if let Some(p) = path {
        let bytes = std::fs::read(p).with_context(|| format!("couldn't read font {}", p.display()))?;
        return Font::from_bytes(bytes, FontSettings::default())
            .map_err(|e| anyhow!("couldn't load font {}: {e}", p.display()));
    }
    for p in system_fonts() {
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(font) = Font::from_bytes(bytes, FontSettings::default()) {
                return Ok(font);
            }
        }
    }
    bail!("couldn't find a monospace font on this system; pass one with --font <file.ttf>")
}

fn system_fonts() -> Vec<PathBuf> {
    if cfg!(windows) {
        let windir = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        ["consolab.ttf", "consola.ttf", "CascadiaMono.ttf", "lucon.ttf", "cour.ttf"]
            .iter()
            .map(|f| windir.join("Fonts").join(f))
            .collect()
    } else if cfg!(target_os = "macos") {
        ["/System/Library/Fonts/Menlo.ttc", "/System/Library/Fonts/Monaco.ttf"]
            .iter()
            .map(PathBuf::from)
            .collect()
    } else {
        [
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono-Bold.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/liberation-mono/LiberationMono-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
            "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
        ]
        .iter()
        .map(PathBuf::from)
        .collect()
    }
}
