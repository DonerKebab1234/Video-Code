//! Headless mode: renders the ASCII version of a video into a new video file
//! (with the original sound), or a single frame into an image.

use std::{
    fmt,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use fontdue::Font;

use crate::{
    ascii::Look,
    cli::Args,
    decode::VideoDecoder,
    font::{self, GlyphSet},
    probe::{self, missing_tool, MediaInfo},
    render::{Grid, Renderer},
};

/// ffmpeg's name for our in-memory pixel layout (0x00RRGGBB as u32).
const PIX_FMT: &str = if cfg!(target_endian = "little") { "bgr0" } else { "0rgb" };

/// Everything that decides what an export looks like.
pub struct Settings<'a> {
    pub font: &'a Font,
    pub font_size: f32,
    pub charset: &'a [char],
    pub look: Look,
    /// Seconds into the video to start at.
    pub start: f64,
    /// Seconds to export; `None` means to the end.
    pub duration: Option<f64>,
    /// Longest side of the output in pixels.
    pub size: usize,
    /// x264 quality, lower is better.
    pub crf: u32,
    pub decode_size: usize,
    pub hwaccel: bool,
}

/// What an export is about to make. Reported once, before the first frame.
pub struct Plan {
    pub width: usize,
    pub height: usize,
    pub cols: usize,
    pub rows: usize,
    pub fps: f64,
    /// Expected number of frames, if the video's length is known.
    pub total: Option<u64>,
}

pub enum Event<'a> {
    Start(&'a Plan),
    Frame { frames: u64, total: Option<u64> },
}

/// Returned by [`render`] when the cancel flag was set.
#[derive(Debug)]
pub struct Cancelled;

impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("export cancelled")
    }
}

impl std::error::Error for Cancelled {}

/// `video-code clip.mp4 -o out.mp4`: exports with the command-line settings and prints progress.
pub fn run(args: &Args, input: &Path, output: &Path) -> Result<()> {
    let font = font::load_font(args.font.as_deref())?;
    let charset = args.charset();
    let settings = Settings {
        font: &font,
        font_size: args.font_size,
        charset: &charset,
        look: args.look(),
        start: args.start,
        duration: args.duration,
        size: args.size,
        crf: args.crf,
        decode_size: args.decode_size,
        hwaccel: !args.no_hwaccel,
    };

    let image = is_image(output);
    let started = Instant::now();
    let mut last_report = started;
    let mut expected = None;
    let never = AtomicBool::new(false);
    let frames = render(input, output, &settings, &never, |event| match event {
        Event::Start(plan) => {
            expected = plan.total;
            eprintln!(
                "Exporting {} -> {}\n  {}x{} px, {}x{} characters, {:.2} fps",
                input.display(),
                output.display(),
                plan.width,
                plan.height,
                plan.cols,
                plan.rows,
                plan.fps
            );
        }
        Event::Frame { frames, total } => {
            if !image && last_report.elapsed() >= Duration::from_millis(250) {
                last_report = Instant::now();
                report(frames, total, started);
            }
        }
    })?;

    if !image {
        report(frames, expected, started);
    }
    eprintln!(
        "\nDone: {} frame{} in {:.1}s -> {}",
        frames,
        if frames == 1 { "" } else { "s" },
        started.elapsed().as_secs_f64(),
        output.display()
    );
    Ok(())
}

/// Renders `input` into `output` and returns the number of frames written.
/// The output type follows its extension (.mp4, .webm, .gif, .png, ...).
pub fn render(
    input: &Path,
    output: &Path,
    settings: &Settings,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(Event),
) -> Result<u64> {
    let info = probe::probe(input)?;
    let glyphs = GlyphSet::new(settings.font, settings.font_size, settings.charset);
    let (cw, ch) = (glyphs.cell_w, glyphs.cell_h);
    let (dw, dh) = info.decode_size(settings.decode_size);

    // Fit the longest side to `size`, then snap to whole characters.
    let (cols, rows) = output_grid(dw as f64 / dh as f64, settings.size, cw, ch);
    let grid = Grid { cols, rows, off_x: 0, off_y: 0 };
    // yuv420p needs even dimensions; any spare pixel stays black.
    let (out_w, out_h) = (even(cols * cw), even(rows * ch));

    let image = is_image(output);
    let duration = if image { None } else { settings.duration };
    let decoder = VideoDecoder::spawn(input, settings.start, duration, dw, dh, &info.fps, settings.hwaccel)?;

    let mut encoder = encoder_command(settings, input, output, out_w, out_h, &info, image)
        .spawn()
        .map_err(|e| missing_tool(e, "ffmpeg"))?;
    let mut stdin = encoder.stdin.take().context("ffmpeg stdin")?;
    // Collect ffmpeg's error messages so they can go into our own error.
    let stderr = encoder.stderr.take();
    let messages = thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut text);
        }
        text
    });

    let total = expected_frames(settings, &info, image);
    on_event(Event::Start(&Plan { width: out_w, height: out_h, cols, rows, fps: info.fps.value, total }));

    let mut renderer = Renderer::new(glyphs);
    let mut buf = vec![0u32; out_w * out_h];
    let mut frames = 0u64;
    let mut write_error = None;
    let mut cancelled = false;

    for frame in decoder.rx.iter() {
        if cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        renderer.draw_grid(&frame.data, dw, dh, &settings.look, grid, &mut buf, out_w, out_h);
        if let Err(e) = stdin.write_all(as_bytes(&buf)) {
            write_error = Some(e);
            break;
        }
        frames += 1;
        on_event(Event::Frame { frames, total });
        if image {
            break;
        }
    }

    drop(stdin);
    drop(decoder);
    if cancelled {
        let _ = encoder.kill();
        let _ = encoder.wait();
        let _ = std::fs::remove_file(output);
        return Err(Cancelled.into());
    }
    let status = encoder.wait()?;
    let messages = messages.join().unwrap_or_default();
    if !status.success() || write_error.is_some() {
        let detail = last_lines(&messages);
        if detail.is_empty() {
            bail!("ffmpeg failed while writing {}", output.display());
        }
        bail!("ffmpeg failed while writing {}: {detail}", output.display());
    }
    if frames == 0 {
        bail!("no frames were decoded from {}", input.display());
    }
    Ok(frames)
}

/// Character grid of an export: the longest side fits `size` pixels.
pub fn output_grid(aspect: f64, size: usize, cw: usize, ch: usize) -> (usize, usize) {
    let size = size.max(64) as f64;
    let target_w = if aspect >= 1.0 { size } else { size * aspect };
    let cols = ((target_w / cw as f64).round() as usize).max(1);
    let rows = ((cols as f64 * cw as f64 / aspect / ch as f64).round() as usize).max(1);
    (cols, rows)
}

fn expected_frames(settings: &Settings, info: &MediaInfo, image: bool) -> Option<u64> {
    if image {
        return Some(1);
    }
    settings
        .duration
        .or(info.duration.map(|d| (d - settings.start).max(0.0)))
        .map(|d| (d * info.fps.value).round().max(1.0) as u64)
}

fn encoder_command(
    settings: &Settings,
    input: &Path,
    output: &Path,
    w: usize,
    h: usize,
    info: &MediaInfo,
    image: bool,
) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-hide_banner", "-loglevel", "error", "-y"])
        .args(["-f", "rawvideo", "-pix_fmt", PIX_FMT])
        .arg("-s")
        .arg(format!("{w}x{h}"))
        .arg("-framerate")
        .arg(&info.fps.expr)
        .args(["-i", "pipe:0"]);

    if image {
        cmd.args(["-frames:v", "1", "-update", "1"]);
    } else {
        let ext = extension(output);
        if ext == "gif" {
            // GIFs can't hold sound, so the original file isn't needed.
            cmd.args(["-vf", "split[a][b];[a]palettegen[p];[b][p]paletteuse"]);
        } else {
            // Second input: the original file, for its sound.
            if settings.start > 0.0 {
                cmd.arg("-ss").arg(format!("{:.3}", settings.start));
            }
            if let Some(d) = settings.duration {
                cmd.arg("-t").arg(format!("{d:.3}"));
            }
            cmd.arg("-i").arg(input).args(["-map", "0:v:0", "-map", "1:a:0?"]);

            if ext == "webm" {
                cmd.args(["-c:v", "libvpx-vp9", "-b:v", "0", "-crf"])
                    .arg((settings.crf + 13).to_string())
                    .args(["-row-mt", "1", "-c:a", "libopus"]);
            } else {
                cmd.args(["-c:v", "libx264", "-preset", "medium", "-crf"])
                    .arg(settings.crf.to_string())
                    .args(["-pix_fmt", "yuv420p", "-c:a", "aac", "-b:a", "192k"]);
                if matches!(ext.as_str(), "mp4" | "mov" | "m4v") {
                    cmd.args(["-movflags", "+faststart"]);
                }
            }
            cmd.arg("-shortest");
        }
    }

    cmd.arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

fn report(frames: u64, expected: Option<u64>, started: Instant) {
    let secs = started.elapsed().as_secs_f64().max(1e-6);
    let speed = frames as f64 / secs;
    match expected {
        Some(total) if total > 0 => {
            let pct = (frames as f64 / total as f64 * 100.0).min(100.0);
            let eta = total.saturating_sub(frames) as f64 / speed.max(1e-6);
            eprint!("\r  {pct:5.1}%  frame {frames}/{total}  {speed:.0} fps  ETA {eta:.0}s    ");
        }
        _ => eprint!("\r  frame {frames}  {speed:.0} fps    "),
    }
}

/// The last few lines of a tool's error output, for an error message.
pub fn last_lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(3)..].join(" / ")
}

fn as_bytes(pixels: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and any byte pattern is a valid u8.
    unsafe { std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), std::mem::size_of_val(pixels)) }
}

fn even(x: usize) -> usize {
    x + (x & 1)
}

pub fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_image(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "webp" | "tif" | "tiff"
    )
}
