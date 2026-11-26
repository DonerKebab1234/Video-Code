//! Reads a video's size, frame rate and duration with ffprobe.

use std::{path::Path, process::Command};

use anyhow::{anyhow, bail, Result};

#[derive(Clone, Debug)]
pub struct Fps {
    /// Exact rate as ffmpeg wrote it, e.g. "30000/1001". Passed back to ffmpeg filters.
    pub expr: String,
    pub value: f64,
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    /// Display size, after applying rotation metadata (phone videos).
    pub width: usize,
    pub height: usize,
    pub fps: Fps,
    pub duration: Option<f64>,
    pub has_audio: bool,
    /// Colour matrix to decode with because the file doesn't name one
    /// (see [`untagged_matrix`]). `None` lets ffmpeg use the file's own.
    pub color_matrix: Option<&'static str>,
}

impl MediaInfo {
    /// Size to decode at: the source size, shrunk so the longest side is at most `max_side`.
    pub fn decode_size(&self, max_side: usize) -> (usize, usize) {
        let longest = self.width.max(self.height).max(1) as f64;
        let scale = (max_side.max(16) as f64 / longest).min(1.0);
        let w = ((self.width as f64 * scale).round() as usize).max(1);
        let h = ((self.height as f64 * scale).round() as usize).max(1);
        (w, h)
    }
}

pub fn probe(input: &Path) -> Result<MediaInfo> {
    let video = ffprobe(
        input,
        &[
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height,avg_frame_rate,r_frame_rate,color_space:stream_tags=rotate:stream_side_data=rotation:format=duration",
            "-of",
            "default=noprint_wrappers=1",
        ],
    )?;

    let (mut width, mut height) = (None::<usize>, None::<usize>);
    let (mut avg, mut real) = (None, None);
    let mut duration = None;
    let mut rotation = None::<f64>;
    let mut color_space = None::<String>;
    for line in video.lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "width" => width = value.parse().ok(),
            "height" => height = value.parse().ok(),
            "avg_frame_rate" => avg = parse_rate(value),
            "r_frame_rate" => real = parse_rate(value),
            "duration" => duration = value.parse::<f64>().ok().filter(|d| d.is_finite() && *d > 0.0),
            "rotation" | "TAG:rotate" => rotation = value.parse().ok(),
            "color_space" => color_space = Some(value.to_string()),
            _ => {}
        }
    }

    let (Some(mut width), Some(mut height)) = (width, height) else {
        bail!("{} has no video stream", input.display());
    };
    // Decided on the stored picture, before rotation, like the browsers do.
    let color_matrix = untagged_matrix(color_space.as_deref(), height);
    if rotation.is_some_and(|r| (r.round() as i64).rem_euclid(180) == 90) {
        std::mem::swap(&mut width, &mut height);
    }

    let fps = avg.or(real).unwrap_or(Fps { expr: "30".into(), value: 30.0 });

    let audio = ffprobe(
        input,
        &["-select_streams", "a:0", "-show_entries", "stream=index", "-of", "csv=p=0"],
    )?;

    Ok(MediaInfo {
        width,
        height,
        fps,
        duration,
        has_audio: !audio.trim().is_empty(),
        color_matrix,
    })
}

/// A video without a colour-space tag doesn't say how to turn its YUV pixels
/// into RGB. ffmpeg always guesses BT.601 (the SD standard), which shifts the
/// colours of HD video. Browsers guess by size instead: BT.709 from 720 px
/// tall, BT.601 below (checked in Chrome). We do the same, so HD colours come
/// out right and the web preview matches exports.
fn untagged_matrix(color_space: Option<&str>, coded_height: usize) -> Option<&'static str> {
    let untagged = matches!(color_space, None | Some("" | "unknown" | "unspecified" | "reserved"));
    (untagged && coded_height >= 720).then_some("bt709")
}

fn parse_rate(s: &str) -> Option<Fps> {
    let value = match s.split_once('/') {
        Some((n, d)) => {
            let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
            if d == 0.0 {
                return None;
            }
            n / d
        }
        None => s.parse().ok()?,
    };
    if !(value.is_finite() && value >= 1.0) {
        return None;
    }
    // Some containers report a timebase (1000/1) instead of a frame rate.
    if value > 240.0 {
        return Some(Fps { expr: "60".into(), value: 60.0 });
    }
    Some(Fps { expr: s.to_string(), value })
}

fn ffprobe(input: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error"])
        .args(args)
        .arg(input)
        .output()
        .map_err(|e| missing_tool(e, "ffprobe"))?;
    if !out.status.success() {
        bail!(
            "ffprobe couldn't open {}: {}",
            input.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn missing_tool(e: std::io::Error, tool: &str) -> anyhow::Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        anyhow!(
            "`{tool}` was not found. Install FFmpeg and make sure it is on your PATH \
             (Windows: `winget install Gyan.FFmpeg`, macOS: `brew install ffmpeg`)."
        )
    } else {
        anyhow!(e).context(format!("failed to run {tool}"))
    }
}

#[cfg(test)]
mod tests {
    use super::untagged_matrix;

    #[test]
    fn untagged_hd_is_bt709() {
        assert_eq!(untagged_matrix(Some("unknown"), 720), Some("bt709"));
        assert_eq!(untagged_matrix(None, 1080), Some("bt709"));
        assert_eq!(untagged_matrix(Some("unknown"), 1280), Some("bt709")); // portrait 720x1280
    }

    #[test]
    fn untagged_sd_and_tagged_are_left_alone() {
        assert_eq!(untagged_matrix(Some("unknown"), 716), None);
        assert_eq!(untagged_matrix(Some("unknown"), 576), None);
        assert_eq!(untagged_matrix(Some("bt709"), 1080), None);
        assert_eq!(untagged_matrix(Some("smpte170m"), 1080), None);
        assert_eq!(untagged_matrix(Some("bt2020nc"), 2160), None);
    }
}
