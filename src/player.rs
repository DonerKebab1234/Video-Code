//! The interactive player window.

use std::{
    f32::consts::TAU,
    num::NonZeroU32,
    path::{Path, PathBuf},
    rc::Rc,
    sync::mpsc::TryRecvError,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use fontdue::Font;
use rayon::prelude::*;
use softbuffer::{Context, Surface};
use winit::{
    application::ApplicationHandler,
    dpi::{PhysicalPosition, PhysicalSize},
    event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    keyboard::{Key, NamedKey},
    window::{Fullscreen, Window, WindowId},
};

use crate::{
    ascii::Look,
    audio::AudioOut,
    cli::Args,
    decode::{Frame, VideoDecoder},
    font::{self, GlyphSet},
    probe::{self, MediaInfo},
    render::{self, Renderer, ACCENT, GRAY, WHITE},
};

const HUD_TIME: Duration = Duration::from_millis(2500);
const TOAST_TIME: Duration = Duration::from_millis(1600);
const IDLE_FRAME: Duration = Duration::from_millis(33);
const SEEK_STEP: f64 = 5.0;
const KEYS_HELP: &str = "space pause   <- -> seek   0-9 jump   +/- detail   c colour   s style   [ ] edges   , . light   f fullscreen   m mute   l loop   h stats   q quit";

pub fn run(args: Args) -> Result<()> {
    let event_loop = EventLoop::new()?;
    let mut player = Player::new(args)?;
    event_loop.run_app(&mut player)?;
    player.error.take().map_or(Ok(()), Err)
}

/// The file being played and its decoder state.
struct Media {
    path: PathBuf,
    name: String,
    info: MediaInfo,
    /// Decoded frame size.
    dw: usize,
    dh: usize,
    video: Option<VideoDecoder>,
    /// Next frame, waiting for its presentation time.
    pending: Option<Frame>,
    /// Frame on screen.
    frame: Option<Frame>,
    got_frame: bool,
    ended: bool,
    has_audio: bool,
}

impl Media {
    /// Moves the next decoded frame into `pending`, if there is one yet.
    fn pull(&mut self) {
        if self.pending.is_some() {
            return;
        }
        let next = match &self.video {
            Some(v) => v.rx.try_recv(),
            None => return,
        };
        match next {
            Ok(f) => {
                self.pending = Some(f);
                self.got_frame = true;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => self.video = None,
        }
    }

    fn position(&self) -> f64 {
        self.frame.as_ref().map_or(0.0, |f| f.pts)
    }
}

/// Playback position. Follows the sound card while there is sound, the wall clock otherwise.
struct Clock {
    base: f64,
    anchor_pos: f64,
    anchor: Instant,
    paused: bool,
}

impl Clock {
    fn new() -> Self {
        Self { base: 0.0, anchor_pos: 0.0, anchor: Instant::now(), paused: true }
    }

    /// Jumps to `t`, paused until the first frame is ready.
    fn reset(&mut self, t: f64) {
        self.base = t;
        self.anchor_pos = t;
        self.anchor = Instant::now();
        self.paused = true;
    }

    fn pos(&mut self, audio: Option<&AudioOut>) -> f64 {
        if self.paused {
            return self.anchor_pos;
        }
        if let Some(a) = audio {
            let (played, finished) = a.position();
            if !finished {
                self.anchor_pos = self.base + played;
                self.anchor = Instant::now();
                return self.anchor_pos;
            }
        }
        self.anchor_pos + self.anchor.elapsed().as_secs_f64()
    }

    fn set_paused(&mut self, paused: bool, audio: Option<&AudioOut>) {
        if paused == self.paused {
            return;
        }
        if paused {
            self.anchor_pos = self.pos(audio);
            self.paused = true;
        } else {
            self.paused = false;
            self.anchor = Instant::now();
        }
    }
}

struct RateMeter {
    count: u32,
    since: Instant,
    value: f64,
}

impl RateMeter {
    fn tick(&mut self) {
        self.count += 1;
        let secs = self.since.elapsed().as_secs_f64();
        if secs >= 0.5 {
            self.value = self.count as f64 / secs;
            self.count = 0;
            self.since = Instant::now();
        }
    }
}

pub struct Player {
    args: Args,
    font: Font,
    charset: Vec<char>,
    font_px: f32,
    renderer: Renderer,
    hud: GlyphSet,
    look: Look,

    media: Option<Media>,
    audio: Option<AudioOut>,
    clock: Clock,
    /// The user wants playback paused.
    user_paused: bool,
    /// Just started or seeked: waiting for the first frame before the clock runs.
    preroll: bool,
    looping: bool,
    volume: f32,
    muted: bool,

    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,
    redraw: bool,
    hud_until: Instant,
    hud_drawn: bool,
    show_stats: bool,
    toast: Option<(String, Instant)>,
    cursor: PhysicalPosition<f64>,
    fps: RateMeter,
    idle_start: Instant,
    idle_next: Instant,
    idle_frame: Vec<u8>,
    error: Option<anyhow::Error>,
}

impl Player {
    fn new(args: Args) -> Result<Self> {
        let font = font::load_font(args.font.as_deref())?;
        let charset = args.charset();
        let glyphs = GlyphSet::new(&font, args.font_size, &charset);
        let hud = GlyphSet::new(&font, 16.0, &font::ui_charset());
        let audio = match AudioOut::open() {
            Ok(a) => Some(a),
            Err(e) => {
                eprintln!("warning: no audio output ({e:#}), playing without sound");
                None
            }
        };
        let now = Instant::now();
        let mut player = Player {
            font_px: args.font_size,
            look: args.look(),
            looping: args.looping,
            muted: args.mute,
            volume: 1.0,
            font,
            charset,
            renderer: Renderer::new(glyphs),
            hud,
            media: None,
            audio,
            clock: Clock::new(),
            user_paused: false,
            preroll: false,
            window: None,
            surface: None,
            redraw: true,
            hud_until: now + HUD_TIME,
            hud_drawn: false,
            show_stats: false,
            toast: None,
            cursor: PhysicalPosition::new(0.0, 0.0),
            fps: RateMeter { count: 0, since: now, value: 0.0 },
            idle_start: now,
            idle_next: now,
            idle_frame: Vec::new(),
            error: None,
            args,
        };
        player.apply_volume();
        if let Some(input) = player.args.input.clone() {
            let start = player.args.start;
            player.load(&input, start)?;
        }
        Ok(player)
    }

    // ----- media control -------------------------------------------------

    fn load(&mut self, path: &Path, start: f64) -> Result<()> {
        let info = probe::probe(path)?;
        let (dw, dh) = info.decode_size(self.args.decode_size);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let has_audio = info.has_audio && self.audio.is_some();
        self.media = Some(Media {
            path: path.to_path_buf(),
            name,
            info,
            dw,
            dh,
            video: None,
            pending: None,
            frame: None,
            got_frame: false,
            ended: false,
            has_audio,
        });
        self.user_paused = false;
        self.seek_to(start)?;
        if let Some(w) = &self.window {
            w.set_title(&self.title());
        }
        Ok(())
    }

    fn seek_to(&mut self, t: f64) -> Result<()> {
        let Some(m) = self.media.as_mut() else { return Ok(()) };
        let last = m.info.duration.map_or(f64::INFINITY, |d| (d - 0.1).max(0.0));
        let t = t.clamp(0.0, last);

        m.video = None; // stop the old ffmpeg before starting a new one
        m.pending = None;
        m.got_frame = false;
        m.ended = false;
        m.video = Some(VideoDecoder::spawn(&m.path, t, None, m.dw, m.dh, &m.info.fps, !self.args.no_hwaccel)?);
        if let Some(a) = &self.audio {
            if m.has_audio {
                a.play(&m.path, t, None);
            } else {
                a.clear();
            }
        }

        self.clock.reset(t);
        self.preroll = true;
        if let Some(a) = &self.audio {
            a.set_paused(true);
        }
        self.redraw = true;
        Ok(())
    }

    fn seek_by(&mut self, delta: f64) -> Result<()> {
        let Some(m) = &self.media else { return Ok(()) };
        if m.ended {
            self.user_paused = false;
        }
        let t = m.position() + delta;
        self.seek_to(t)
    }

    fn restart(&mut self) {
        self.user_paused = false;
        if let Err(e) = self.seek_to(0.0) {
            self.show_toast(format!("{e:#}"));
        }
    }

    /// Pauses or resumes the clock and the sound together.
    fn set_running(&mut self, running: bool) {
        let audio = match &self.media {
            Some(m) if m.has_audio => self.audio.as_ref(),
            _ => None,
        };
        self.clock.set_paused(!running, audio);
        if let Some(a) = &self.audio {
            a.set_paused(!running);
        }
    }

    fn toggle_pause(&mut self) {
        let Some(m) = &self.media else { return };
        if m.ended {
            self.restart();
            return;
        }
        self.user_paused = !self.user_paused;
        if !self.preroll {
            self.set_running(!self.user_paused);
        }
    }

    fn apply_volume(&self) {
        if let Some(a) = &self.audio {
            a.set_volume(if self.muted { 0.0 } else { self.volume });
        }
    }

    fn zoom(&mut self, steps: i32) {
        let px = (self.font_px * 1.15f32.powi(steps)).clamp(4.0, 96.0);
        if (px - self.font_px).abs() < 0.01 {
            return;
        }
        self.font_px = px;
        self.renderer.glyphs = GlyphSet::new(&self.font, px, &self.charset);
        self.show_toast(format!("font {px:.0}px"));
    }

    // ----- timing -----------------------------------------------------------

    /// Advances playback. Returns when the event loop should wake up next.
    fn tick(&mut self) -> Instant {
        let now = Instant::now();

        if self.media.is_none() {
            if now >= self.idle_next {
                self.idle_next = now + IDLE_FRAME;
                self.redraw = true;
            }
            return self.wake_for_overlays(now, self.idle_next);
        }

        // The clock starts once the first frame after a load/seek has arrived.
        if let Some(m) = self.media.as_mut() {
            m.pull();
        }
        if self.preroll {
            let m = self.media.as_ref().unwrap();
            if m.pending.is_some() || m.video.is_none() {
                self.preroll = false;
                self.set_running(!self.user_paused);
            }
        }

        let use_audio = self.media.as_ref().is_some_and(|m| m.has_audio);
        let pos = self.clock.pos(if use_audio { self.audio.as_ref() } else { None });

        let m = self.media.as_mut().unwrap();
        let mut shown = false;
        while m.pending.as_ref().is_some_and(|f| f.pts <= pos + 0.002) {
            m.frame = m.pending.take();
            shown = true;
            m.pull();
        }
        let finished = !self.preroll && m.video.is_none() && m.pending.is_none() && !m.ended;
        let can_loop = m.got_frame && m.info.duration.is_some_and(|d| d > 0.5);
        if shown {
            self.redraw = true;
        }

        if finished {
            if self.looping && can_loop {
                self.restart();
            } else {
                if let Some(m) = self.media.as_mut() {
                    m.ended = true;
                }
                self.user_paused = true;
                self.set_running(false);
                self.hud_until = now + HUD_TIME;
            }
            self.redraw = true;
        }

        let mut wake = now + Duration::from_millis(500);
        let m = self.media.as_ref().unwrap();
        if let Some(f) = &m.pending {
            if !self.clock.paused {
                let wait = (f.pts - pos).clamp(0.0, 0.5);
                wake = wake.min(now + Duration::from_secs_f64(wait));
            }
        } else if m.video.is_some() {
            wake = wake.min(now + Duration::from_millis(2)); // decoder is catching up
        }
        self.wake_for_overlays(now, wake)
    }

    /// Schedules redraws for the HUD and toast disappearing.
    fn wake_for_overlays(&mut self, now: Instant, mut wake: Instant) -> Instant {
        if let Some((_, until)) = &self.toast {
            if now >= *until {
                self.toast = None;
                self.redraw = true;
            } else {
                wake = wake.min(*until);
            }
        }
        if now < self.hud_until {
            wake = wake.min(self.hud_until);
        } else if self.hud_drawn && !self.hud_pinned() {
            self.redraw = true;
        }
        wake
    }

    fn hud_pinned(&self) -> bool {
        self.show_stats || (self.user_paused && self.media.is_some())
    }

    fn poke_hud(&mut self) {
        self.hud_until = Instant::now() + HUD_TIME;
        if !self.hud_drawn {
            self.redraw = true;
        }
    }

    fn show_toast(&mut self, message: String) {
        self.toast = Some((message, Instant::now() + TOAST_TIME));
        self.redraw = true;
    }

    // ----- drawing ----------------------------------------------------------

    fn draw(&mut self) -> Result<()> {
        let Some(window) = self.window.clone() else { return Ok(()) };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return Ok(()); // minimised
        };
        let Some(mut surface) = self.surface.take() else { return Ok(()) };
        let result = (|| -> Result<()> {
            surface.resize(w, h).map_err(|e| anyhow!("{e}"))?;
            let mut buffer = surface.buffer_mut().map_err(|e| anyhow!("{e}"))?;
            self.paint(&mut buffer, size.width as usize, size.height as usize);
            buffer.present().map_err(|e| anyhow!("{e}"))?;
            Ok(())
        })();
        self.surface = Some(surface);
        self.redraw = false;
        self.fps.tick();
        result
    }

    fn paint(&mut self, buf: &mut [u32], bw: usize, bh: usize) {
        let now = Instant::now();
        match &self.media {
            Some(m) => match &m.frame {
                Some(f) => self.renderer.draw_fit(&f.data, m.dw, m.dh, &self.look, buf, bw, bh),
                None => buf.fill(0),
            },
            None => {
                let t = self.idle_start.elapsed().as_secs_f32();
                plasma(t, 320, 180, &mut self.idle_frame);
                self.renderer.draw_fit(&self.idle_frame, 320, 180, &self.look, buf, bw, bh);
            }
        }
        self.hud_drawn = self.overlay(buf, bw, bh, now);
    }

    /// Draws the HUD, progress bar and messages. Returns whether the HUD is showing.
    fn overlay(&self, buf: &mut [u32], bw: usize, bh: usize, now: Instant) -> bool {
        let g = &self.hud;
        let margin = g.cell_w * 2;

        let Some(m) = &self.media else {
            let lines = vec![
                ("V I D E O - C O D E".to_string(), ACCENT),
                (String::new(), WHITE),
                ("Drop a video file onto this window to play it.".to_string(), WHITE),
                ("or run:  video-code path/to/video.mp4".to_string(), GRAY),
            ];
            let (w, h) = render::text_box_size(g, &lines);
            render::text_box(buf, bw, bh, g, bw.saturating_sub(w) / 2, bh.saturating_sub(h) / 2, &lines);
            self.draw_toast(buf, bw, bh, now);
            return false;
        };

        if m.frame.is_none() {
            let lines = vec![(format!("loading {} ...", m.name), WHITE)];
            let (w, h) = render::text_box_size(g, &lines);
            render::text_box(buf, bw, bh, g, bw.saturating_sub(w) / 2, bh.saturating_sub(h) / 2, &lines);
        }

        let visible = now < self.hud_until || self.hud_pinned();
        if visible {
            render::text_box(buf, bw, bh, g, margin, margin, &self.hud_lines(m));

            if let Some(total) = m.info.duration {
                let bar_h = (g.cell_h / 4).max(3);
                let y = bh.saturating_sub(bar_h);
                let done = ((m.position() / total).clamp(0.0, 1.0) * bw as f64) as usize;
                render::fill_rect(buf, bw, bh, 0, y, bw, bar_h, 0x2A2A2A);
                render::fill_rect(buf, bw, bh, 0, y, done, bar_h, ACCENT);
            }
        }
        self.draw_toast(buf, bw, bh, now);
        visible
    }

    fn draw_toast(&self, buf: &mut [u32], bw: usize, bh: usize, now: Instant) {
        if let Some((message, until)) = &self.toast {
            if now < *until {
                let g = &self.hud;
                let lines = vec![(message.clone(), WHITE)];
                let (w, h) = render::text_box_size(g, &lines);
                let y = bh.saturating_sub(h + g.cell_h * 3);
                render::text_box(buf, bw, bh, g, bw.saturating_sub(w) / 2, y, &lines);
            }
        }
    }

    fn hud_lines(&self, m: &Media) -> Vec<(String, u32)> {
        let total = m.info.duration.map_or_else(|| "--:--".to_string(), fmt_time);
        let state = if m.ended {
            "ended - space to replay"
        } else if self.preroll {
            "loading"
        } else if self.user_paused {
            "paused"
        } else {
            "playing"
        };
        let sound = if !m.has_audio {
            "no sound".to_string()
        } else if self.muted {
            "muted".to_string()
        } else {
            format!("volume {:.0}%", self.volume * 100.0)
        };
        let grid = self.renderer.grid;
        vec![
            (
                format!(
                    "{}   {} / {}   {}   {}{}",
                    m.name,
                    fmt_time(m.position()),
                    total,
                    state,
                    sound,
                    if self.looping { "   loop" } else { "" }
                ),
                WHITE,
            ),
            (
                format!(
                    "{}  |  {}  |  {}x{} characters  |  font {:.0}px  |  edges {:.2}  |  light {:.1}  |  {:.0} fps",
                    self.look.mode.name(),
                    self.look.style.name(),
                    grid.cols,
                    grid.rows,
                    self.font_px,
                    self.look.contrast,
                    self.look.brightness,
                    self.fps.value
                ),
                ACCENT,
            ),
            (KEYS_HELP.to_string(), GRAY),
        ]
    }

    fn title(&self) -> String {
        match &self.media {
            Some(m) => format!("{} - video-code", m.name),
            None => "video-code".to_string(),
        }
    }

    // ----- input -----------------------------------------------------------

    fn on_key(&mut self, event_loop: &ActiveEventLoop, event: &KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        self.hud_until = Instant::now() + HUD_TIME;
        self.redraw = true;
        let result = match event.logical_key.as_ref() {
            Key::Named(NamedKey::Escape) => {
                if self.is_fullscreen() {
                    self.set_fullscreen(false);
                } else {
                    event_loop.exit();
                }
                Ok(())
            }
            Key::Named(NamedKey::Space) => {
                self.toggle_pause();
                Ok(())
            }
            Key::Named(NamedKey::ArrowLeft) => self.seek_by(-SEEK_STEP),
            Key::Named(NamedKey::ArrowRight) => self.seek_by(SEEK_STEP),
            Key::Named(NamedKey::ArrowUp) => {
                self.change_volume(0.1);
                Ok(())
            }
            Key::Named(NamedKey::ArrowDown) => {
                self.change_volume(-0.1);
                Ok(())
            }
            Key::Named(NamedKey::Home) => self.seek_to(0.0),
            Key::Named(NamedKey::F11) => {
                self.set_fullscreen(!self.is_fullscreen());
                Ok(())
            }
            Key::Character(c) => self.on_char(event_loop, c),
            _ => Ok(()),
        };
        if let Err(e) = result {
            self.show_toast(format!("{e:#}"));
        }
    }

    fn on_char(&mut self, event_loop: &ActiveEventLoop, c: &str) -> Result<()> {
        match c {
            "+" | "=" => self.zoom(1),
            "-" | "_" => self.zoom(-1),
            "[" => {
                self.look.contrast = (self.look.contrast - 0.25).max(0.5);
                self.show_toast(format!("edges {:.2}", self.look.contrast));
            }
            "]" => {
                self.look.contrast = (self.look.contrast + 0.25).min(6.0);
                self.show_toast(format!("edges {:.2}", self.look.contrast));
            }
            "," => {
                self.look.brightness = (self.look.brightness - 0.1).max(0.2);
                self.show_toast(format!("light {:.1}", self.look.brightness));
            }
            "." => {
                self.look.brightness = (self.look.brightness + 0.1).min(4.0);
                self.show_toast(format!("light {:.1}", self.look.brightness));
            }
            _ => match c.to_ascii_lowercase().as_str() {
                "q" => event_loop.exit(),
                "f" => self.set_fullscreen(!self.is_fullscreen()),
                "c" => {
                    self.look.mode = self.look.mode.next();
                    self.show_toast(format!("colour: {}", self.look.mode.name()));
                }
                "s" => {
                    self.look.style = self.look.style.next();
                    self.show_toast(format!("style: {}", self.look.style.name()));
                }
                "m" => {
                    self.muted = !self.muted;
                    self.apply_volume();
                    self.show_toast(if self.muted { "muted" } else { "sound on" }.to_string());
                }
                "l" => {
                    self.looping = !self.looping;
                    self.show_toast(format!("loop {}", if self.looping { "on" } else { "off" }));
                }
                "h" => self.show_stats = !self.show_stats,
                d => {
                    if let (Ok(n), Some(m)) = (d.parse::<u8>(), &self.media) {
                        if let Some(total) = m.info.duration {
                            if m.ended {
                                self.user_paused = false;
                            }
                            return self.seek_to(total * n as f64 / 10.0);
                        }
                    }
                }
            },
        }
        Ok(())
    }

    fn change_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 1.5);
        self.muted = false;
        self.apply_volume();
        self.show_toast(format!("volume {:.0}%", self.volume * 100.0));
    }

    fn on_click(&mut self) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        let Some(m) = &self.media else { return };
        let Some(total) = m.info.duration else { return };
        // Clicks along the bottom edge seek.
        if self.cursor.y >= size.height as f64 - (self.hud.cell_h * 2) as f64 {
            let frac = (self.cursor.x / size.width.max(1) as f64).clamp(0.0, 1.0);
            if m.ended {
                self.user_paused = false;
            }
            if let Err(e) = self.seek_to(total * frac) {
                self.show_toast(format!("{e:#}"));
            }
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.fullscreen().is_some())
    }

    fn set_fullscreen(&self, on: bool) {
        if let Some(w) = &self.window {
            w.set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
        }
    }

    fn fail(&mut self, event_loop: &ActiveEventLoop, e: anyhow::Error) {
        self.error.get_or_insert(e);
        event_loop.exit();
    }
}

impl ApplicationHandler for Player {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        // Start at 75% of the screen, shaped like the video.
        let aspect = self.media.as_ref().map_or(16.0 / 9.0, |m| m.dw as f64 / m.dh as f64);
        let (sw, sh) = event_loop
            .primary_monitor()
            .or_else(|| event_loop.available_monitors().next())
            .map(|m| (m.size().width as f64, m.size().height as f64))
            .unwrap_or((1920.0, 1080.0));
        let (mut w, mut h) = (sw * 0.75, sh * 0.75);
        if w / h > aspect {
            w = h * aspect;
        } else {
            h = w / aspect;
        }

        let attributes = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(PhysicalSize::new(w.round() as u32, h.round() as u32))
            .with_min_inner_size(PhysicalSize::new(240u32, 135u32));
        let window = match event_loop.create_window(attributes) {
            Ok(w) => Rc::new(w),
            Err(e) => return self.fail(event_loop, e.into()),
        };
        let surface = Context::new(window.clone())
            .and_then(|context| Surface::new(&context, window.clone()))
            .map_err(|e| anyhow!("couldn't set up drawing: {e}"));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => return self.fail(event_loop, e),
        }

        let ui_px = (15.0 * window.scale_factor() as f32).round();
        self.hud = GlyphSet::new(&self.font, ui_px, &font::ui_charset());
        self.window = Some(window);
        self.redraw = true;
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.draw() {
                    self.fail(event_loop, e);
                }
            }
            WindowEvent::Resized(_) => self.redraw = true,
            WindowEvent::KeyboardInput { event, .. } => self.on_key(event_loop, &event),
            WindowEvent::DroppedFile(path) => {
                if let Err(e) = self.load(&path, 0.0) {
                    eprintln!("{e:#}");
                    self.show_toast(format!("{e:#}"));
                }
                self.poke_hud();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = position;
                self.poke_hud();
            }
            WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                self.on_click();
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let up = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                if up != 0.0 {
                    self.zoom(if up > 0.0 { 1 } else { -1 });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            return;
        }
        let wake = self.tick();
        if self.redraw {
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(wake));
    }
}

fn fmt_time(t: f64) -> String {
    let s = t.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Colourful animated background shown while no video is loaded.
fn plasma(t: f32, w: usize, h: usize, out: &mut Vec<u8>) {
    out.resize(w * h * 3, 0);
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let fy = y as f32 / h as f32;
        for x in 0..w {
            let fx = x as f32 / w as f32;
            let (dx, dy) = (fx - 0.5, (fy - 0.5) * 0.6);
            let d = (dx * dx + dy * dy).sqrt();
            let v = ((fx * 9.0 + t).sin()
                + (fy * 7.0 - t * 0.7).sin()
                + ((fx + fy) * 5.0 + t * 1.3).sin()
                + (d * 22.0 - t * 2.0).sin())
                * 0.25;
            let hue = v * 0.5 + t * 0.05;
            let fade = (1.4 - d * 1.5).clamp(0.0, 1.0) * (0.75 + 0.25 * v.abs());
            let px = &mut row[x * 3..x * 3 + 3];
            for (i, p) in px.iter_mut().enumerate() {
                let c = 0.5 + 0.5 * (TAU * (hue + i as f32 / 3.0)).cos();
                *p = (c * fade * 255.0) as u8;
            }
        }
    });
}
