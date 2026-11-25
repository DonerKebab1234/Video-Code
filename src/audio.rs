//! Sound output. ffmpeg decodes the audio track to f32 samples, a cpal stream
//! plays them, and the number of samples played is the master clock that the
//! video follows.

use std::{
    collections::VecDeque,
    io::Read,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    FromSample, Sample, SampleFormat, SizedSample, StreamConfig,
};

use crate::probe::missing_tool;

struct Queue {
    samples: VecDeque<f32>,
    /// Bumped on every seek, so feeders from an old position stop pushing.
    generation: u64,
    /// Frames (samples per channel) played since the last seek.
    played: u64,
    /// The feeder for the current generation has finished.
    eof: bool,
    /// When the last callback that played real audio ran, and how long that chunk lasts.
    last_callback: Option<Instant>,
    callback_secs: f64,
}

struct Shared {
    queue: Mutex<Queue>,
    paused: AtomicBool,
    volume: AtomicU32,
}

pub struct AudioOut {
    shared: Arc<Shared>,
    rate: u32,
    channels: u16,
    /// Dropping this stops the audio thread (and the stream it owns).
    _keepalive: mpsc::Sender<()>,
}

impl AudioOut {
    /// Opens the default output device. The stream lives on its own thread so its
    /// COM setup on Windows can't clash with the window's.
    pub fn open() -> Result<Self> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                samples: VecDeque::new(),
                generation: 0,
                played: 0,
                eof: true,
                last_callback: None,
                callback_secs: 0.0,
            }),
            paused: AtomicBool::new(true),
            volume: AtomicU32::new(1.0f32.to_bits()),
        });

        let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, u16)>>();
        let (keepalive, stop_rx) = mpsc::channel::<()>();
        let thread_shared = Arc::clone(&shared);
        thread::Builder::new().name("audio-out".into()).spawn(move || match build_stream(thread_shared) {
            Ok((stream, rate, channels)) => {
                let _ = ready_tx.send(Ok((rate, channels)));
                let _ = stop_rx.recv(); // park until AudioOut is dropped
                drop(stream);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        })?;

        let (rate, channels) = ready_rx.recv().map_err(|_| anyhow!("audio thread died"))??;
        Ok(Self { shared, rate, channels, _keepalive: keepalive })
    }

    pub fn set_paused(&self, paused: bool) {
        self.shared.paused.store(paused, Ordering::Relaxed);
    }

    pub fn set_volume(&self, volume: f32) {
        self.shared.volume.store(volume.to_bits(), Ordering::Relaxed);
    }

    /// Seconds played since the last `play`, and whether the track has run out.
    pub fn position(&self) -> (f64, bool) {
        let q = self.shared.queue.lock().unwrap();
        let mut secs = q.played as f64 / self.rate as f64;
        if !self.shared.paused.load(Ordering::Relaxed) {
            if let Some(t) = q.last_callback {
                secs += t.elapsed().as_secs_f64().min(q.callback_secs);
            }
        }
        (secs, q.eof && q.samples.is_empty())
    }

    /// Stops whatever is playing and leaves the output silent.
    pub fn clear(&self) {
        let mut q = self.shared.queue.lock().unwrap();
        q.generation += 1;
        q.samples.clear();
        q.played = 0;
        q.eof = true;
        q.last_callback = None;
    }

    /// Starts decoding `input`'s audio from `start` seconds.
    pub fn play(&self, input: &Path, start: f64, duration: Option<f64>) {
        let generation = {
            let mut q = self.shared.queue.lock().unwrap();
            q.generation += 1;
            q.samples.clear();
            q.played = 0;
            q.eof = false;
            q.last_callback = None;
            q.generation
        };

        let mut cmd = Command::new("ffmpeg");
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
        if start > 0.0 {
            cmd.arg("-ss").arg(format!("{start:.3}"));
        }
        if let Some(d) = duration {
            cmd.arg("-t").arg(format!("{d:.3}"));
        }
        cmd.arg("-i")
            .arg(input)
            .args(["-map", "0:a:0", "-vn", "-sn", "-dn"])
            .arg("-ac")
            .arg(self.channels.to_string())
            .arg("-ar")
            .arg(self.rate.to_string())
            .args(["-f", "f32le", "pipe:1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("warning: no sound: {:#}", missing_tool(e, "ffmpeg"));
                self.shared.queue.lock().unwrap().eof = true;
                return;
            }
        };
        let shared = Arc::clone(&self.shared);
        let max_buffered = self.rate as usize * self.channels as usize; // ~1 second
        let _ = thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || feed(shared, generation, child, max_buffered));
    }
}

fn feed(shared: Arc<Shared>, generation: u64, mut child: Child, max_buffered: usize) {
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut bytes = vec![0u8; 32 * 1024];
    let mut carry: Vec<u8> = Vec::with_capacity(4);
    let mut samples: Vec<f32> = Vec::with_capacity(bytes.len() / 4 + 1);

    'read: loop {
        let n = match stdout.read(&mut bytes) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        samples.clear();
        let mut data = &bytes[..n];
        // Finish a sample split across two reads.
        if !carry.is_empty() {
            let take = (4 - carry.len()).min(data.len());
            carry.extend_from_slice(&data[..take]);
            data = &data[take..];
            if carry.len() == 4 {
                samples.push(f32::from_le_bytes([carry[0], carry[1], carry[2], carry[3]]));
                carry.clear();
            }
        }
        let (whole, rest) = data.as_chunks::<4>();
        carry.extend_from_slice(rest);
        samples.extend(whole.iter().map(|c| f32::from_le_bytes(*c)));

        loop {
            let mut q = shared.queue.lock().unwrap();
            if q.generation != generation {
                break 'read;
            }
            if q.samples.len() < max_buffered {
                q.samples.extend(samples.iter().copied());
                break;
            }
            drop(q);
            thread::sleep(Duration::from_millis(5));
        }
    }

    {
        let mut q = shared.queue.lock().unwrap();
        if q.generation == generation {
            q.eof = true;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn build_stream(shared: Arc<Shared>) -> Result<(cpal::Stream, u32, u16)> {
    let device = cpal::default_host()
        .default_output_device()
        .context("no audio output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let (rate, channels) = (config.sample_rate.0, config.channels);

    let stream = match format {
        SampleFormat::F32 => make_stream::<f32>(&device, &config, shared)?,
        SampleFormat::I16 => make_stream::<i16>(&device, &config, shared)?,
        SampleFormat::U16 => make_stream::<u16>(&device, &config, shared)?,
        SampleFormat::I32 => make_stream::<i32>(&device, &config, shared)?,
        SampleFormat::F64 => make_stream::<f64>(&device, &config, shared)?,
        other => bail!("unsupported audio sample format {other:?}"),
    };
    stream.play()?;
    Ok((stream, rate, channels))
}

fn make_stream<T>(device: &cpal::Device, config: &StreamConfig, shared: Arc<Shared>) -> Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate.0 as f64;
    let stream = device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| write_samples(out, channels, rate, &shared),
        |e| eprintln!("audio error: {e}"),
        None,
    )?;
    Ok(stream)
}

fn write_samples<T: Sample + FromSample<f32>>(out: &mut [T], channels: usize, rate: f64, shared: &Shared) {
    let mut q = shared.queue.lock().unwrap();
    if shared.paused.load(Ordering::Relaxed) {
        out.fill(T::EQUILIBRIUM);
        q.last_callback = None;
        return;
    }
    let volume = f32::from_bits(shared.volume.load(Ordering::Relaxed));
    let frames = (out.len() / channels).min(q.samples.len() / channels);
    let n = frames * channels;
    for (dst, src) in out[..n].iter_mut().zip(q.samples.drain(..n)) {
        *dst = T::from_sample(src * volume);
    }
    out[n..].fill(T::EQUILIBRIUM);
    q.played += frames as u64;
    q.last_callback = (frames > 0).then(Instant::now);
    q.callback_secs = frames as f64 / rate;
}
