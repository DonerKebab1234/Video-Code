//! Headless mode: renders the ASCII version of a video into a new video file
//! (with the original sound), or a single frame into an image.

use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};

use crate::{
    cli::Args,
    decode::VideoDecoder,
    font::{self, GlyphSet},
    probe::{self, missing_tool, MediaInfo},
    render::{Grid, Renderer},
};

/// ffmpeg's name for our in-memory pixel layout (0x00RRGGBB as u32).
const PIX_FMT: &str = if cfg!(target_endian = "little") { "bgr0" } else { "0rgb" };

pub fn run(args: &Args, input: &Path, output: &Path) -> Result<()> {
    let info = probe::probe(input)?;
    let font = font::load_font(args.font.as_deref())?;
    let glyphs = GlyphSet::new(&font, args.font_size, &args.charset());
    let (cw, ch) = (glyphs.cell_w, glyphs.cell_h);
    let (dw, dh) = info.decode_size(args.decode_size);

    // Fit the longest side to --size, then snap to whole characters.
    let aspect = dw as f64 / dh as f64;
    let size = args.size.max(64) as f64;
    let target_w = if aspect >= 1.0 { size } else { size * aspect };
    let cols = ((target_w / cw as f64).round() as usize).max(1);
    let rows = ((cols as f64 * cw as f64 / aspect / ch as f64).round() as usize).max(1);
    let grid = Grid { cols, rows, off_x: 0, off_y: 0 };
    // yuv420p needs even dimensions; any spare pixel stays black.
    let (out_w, out_h) = (even(cols * cw), even(rows * ch));

    let image = is_image(output);
    let duration = if image { None } else { args.duration };
    let decoder = VideoDecoder::spawn(input, args.start, duration, dw, dh, &info.fps, !args.no_hwaccel)?;

    let mut encoder = encoder_command(args, input, output, out_w, out_h, &info, image)
        .spawn()
        .map_err(|e| missing_tool(e, "ffmpeg"))?;
    let mut stdin = encoder.stdin.take().context("ffmpeg stdin")?;

    let expected = if image {
        Some(1)
    } else {
        duration
            .or(info.duration.map(|d| (d - args.start).max(0.0)))
            .map(|d| (d * info.fps.value).round().max(1.0) as u64)
    };

    eprintln!(
        "Exporting {} -> {}\n  {}x{} px, {}x{} characters, {:.2} fps",
        input.display(),
        output.display(),
        out_w,
        out_h,
        cols,
        rows,
        info.fps.value
    );

    let look = args.look();
    let mut renderer = Renderer::new(glyphs);
    let mut buf = vec![0u32; out_w * out_h];
    let started = Instant::now();
    let mut last_report = started;
    let mut frames = 0u64;
    let mut write_error = None;

    for frame in decoder.rx.iter() {
        renderer.draw_grid(&frame.data, dw, dh, &look, grid, &mut buf, out_w, out_h);
        if let Err(e) = stdin.write_all(as_bytes(&buf)) {
            write_error = Some(e);
            break;
        }
        frames += 1;
        if image {
            break;
        }
        if last_report.elapsed() >= Duration::from_millis(250) {
            last_report = Instant::now();
            report(frames, expected, started);
        }
    }

    drop(stdin);
    drop(decoder);
    let status = encoder.wait()?;
    if !status.success() || write_error.is_some() {
        bail!("ffmpeg failed while writing {} (see its message above)", output.display());
    }
    if frames == 0 {
        bail!("no frames were decoded from {}", input.display());
    }
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

fn encoder_command(
    args: &Args,
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
        // Second input: the original file, for its sound.
        if args.start > 0.0 {
            cmd.arg("-ss").arg(format!("{:.3}", args.start));
        }
        if let Some(d) = args.duration {
            cmd.arg("-t").arg(format!("{d:.3}"));
        }
        cmd.arg("-i").arg(input).args(["-map", "0:v:0", "-map", "1:a:0?"]);

        let crf = args.crf.to_string();
        match extension(output).as_str() {
            "webm" => {
                cmd.args(["-c:v", "libvpx-vp9", "-b:v", "0", "-crf"])
                    .arg((args.crf + 13).to_string())
                    .args(["-row-mt", "1", "-c:a", "libopus"]);
            }
            "gif" => {
                cmd.args(["-vf", "split[a][b];[a]palettegen[p];[b][p]paletteuse"]);
            }
            ext => {
                cmd.args(["-c:v", "libx264", "-preset", "medium", "-crf", &crf, "-pix_fmt", "yuv420p"])
                    .args(["-c:a", "aac", "-b:a", "192k"]);
                if matches!(ext, "mp4" | "mov" | "m4v") {
                    cmd.args(["-movflags", "+faststart"]);
                }
            }
        }
        cmd.arg("-shortest");
    }

    cmd.arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
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

fn as_bytes(pixels: &[u32]) -> &[u8] {
    // SAFETY: u32 has no padding and any byte pattern is a valid u8.
    unsafe { std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), std::mem::size_of_val(pixels)) }
}

fn even(x: usize) -> usize {
    x + (x & 1)
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

fn is_image(path: &Path) -> bool {
    matches!(
        extension(path).as_str(),
        "png" | "jpg" | "jpeg" | "bmp" | "webp" | "tif" | "tiff"
    )
}
