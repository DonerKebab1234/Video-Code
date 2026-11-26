//! `--web`: a small local web server for the browser interface.
//!
//! The browser draws the live preview itself (WebGL), using glyph shapes it
//! gets from here, so the preview picks the same characters as an export.
//! Exports run here, with the same code as `video-code clip.mp4 -o out.mp4`:
//! the page uploads the video once, starts a job, polls it and downloads the
//! result. Everything lives in a temp folder that is wiped on the next start.

use std::{
    collections::HashMap,
    fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use clap::ValueEnum;
use fontdue::Font;
use serde::{Deserialize, Serialize};
use tiny_http::{Header, Method, Request, Response, ResponseBox, Server, StatusCode};

use crate::{
    ascii::{ColorMode, Look, Style},
    cli::Args,
    export::{self, Cancelled, Event, Settings},
    font::{self, GlyphSet},
    probe::{self, missing_tool, MediaInfo},
};

const INDEX_HTML: &str = include_str!("../web/index.html");
const STYLE_CSS: &str = include_str!("../web/style.css");
const APP_JS: &str = include_str!("../web/app.js");
const ENGINE_JS: &str = include_str!("../web/engine.js");
const FAVICON_SVG: &str = include_str!("../web/favicon.svg");

/// The preview shader stores a glyph index in one byte.
const MAX_GLYPHS: usize = 256;

/// Longest side of the browser-friendly copy made when the browser can't play a file.
const PREVIEW_SIZE: usize = 1280;

const CSP: &str = "default-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; \
                   style-src 'self'; script-src 'self'; connect-src 'self'; object-src 'none'; \
                   base-uri 'none'; frame-ancestors 'none'";

pub fn run(args: Args) -> Result<()> {
    let font = font::load_font(args.font.as_deref())?;

    // One folder for uploads and finished files. Whatever the last run left is deleted.
    let dir = std::env::temp_dir().join("video-code-web");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).with_context(|| format!("couldn't create {}", dir.display()))?;

    let server = bind(args.port)?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|a| a.port())
        .context("the web server has no TCP address")?;
    let ffmpeg = tool_works("ffmpeg") && tool_works("ffprobe");

    let app = Arc::new(App {
        font,
        port,
        dir,
        ffmpeg,
        next_id: AtomicU64::new(1),
        uploads: Mutex::new(HashMap::new()),
        jobs: Mutex::new(HashMap::new()),
    });

    let url = format!("http://127.0.0.1:{port}/");
    println!("video-code is running at {url}");
    println!("Press Ctrl+C to stop it.");
    if !ffmpeg {
        eprintln!(
            "warning: ffmpeg/ffprobe were not found, so exporting won't work. \
             Install FFmpeg (Windows: `winget install Gyan.FFmpeg`) and restart."
        );
    }
    if !args.no_browser {
        open_browser(&url);
    }

    for request in server.incoming_requests() {
        let app = Arc::clone(&app);
        thread::spawn(move || handle(&app, request));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

struct App {
    font: Font,
    port: u16,
    dir: PathBuf,
    ffmpeg: bool,
    next_id: AtomicU64,
    uploads: Mutex<HashMap<String, Arc<Upload>>>,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
}

impl App {
    fn new_id(&self, prefix: &str) -> String {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        let salt = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        format!("{prefix}{n}{:05x}", salt & 0xFFFFF)
    }

    /// Deletes earlier uploads that no running job still reads from.
    fn forget_old_uploads(&self) {
        let busy: Vec<String> = lock(&self.jobs)
            .values()
            .filter(|j| lock(&j.status).state == State::Running)
            .map(|j| j.upload.clone())
            .collect();
        lock(&self.uploads).retain(|id, upload| {
            let keep = busy.contains(id);
            if !keep {
                let _ = fs::remove_dir_all(&upload.dir);
            }
            keep
        });
    }
}

struct Upload {
    dir: PathBuf,
    path: PathBuf,
    name: String,
    info: MediaInfo,
}

struct Job {
    upload: String,
    output: PathBuf,
    file_name: String,
    started: Instant,
    cancel: AtomicBool,
    status: Mutex<Status>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum State {
    Running,
    Done,
    Failed,
    Cancelled,
}

struct Status {
    state: State,
    frames: u64,
    total: Option<u64>,
    /// Seconds the job took, once it has finished.
    took: Option<f64>,
    error: Option<String>,
    bytes: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

struct ApiError {
    status: u16,
    message: String,
}

fn fail(status: u16, message: impl Into<String>) -> ApiError {
    ApiError { status, message: message.into() }
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    fail(500, e.to_string())
}

type Reply = Result<ResponseBox, ApiError>;

fn handle(app: &Arc<App>, mut request: Request) {
    let mut response = route(app, &mut request).unwrap_or_else(|e| {
        let body = serde_json::json!({ "error": e.message }).to_string();
        Response::from_string(body)
            .with_status_code(e.status)
            .with_header(content_type("application/json"))
            .boxed()
    });
    response.add_header(header("X-Content-Type-Options", "nosniff"));
    response.add_header(header("Referrer-Policy", "no-referrer"));
    let _ = request.respond(response);
}

fn route(app: &Arc<App>, request: &mut Request) -> Reply {
    if !trusted(app, request) {
        return Err(fail(403, "requests must come from the video-code page"));
    }
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((&url, ""));
    let method = request.method().clone();

    match (&method, path) {
        (Method::Get, "/" | "/index.html") => Ok(page(INDEX_HTML, "text/html; charset=utf-8")),
        (Method::Get, "/style.css") => Ok(page(STYLE_CSS, "text/css; charset=utf-8")),
        (Method::Get, "/app.js") => Ok(page(APP_JS, "text/javascript; charset=utf-8")),
        (Method::Get, "/engine.js") => Ok(page(ENGINE_JS, "text/javascript; charset=utf-8")),
        (Method::Get, "/favicon.svg") => Ok(page(FAVICON_SVG, "image/svg+xml")),
        (Method::Get, "/api/info") => Ok(json(&serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "ffmpeg": app.ffmpeg,
        }))),
        (Method::Get, "/api/glyphs") => glyphs(app, query),
        (Method::Post, "/api/upload") => upload(app, request),
        (Method::Post, "/api/jobs") => create_job(app, request),
        _ => match path.strip_prefix("/api/jobs/") {
            Some(rest) => job_route(app, &method, rest, query, request),
            None => Err(fail(404, "not found")),
        },
    }
}

/// Only this page may talk to the server: the Host header guards against DNS
/// rebinding, and the Origin header against other websites posting to localhost.
fn trusted(app: &App, request: &Request) -> bool {
    let local = |authority: &str| {
        let a = authority.trim().to_ascii_lowercase();
        let port = app.port;
        [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")].contains(&a)
    };
    if !get_header(request, "Host").is_some_and(|h| local(&h)) {
        return false;
    }
    match get_header(request, "Origin") {
        Some(origin) => origin.trim().strip_prefix("http://").is_some_and(local),
        None => true,
    }
}

fn job_route(app: &Arc<App>, method: &Method, rest: &str, query: &str, request: &Request) -> Reply {
    let (id, action) = rest.split_once('/').unwrap_or((rest, ""));
    let job = lock(&app.jobs).get(id).cloned().ok_or_else(|| fail(404, "no such job"))?;
    match (method, action) {
        (Method::Get, "") => Ok(json(&job_json(&job))),
        (Method::Post, "cancel") => {
            job.cancel.store(true, Ordering::Relaxed);
            Ok(json(&job_json(&job)))
        }
        (Method::Get, "file") => {
            if lock(&job.status).state != State::Done {
                return Err(fail(409, "the file isn't ready yet"));
            }
            let inline = parse_query(query).contains_key("inline");
            serve_file(&job.output, &job.file_name, inline, get_header(request, "Range"))
        }
        _ => Err(fail(404, "not found")),
    }
}

fn job_json(job: &Job) -> serde_json::Value {
    let s = lock(&job.status);
    serde_json::json!({
        "state": s.state,
        "frames": s.frames,
        "total": s.total,
        "elapsed": s.took.unwrap_or_else(|| job.started.elapsed().as_secs_f64()),
        "error": s.error,
        "file": job.file_name,
        "bytes": s.bytes,
    })
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

fn glyphs(app: &App, query: &str) -> Reply {
    let q = parse_query(query);
    let px = q
        .get("px")
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(12.0)
        .clamp(3.0, 200.0);
    let charset = charset_from(q.get("chars").map_or("", String::as_str));
    let set = GlyphSet::new(&app.font, px, &charset);
    Ok(Response::from_data(set.to_bytes())
        .with_header(content_type("application/octet-stream"))
        .with_header(header("Cache-Control", "private, max-age=3600"))
        .boxed())
}

/// The characters to draw with: the user's own (deduplicated, at most 256) or all of ASCII.
fn charset_from(chars: &str) -> Vec<char> {
    let mut set = Vec::new();
    for c in chars.chars() {
        if !c.is_control() && !set.contains(&c) {
            set.push(c);
            if set.len() == MAX_GLYPHS {
                break;
            }
        }
    }
    if set.is_empty() {
        font::ascii_charset()
    } else {
        set
    }
}

// ---------------------------------------------------------------------------
// Uploads
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct UploadReply {
    id: String,
    name: String,
    duration: Option<f64>,
    width: usize,
    height: usize,
    fps: f64,
    has_audio: bool,
}

fn upload(app: &App, request: &mut Request) -> Reply {
    if !app.ffmpeg {
        return Err(fail(503, "FFmpeg isn't installed, so videos can't be converted. Install it and restart video-code."));
    }
    let name = get_header(request, "X-File-Name").map(|v| percent_decode(&v)).unwrap_or_default();
    let name = safe_file_name(&name);
    let id = app.new_id("u");
    let dir = app.dir.join(&id);
    fs::create_dir_all(&dir).map_err(internal)?;
    let path = dir.join(&name);

    let copied = fs::File::create(&path).and_then(|mut file| io::copy(request.as_reader(), &mut file));
    if let Err(e) = copied {
        let _ = fs::remove_dir_all(&dir);
        return Err(fail(400, format!("the upload didn't finish: {e}")));
    }

    let info = match probe::probe(&path) {
        Ok(info) => info,
        Err(e) => {
            eprintln!("couldn't read {name}: {e:#}");
            let _ = fs::remove_dir_all(&dir);
            return Err(fail(422, format!("{name} isn't a video FFmpeg can read.")));
        }
    };
    app.forget_old_uploads();

    let reply = UploadReply {
        id: id.clone(),
        name: name.clone(),
        duration: info.duration,
        width: info.width,
        height: info.height,
        fps: info.fps.value,
        has_audio: info.has_audio,
    };
    lock(&app.uploads).insert(id, Arc::new(Upload { dir, path, name, info }));
    Ok(json(&reply))
}

/// A file name that is safe to create on any OS.
fn safe_file_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .map(|c| if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' })
        .take(120)
        .collect();
    let clean = clean.trim_matches(|c: char| c == '.' || c == ' ').to_string();
    if clean.is_empty() {
        "video".to_string()
    } else {
        clean
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct JobRequest {
    upload: String,
    /// "export" renders the ASCII version; "preview" makes a copy the browser can play.
    kind: String,
    /// mp4, webm, gif or png (one frame).
    format: String,
    start: f64,
    end: Option<f64>,
    size: usize,
    crf: u32,
    font_size: f32,
    chars: String,
    color: String,
    style: String,
    contrast: f32,
    brightness: f32,
    saturation: f32,
}

impl Default for JobRequest {
    fn default() -> Self {
        Self {
            upload: String::new(),
            kind: "export".into(),
            format: "mp4".into(),
            start: 0.0,
            end: None,
            size: 1920,
            crf: 18,
            font_size: 12.0,
            chars: String::new(),
            color: "color".into(),
            style: "hybrid".into(),
            contrast: 2.0,
            brightness: 1.0,
            saturation: 1.3,
        }
    }
}

fn create_job(app: &Arc<App>, request: &mut Request) -> Reply {
    let mut body = String::new();
    request
        .as_reader()
        .take(64 * 1024)
        .read_to_string(&mut body)
        .map_err(|e| fail(400, e.to_string()))?;
    let req: JobRequest = serde_json::from_str(&body).map_err(|e| fail(400, format!("bad job: {e}")))?;
    let upload = lock(&app.uploads)
        .get(&req.upload)
        .cloned()
        .ok_or_else(|| fail(404, "that upload is gone; open the video again"))?;

    let stem = Path::new(&upload.name)
        .file_stem()
        .map_or_else(|| "video".to_string(), |s| s.to_string_lossy().into_owned());

    let (file_name, work): (String, Work) = match req.kind.as_str() {
        "preview" => (
            format!("{stem}-preview.mp4"),
            Box::new(move |_app: &App, job: &Job| transcode_preview(&upload, job)),
        ),
        "export" => {
            let format = req.format.to_ascii_lowercase();
            if !matches!(format.as_str(), "mp4" | "webm" | "gif" | "png") {
                return Err(fail(400, format!("can't export to {format}")));
            }
            let mode = ColorMode::from_str(&req.color, true)
                .map_err(|_| fail(400, format!("unknown colour style {}", req.color)))?;
            let style = Style::from_str(&req.style, true)
                .map_err(|_| fail(400, format!("unknown character style {}", req.style)))?;
            let start = finite(req.start, 0.0).max(0.0);
            let duration = req
                .end
                .map(|end| finite(end, 0.0) - start)
                .filter(|d| *d > 0.0 && format != "png");
            let look = Look {
                mode,
                style,
                contrast: finite(req.contrast as f64, 2.0).clamp(0.1, 10.0) as f32,
                brightness: finite(req.brightness as f64, 1.0).clamp(0.05, 10.0) as f32,
                saturation: finite(req.saturation as f64, 1.3).clamp(0.0, 5.0) as f32,
            };
            let font_size = finite(req.font_size as f64, 12.0).clamp(3.0, 200.0) as f32;
            let size = req.size.clamp(64, 7680);
            let crf = req.crf.min(51);
            let charset = charset_from(&req.chars);

            let file_name = if format == "png" {
                let s = start as u64;
                format!("{stem}-ascii-{:02}-{:02}.png", s / 60, s % 60)
            } else {
                format!("{stem}-ascii.{format}")
            };
            let work: Work = Box::new(move |app: &App, job: &Job| {
                let settings = Settings {
                    font: &app.font,
                    font_size,
                    charset: &charset,
                    look,
                    start,
                    duration,
                    size,
                    crf,
                    decode_size: 1280,
                    hwaccel: true,
                };
                export::render(&upload.path, &job.output, &settings, &job.cancel, |event| {
                    let mut s = lock(&job.status);
                    match event {
                        Event::Start(plan) => s.total = plan.total,
                        Event::Frame { frames, total } => {
                            s.frames = frames;
                            s.total = total;
                        }
                    }
                })
                .map(|_| ())
            });
            (file_name, work)
        }
        other => return Err(fail(400, format!("unknown job kind {other}"))),
    };

    let id = app.new_id("j");
    let out_dir = app.dir.join(&id);
    fs::create_dir_all(&out_dir).map_err(internal)?;
    let job = register(app, &id, &req.upload, out_dir.join(&file_name), file_name);
    spawn(app, job, work);
    Ok(json(&serde_json::json!({ "id": id })))
}

/// A job's work, run on its own thread.
type Work = Box<dyn FnOnce(&App, &Job) -> Result<()> + Send>;

fn register(app: &App, id: &str, upload: &str, output: PathBuf, file_name: String) -> Arc<Job> {
    let job = Arc::new(Job {
        upload: upload.to_string(),
        output,
        file_name,
        started: Instant::now(),
        cancel: AtomicBool::new(false),
        status: Mutex::new(Status { state: State::Running, frames: 0, total: None, took: None, error: None, bytes: 0 }),
    });
    lock(&app.jobs).insert(id.to_string(), Arc::clone(&job));
    job
}

fn spawn(app: &Arc<App>, job: Arc<Job>, work: Work) {
    let app = Arc::clone(app);
    thread::spawn(move || {
        let result = work(&app, &job);
        let mut s = lock(&job.status);
        s.took = Some(job.started.elapsed().as_secs_f64());
        match result {
            Ok(()) => {
                s.state = State::Done;
                s.bytes = fs::metadata(&job.output).map_or(0, |m| m.len());
            }
            Err(e) if e.is::<Cancelled>() => s.state = State::Cancelled,
            Err(e) => {
                eprintln!("job failed: {e:#}");
                s.state = State::Failed;
                s.error = Some(format!("{e:#}"));
            }
        }
    });
}

/// Re-encodes a video the browser can't play (AVI, ProRes, HEVC, ...) as H.264 MP4.
fn transcode_preview(upload: &Upload, job: &Job) -> Result<()> {
    let (w, h) = upload.info.decode_size(PREVIEW_SIZE);
    let (w, h) = ((w & !1).max(2), (h & !1).max(2));
    let fps = upload.info.fps.value;
    lock(&job.status).total = upload.info.duration.map(|d| (d * fps).round().max(1.0) as u64);

    let mut child = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y", "-i"])
        .arg(&upload.path)
        .args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn", "-vf"])
        .arg(format!("scale={w}:{h}"))
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac", "-b:a", "160k", "-ac", "2", "-movflags", "+faststart"])
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(&job.output)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| missing_tool(e, "ffmpeg"))?;

    let stderr = child.stderr.take();
    let messages = thread::spawn(move || {
        let mut text = String::new();
        if let Some(mut s) = stderr {
            let _ = s.read_to_string(&mut text);
        }
        text
    });

    let stdout = child.stdout.take().context("ffmpeg stdout")?;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        if job.cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&job.output);
            return Err(Cancelled.into());
        }
        if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
            lock(&job.status).frames = (us.max(0.0) / 1e6 * fps) as u64;
        }
    }

    let status = child.wait()?;
    if !status.success() {
        let detail = export::last_lines(&messages.join().unwrap_or_default());
        bail!("ffmpeg couldn't convert {}: {detail}", upload.name);
    }
    Ok(())
}

fn finite(v: f64, fallback: f64) -> f64 {
    if v.is_finite() {
        v
    } else {
        fallback
    }
}

// ---------------------------------------------------------------------------
// Responses
// ---------------------------------------------------------------------------

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn content_type(value: &str) -> Header {
    header("Content-Type", value)
}

fn get_header(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn page(body: &'static str, kind: &str) -> ResponseBox {
    Response::from_string(body)
        .with_header(content_type(kind))
        .with_header(header("Cache-Control", "no-cache"))
        .with_header(header("Content-Security-Policy", CSP))
        .boxed()
}

fn json<T: Serialize>(value: &T) -> ResponseBox {
    let body = serde_json::to_string(value).unwrap_or_else(|_| "null".into());
    Response::from_string(body)
        .with_header(content_type("application/json"))
        .with_header(header("Cache-Control", "no-store"))
        .boxed()
}

/// Sends a finished file, with byte-range support so the browser can seek in videos.
fn serve_file(path: &Path, name: &str, inline: bool, range: Option<String>) -> Reply {
    let mut file = fs::File::open(path).map_err(|_| fail(410, "the file is gone; export it again"))?;
    let len = file.metadata().map_err(internal)?.len();

    let kind = match export::extension(path).as_str() {
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "gif" => "image/gif",
        "png" => "image/png",
        _ => "application/octet-stream",
    };
    let ascii_name: String = name
        .chars()
        .map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' })
        .collect();
    let disposition = format!(
        "{}; filename=\"{ascii_name}\"; filename*=UTF-8''{}",
        if inline { "inline" } else { "attachment" },
        percent_encode(name)
    );
    let mut headers = vec![
        content_type(kind),
        header("Accept-Ranges", "bytes"),
        header("Content-Disposition", &disposition),
        header("Cache-Control", "no-store"),
    ];

    let Some(range) = range else {
        return Ok(Response::new(StatusCode(200), headers, Box::new(file) as Box<dyn Read + Send>, Some(len as usize), None));
    };
    let Some((start, end)) = parse_range(&range, len) else {
        headers.push(header("Content-Range", &format!("bytes */{len}")));
        return Ok(Response::new(StatusCode(416), headers, Box::new(io::empty()) as Box<dyn Read + Send>, Some(0), None));
    };
    file.seek(SeekFrom::Start(start)).map_err(internal)?;
    let part = end - start + 1;
    headers.push(header("Content-Range", &format!("bytes {start}-{end}/{len}")));
    Ok(Response::new(StatusCode(206), headers, Box::new(file.take(part)) as Box<dyn Read + Send>, Some(part as usize), None))
}

/// Parses a single `bytes=a-b` range into inclusive offsets.
fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = if a.trim().is_empty() {
        let n: u64 = b.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        (len.saturating_sub(n), len - 1)
    } else {
        let start: u64 = a.trim().parse().ok()?;
        let end = if b.trim().is_empty() { len - 1 } else { b.trim().parse::<u64>().ok()?.min(len - 1) };
        (start, end)
    };
    (start <= end).then_some((start, end))
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// Decodes `%XX` escapes (as made by JavaScript's `encodeURIComponent`).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let hex = |b: u8| (b as char).to_digit(16);
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

fn bind(port: u16) -> Result<Server> {
    match Server::http(("127.0.0.1", port)) {
        Ok(server) => Ok(server),
        Err(e) if port != 0 => {
            eprintln!("port {port} isn't free ({e}), using another one");
            Server::http(("127.0.0.1", 0)).map_err(|e| anyhow!("couldn't start the web server: {e}"))
        }
        Err(e) => Err(anyhow!("couldn't start the web server: {e}")),
    }
}

fn tool_works(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn open_browser(url: &str) {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", url]);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(url);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    let opened = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if opened.is_err() {
        eprintln!("Couldn't open a browser. Open {url} yourself.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_range("bytes=900-", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=0-5000", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=5-1", 1000), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 1000), None);
    }

    #[test]
    fn percent() {
        assert_eq!(percent_decode("a%20b%2Bc"), "a b+c");
        assert_eq!(percent_decode("%E2%96%91%E2%96%92"), "░▒");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_encode("é a"), "%C3%A9%20a");
    }

    #[test]
    fn file_names() {
        assert_eq!(safe_file_name("C:\\Users\\me\\My clip.mp4"), "My clip.mp4");
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("a<b>:c.mov"), "a_b__c.mov");
        assert_eq!(safe_file_name(".."), "video");
        assert_eq!(safe_file_name(""), "video");
    }

    #[test]
    fn charsets() {
        assert_eq!(charset_from(" .:: ."), vec![' ', '.', ':']);
        assert_eq!(charset_from("").len(), 95);
    }
}
