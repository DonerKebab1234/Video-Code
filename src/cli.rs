use std::path::PathBuf;

use clap::Parser;

use crate::ascii::{ColorMode, Style};

/// Play any video as live, full-colour ASCII art.
///
/// Run it with a video file, or start it without one and drop a file onto the window.
#[derive(Parser, Debug, Clone)]
#[command(name = "video-code", version, about)]
pub struct Args {
    /// Video to play: any file (or URL) ffmpeg can open.
    pub input: Option<PathBuf>,

    /// Character height in pixels. Smaller means more characters and more detail.
    #[arg(short = 's', long, default_value_t = 12.0, value_name = "PX")]
    pub font_size: f32,

    /// Font file (.ttf / .otf) to draw with. Defaults to a monospace system font.
    #[arg(long, value_name = "FILE")]
    pub font: Option<PathBuf>,

    /// Characters to draw with, e.g. " .:-=+*#%@". Defaults to every printable ASCII character.
    #[arg(long, value_name = "CHARS")]
    pub charset: Option<String>,

    /// Colour style.
    #[arg(short, long, value_enum, default_value_t = ColorMode::Color)]
    pub color: ColorMode,

    /// How characters are picked.
    #[arg(long, value_enum, default_value_t = Style::Hybrid)]
    pub style: Style,

    /// How hard edges are sharpened when matching character shapes.
    #[arg(long, default_value_t = 2.0, value_name = "N")]
    pub contrast: f32,

    /// Brightness multiplier.
    #[arg(long, default_value_t = 1.0, value_name = "N")]
    pub brightness: f32,

    /// Colour saturation multiplier.
    #[arg(long, default_value_t = 1.3, value_name = "N")]
    pub saturation: f32,

    /// Start this many seconds into the video.
    #[arg(long, default_value_t = 0.0, value_name = "SECONDS")]
    pub start: f64,

    /// Start over when the video ends.
    #[arg(short = 'l', long = "loop")]
    pub looping: bool,

    /// Start with the sound muted.
    #[arg(long)]
    pub mute: bool,

    /// Don't use the GPU to decode video.
    #[arg(long)]
    pub no_hwaccel: bool,

    /// Decode frames at most this big (longest side, px) before turning them into characters.
    #[arg(long, default_value_t = 1280, value_name = "PX")]
    pub decode_size: usize,

    /// Render to a file instead of opening a window, e.g. ascii.mp4 (video) or frame.png (one image).
    #[arg(short = 'o', long, value_name = "FILE")]
    pub export: Option<PathBuf>,

    /// Export: longest side of the output in pixels.
    #[arg(long, default_value_t = 1920, value_name = "PX")]
    pub size: usize,

    /// Export: only this many seconds.
    #[arg(short = 't', long, value_name = "SECONDS")]
    pub duration: Option<f64>,

    /// Export: x264 quality, lower is better (18 looks lossless).
    #[arg(long, default_value_t = 18, value_name = "N")]
    pub crf: u32,

    /// Open the browser interface instead of a window.
    #[arg(long, conflicts_with_all = ["input", "export"])]
    pub web: bool,

    /// Browser interface: port to listen on (if it's taken, a free one is picked).
    #[arg(long, default_value_t = 7878, value_name = "PORT", requires = "web")]
    pub port: u16,

    /// Browser interface: don't open the browser automatically.
    #[arg(long, requires = "web")]
    pub no_browser: bool,
}

impl Args {
    pub fn charset(&self) -> Vec<char> {
        match self.charset.as_deref() {
            Some(s) if !s.is_empty() => s.chars().collect(),
            _ => crate::font::ascii_charset(),
        }
    }

    pub fn look(&self) -> crate::ascii::Look {
        crate::ascii::Look {
            mode: self.color,
            style: self.style,
            contrast: self.contrast,
            brightness: self.brightness,
            saturation: self.saturation,
        }
    }
}
