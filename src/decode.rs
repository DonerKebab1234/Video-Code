//! Decodes video frames with an ffmpeg child process on a background thread.

use std::{
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        mpsc::{sync_channel, Receiver},
        Arc, Mutex,
    },
    thread,
};

use anyhow::Result;

use crate::probe::{missing_tool, Fps};

pub struct Frame {
    /// Presentation time in seconds from the start of the file.
    pub pts: f64,
    /// RGB24 pixels.
    pub data: Vec<u8>,
}

/// A running ffmpeg that decodes `w` x `h` RGB frames at a constant frame rate.
/// The channel closes when the video ends. Dropping the decoder stops ffmpeg.
pub struct VideoDecoder {
    pub rx: Receiver<Frame>,
    child: Arc<Mutex<Child>>,
}

impl VideoDecoder {
    pub fn spawn(
        input: &Path,
        start: f64,
        duration: Option<f64>,
        w: usize,
        h: usize,
        fps: &Fps,
        hwaccel: bool,
    ) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if hwaccel {
            cmd.args(["-hwaccel", "auto"]);
        }
        if start > 0.0 {
            cmd.arg("-ss").arg(format!("{start:.3}"));
        }
        if let Some(d) = duration {
            cmd.arg("-t").arg(format!("{d:.3}"));
        }
        cmd.arg("-i")
            .arg(input)
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vf"])
            .arg(format!("fps={},scale={w}:{h}:flags=area", fps.expr))
            .args(["-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let mut child = cmd.spawn().map_err(|e| missing_tool(e, "ffmpeg"))?;
        let mut stdout = child.stdout.take().expect("stdout is piped");
        let child = Arc::new(Mutex::new(child));

        // A few frames of look-ahead; the decoder blocks when the player falls behind.
        let (tx, rx) = sync_channel::<Frame>(3);
        let len = w * h * 3;
        let rate = fps.value;
        let reaper = Arc::clone(&child);
        thread::Builder::new().name("video-decode".into()).spawn(move || {
            let mut n: u64 = 0;
            loop {
                let mut data = vec![0u8; len];
                if stdout.read_exact(&mut data).is_err() {
                    break;
                }
                let frame = Frame { pts: start + n as f64 / rate, data };
                if tx.send(frame).is_err() {
                    break; // player dropped us
                }
                n += 1;
            }
            if let Ok(mut c) = reaper.lock() {
                let _ = c.kill();
                let _ = c.wait();
            }
        })?;

        Ok(Self { rx, child })
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
    }
}
