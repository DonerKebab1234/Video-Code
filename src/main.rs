//! video-code: play any video as live, full-colour ASCII art.
//!
//! Pipeline: ffmpeg decodes frames -> every character cell is split into six
//! zones -> the character whose shape matches those zones best is picked and
//! tinted with the cell's colour -> the characters are rasterised into a
//! window (or piped back into ffmpeg when exporting).
//!
//! With `--web` it serves a browser interface instead (see web.rs).

// Pixel-pushing helpers naturally take (buffer, width, height, x, y, ...).
#![allow(clippy::too_many_arguments)]

mod ascii;
mod audio;
mod cli;
mod decode;
mod export;
mod font;
mod player;
mod probe;
mod render;
mod web;

use anyhow::{Context, Result};
use clap::Parser;

fn main() -> Result<()> {
    let args = cli::Args::parse();
    if args.web {
        return web::run(args);
    }
    match &args.export {
        Some(output) => {
            let input = args
                .input
                .as_deref()
                .context("--export needs a video to read from, e.g. `video-code clip.mp4 -o ascii.mp4`")?;
            export::run(&args, input, output)
        }
        None => player::run(args),
    }
}
