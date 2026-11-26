// The browser interface: opening files, playback controls, keyboard
// shortcuts, the look panel and exports. Drawing lives in engine.js.

import { Engine, parseGlyphs } from '/engine.js';

const MODES = ['color', 'color-bg', 'matrix', 'amber', 'mono'];
const STYLES = ['hybrid', 'shape', 'ramp'];
const STYLE_INFO = {
  hybrid: 'Edges get the character with the closest shape, flat areas a brightness ramp.',
  shape: 'Every cell gets the character whose shape matches it best.',
  ramp: 'Old-school ASCII: characters picked by brightness only.',
};
const CHAR_PRESETS = [
  ['All ASCII', ''],
  ['Classic', ' .:-=+*#%@'],
  ['Blocks', ' ░▒▓█'],
  ['Binary', ' 01'],
];
const DEFAULT_LOOK = { mode: 'color', style: 'hybrid', fontSize: 12, contrast: 2, brightness: 1, saturation: 1.3, chars: '' };
const FONT_MIN = 4;
const FONT_MAX = 64;
const SEEK_STEP = 5;
const HIDE_AFTER = 2500;
const TOAST_TIME = 1600;
const VIDEO_EXT = /\.(mp4|m4v|mov|mkv|webm|avi|wmv|flv|mpe?g|ts|m2ts|mts|3gp|ogv|gif)$/i;
const EXPORT_SIZES = [
  [640, '640 px'],
  [1280, '1280 px (720p)'],
  [1920, '1920 px (1080p)'],
  [2560, '2560 px (1440p)'],
  [3840, '3840 px (4K)'],
];

const $ = (id) => document.getElementById(id);
const el = {
  stage: $('stage'),
  video: $('video'),
  canvas: $('view'),
  empty: $('empty'),
  emptyText: $('empty-text'),
  dropHint: $('drop-hint'),
  choose: $('choose'),
  warn: $('warn'),
  file: $('file'),
  notice: $('notice'),
  noticeText: $('notice-text'),
  noticeBar: $('notice-progress'),
  noticeActions: $('notice-actions'),
  drop: $('drop'),
  title: $('title'),
  stats: $('stats'),
  pill: $('job-pill'),
  bar: $('bar'),
  seek: $('seek'),
  seekTip: $('seek-tip'),
  play: $('play'),
  mute: $('mute'),
  volume: $('volume'),
  timeNow: $('time-now'),
  timeTotal: $('time-total'),
  open: $('open'),
  loop: $('loop'),
  lookBtn: $('look-btn'),
  exportBtn: $('export-btn'),
  helpBtn: $('help-btn'),
  fullscreen: $('fullscreen'),
  toast: $('toast'),
  look: $('look'),
  modeSeg: $('mode-seg'),
  styleSeg: $('style-seg'),
  styleInfo: $('style-info'),
  size: $('look-size'),
  sizeOut: $('look-size-out'),
  edges: $('look-edges'),
  edgesOut: $('look-edges-out'),
  light: $('look-light'),
  lightOut: $('look-light-out'),
  sat: $('look-sat'),
  satOut: $('look-sat-out'),
  chars: $('look-chars'),
  presets: $('look-presets'),
  reset: $('look-reset'),
  help: $('help'),
  exportDlg: $('export'),
  exForm: $('export-form'),
  exFormat: $('ex-format'),
  exStart: $('ex-start'),
  exStartLabel: $('ex-start-label'),
  exEnd: $('ex-end'),
  exEndField: $('ex-end-field'),
  exRangeHelp: $('ex-range-help'),
  exSize: $('ex-size'),
  exDims: $('ex-dims'),
  exLook: $('ex-look'),
  exError: $('ex-error'),
  exFields: $('ex-fields'),
  exProgress: $('ex-progress'),
  exBar: $('ex-bar'),
  exStatus: $('ex-status'),
  exSecondary: $('ex-secondary'),
  exPrimary: $('ex-primary'),
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

const store = {
  get(key, fallback) {
    try {
      const v = JSON.parse(localStorage.getItem(`video-code:${key}`));
      return v ?? fallback;
    } catch {
      return fallback;
    }
  },
  set(key, value) {
    try {
      localStorage.setItem(`video-code:${key}`, JSON.stringify(value));
    } catch {
      // Private mode or storage full: settings just won’t be remembered.
    }
  },
};

const state = {
  look: sanitizeLook({ ...DEFAULT_LOOK, ...store.get('look', {}) }),
  volume: clamp(Number(store.get('volume', 1)), 0, 1),
  muted: Boolean(store.get('muted', false)),
  looping: Boolean(store.get('loop', false)),
  ffmpeg: true,
  file: null,
  name: '',
  /** Bumped on every open, so late results for an old file are ignored. */
  loadId: 0,
  objectUrl: null,
  /** The video element has a picture to draw. */
  ready: false,
  converting: false,
  upload: null,
  glyphKey: '',
  glyphs: null,
  dirty: true,
  newFrame: false,
  lastFrameCallback: -Infinity,
  lastUploadTime: -1,
  scrubbing: false,
  dragging: false,
  showStats: false,
  grid: null,
  fps: { count: 0, since: performance.now(), value: 0 },
  job: null,
  exportFormat: 'mp4',
  exportSize: 1920,
  sizeTouched: false,
};

function sanitizeLook(look) {
  return {
    mode: MODES.includes(look.mode) ? look.mode : DEFAULT_LOOK.mode,
    style: STYLES.includes(look.style) ? look.style : DEFAULT_LOOK.style,
    fontSize: clamp(Number(look.fontSize) || 12, FONT_MIN, FONT_MAX),
    contrast: clamp(Number(look.contrast) || 2, 0.5, 6),
    brightness: clamp(Number(look.brightness) || 1, 0.2, 4),
    saturation: clamp(Number.isFinite(Number(look.saturation)) ? Number(look.saturation) : 1.3, 0, 3),
    chars: typeof look.chars === 'string' ? look.chars : '',
  };
}

const engine = new Engine(el.canvas);
const reducedMotion = matchMedia('(prefers-reduced-motion: reduce)');

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function clamp(v, lo, hi) {
  return Math.min(hi, Math.max(lo, v));
}

function round(v, step) {
  return Math.round(v / step) * step;
}

/** 83.4 -> "01:23", like the desktop player. */
function fmtTime(t) {
  const s = Math.max(0, Math.floor(t || 0));
  const two = (n) => String(n).padStart(2, '0');
  if (s >= 3600) return `${Math.floor(s / 3600)}:${two(Math.floor(s / 60) % 60)}:${two(s % 60)}`;
  return `${two(Math.floor(s / 60))}:${two(s % 60)}`;
}

/** 83.45 -> "1:23.5", for the export fields. */
function fmtPrecise(t) {
  const tenths = Math.round(Math.max(0, t) * 10);
  const s = Math.floor(tenths / 10);
  const frac = tenths % 10;
  const two = (n) => String(n).padStart(2, '0');
  const head = s >= 3600 ? `${Math.floor(s / 3600)}:${two(Math.floor(s / 60) % 60)}` : `${Math.floor(s / 60)}`;
  return `${head}:${two(s % 60)}.${frac}`;
}

/** "1:23.5", "83.5" or "1:02:03" -> seconds, or NaN. */
function parseTime(text) {
  const parts = text.trim().replace(',', '.').split(':');
  if (!parts.length || parts.length > 3 || parts.some((p) => !/^\d*\.?\d*$/.test(p) || p === '')) return NaN;
  return parts.reduce((total, p) => total * 60 + Number(p), 0);
}

const NBSP = ' ';
const oneDecimal = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 });

function fmtBytes(n) {
  if (n >= 1e9) return `${oneDecimal.format(n / 1e9)}${NBSP}GB`;
  if (n >= 1e6) return `${oneDecimal.format(n / 1e6)}${NBSP}MB`;
  if (n >= 1e3) return `${Math.round(n / 1e3)}${NBSP}KB`;
  return `${n}${NBSP}bytes`;
}

function stem(name) {
  return name.replace(/\.[^.]+$/, '') || 'video';
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const OFFLINE = "Can’t reach video-code. Check that it’s still running, then reload this page.";

async function api(path, options = {}) {
  let res;
  try {
    res = await fetch(path, options);
  } catch {
    throw new Error(OFFLINE);
  }
  const body = await res.json().catch(() => null);
  if (!res.ok) throw new Error(body?.error || `${res.status} ${res.statusText}`);
  return body;
}

function download(href, name) {
  const a = document.createElement('a');
  a.href = href;
  a.download = name;
  document.body.append(a);
  a.click();
  a.remove();
}

// ---------------------------------------------------------------------------
// Glyphs
// ---------------------------------------------------------------------------

const glyphCache = new Map();
let glyphRequest = 0;

async function loadGlyphs() {
  const px = Math.round(state.look.fontSize * 100) / 100;
  const key = `${px}|${state.look.chars}`;
  if (key === state.glyphKey) return;
  const id = ++glyphRequest;
  let glyphs = glyphCache.get(key);
  if (!glyphs) {
    try {
      const res = await fetch(`/api/glyphs?px=${px}&chars=${encodeURIComponent(state.look.chars)}`);
      if (!res.ok) throw new Error(`${res.status}`);
      glyphs = parseGlyphs(await res.arrayBuffer());
      glyphCache.set(key, glyphs);
    } catch {
      if (id === glyphRequest) showNotice(OFFLINE);
      return;
    }
  }
  if (id !== glyphRequest) return;
  engine.setGlyphs(glyphs);
  state.glyphs = glyphs;
  state.glyphKey = key;
  state.dirty = true;
  updateExportDims();
}

// ---------------------------------------------------------------------------
// Drawing loop
// ---------------------------------------------------------------------------

const startTime = performance.now();

function frame(now) {
  requestAnimationFrame(frame);
  let source = null;
  if (state.file) {
    const v = el.video;
    if (state.ready && v.readyState >= 2) {
      // New frames are announced by requestVideoFrameCallback. If that goes
      // quiet while playing (or doesn’t exist), follow the clock instead.
      const quiet = now - state.lastFrameCallback > 250;
      const fresh = state.newFrame || (!v.paused && quiet && v.currentTime !== state.lastUploadTime);
      if (fresh || state.dirty) {
        source = { video: v, upload: fresh };
        if (fresh) state.lastUploadTime = v.currentTime;
      }
    }
  } else if (state.dirty || !reducedMotion.matches) {
    const t = reducedMotion.matches ? 4 : (now - startTime) / 1000;
    source = { plasma: t };
  }

  if (source) {
    const look = state.dragging && !state.file ? { ...state.look, brightness: state.look.brightness * 1.7 } : state.look;
    const grid = engine.render(source, look);
    if (grid) {
      state.grid = grid;
      state.newFrame = false;
      state.dirty = false;
      tickFps(now);
      if (state.showStats) renderStats();
    }
  }
  if (state.file && !state.scrubbing) syncTime();
}

function tickFps(now) {
  const f = state.fps;
  f.count++;
  if (now - f.since >= 500) {
    f.value = (f.count * 1000) / (now - f.since);
    f.count = 0;
    f.since = now;
  }
}

function watchFrames() {
  if (!('requestVideoFrameCallback' in el.video)) return;
  const onFrame = (now) => {
    state.newFrame = true;
    state.lastFrameCallback = now;
    el.video.requestVideoFrameCallback(onFrame);
  };
  el.video.requestVideoFrameCallback(onFrame);
}

function resizeCanvas(width, height) {
  if (el.canvas.width !== width || el.canvas.height !== height) {
    el.canvas.width = Math.max(1, width);
    el.canvas.height = Math.max(1, height);
    state.dirty = true;
  }
}

new ResizeObserver(([entry]) => {
  const box = entry.devicePixelContentBoxSize?.[0];
  if (box) resizeCanvas(box.inlineSize, box.blockSize);
  else {
    const dpr = devicePixelRatio || 1;
    resizeCanvas(Math.round(entry.contentRect.width * dpr), Math.round(entry.contentRect.height * dpr));
  }
}).observe(el.canvas);

// ---------------------------------------------------------------------------
// Opening files
// ---------------------------------------------------------------------------

function openPicker() {
  el.file.value = '';
  el.file.click();
}

function looksLikeVideo(file) {
  return file.type.startsWith('video/') || file.type === 'image/gif' || VIDEO_EXT.test(file.name);
}

function openFile(file) {
  if (!file) return;
  if (!looksLikeVideo(file) && !confirm(`${file.name} doesn’t look like a video. Try to open it anyway?`)) return;
  closeVideo();
  state.loadId++;
  state.file = file;
  state.name = file.name;
  document.title = `${file.name} - video-code`;
  el.title.textContent = file.name;
  el.stage.dataset.mode = 'video';
  setSource(URL.createObjectURL(file), true);
  updateControls();
  poke();
}

function setSource(url, isObjectUrl) {
  if (state.objectUrl) URL.revokeObjectURL(state.objectUrl);
  state.objectUrl = isObjectUrl ? url : null;
  state.ready = false;
  state.dirty = true;
  engine.clear();
  el.video.src = url;
  el.video.load();
  el.video.play().catch((e) => {
    // Autoplay can be blocked until the page gets a click or key press.
    if (e.name === 'NotAllowedError') toast('press Space to play');
    updateControls();
  });
}

function closeVideo() {
  state.loadId++;
  if (state.objectUrl) URL.revokeObjectURL(state.objectUrl);
  state.objectUrl = null;
  el.video.removeAttribute('src');
  el.video.load();
  Object.assign(state, { file: null, name: '', ready: false, converting: false, upload: null, dirty: true });
  el.stage.dataset.mode = 'empty';
  document.title = 'video-code';
  hideNotice();
  updateControls();
}

el.video.addEventListener('loadedmetadata', () => {
  if (!state.file) return;
  if (!el.video.videoWidth) {
    // Some files play their sound but the browser can’t decode the picture (HEVC, ProRes...).
    convertForPreview();
    return;
  }
  el.video.loop = state.looping;
  applyVolume();
  updateExportDims();
});

el.video.addEventListener('loadeddata', () => {
  if (!state.file || !el.video.videoWidth) return;
  state.ready = true;
  state.newFrame = true;
  hideNotice();
  updateControls();
});

el.video.addEventListener('error', () => {
  if (state.file && el.video.getAttribute('src')) convertForPreview();
});

for (const type of ['play', 'pause', 'ended', 'durationchange', 'volumechange']) {
  el.video.addEventListener(type, updateControls);
}
el.video.addEventListener('seeked', () => {
  state.newFrame = true;
});
el.video.addEventListener('pause', () => poke());
el.video.addEventListener('ended', () => poke());

/** Makes an H.264 copy of a file the browser can’t play, and plays that instead. */
async function convertForPreview() {
  if (state.converting) {
    showNotice(`Your browser can’t play ${state.name}, even after converting it.`, {
      actions: [['Choose another video', openPicker]],
    });
    return;
  }
  const loadId = state.loadId;
  state.converting = true;
  state.ready = false;
  if (!state.ffmpeg) {
    showNotice(`Your browser can’t play ${state.name}. Install FFmpeg and restart video-code to convert it.`, {
      actions: [['Choose another video', openPicker]],
    });
    return;
  }
  const cancel = () => {
    if (job) api(`/api/jobs/${job.id}/cancel`, { method: 'POST' }).catch(() => {});
    closeVideo();
  };
  let job = null;
  try {
    showNotice(`Your browser can’t play ${state.name}. Copying it to video-code to convert it.`, {
      progress: 0,
      actions: [['Cancel', cancel]],
    });
    const upload = await ensureUpload((p) => {
      if (loadId === state.loadId) setNoticeProgress(p);
    });
    if (loadId !== state.loadId) return;
    job = await api('/api/jobs', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ upload: upload.id, kind: 'preview' }),
    });
    showNotice(`Converting ${state.name} into a format your browser can play.`, {
      progress: 0,
      actions: [['Cancel', cancel]],
    });
    const done = await pollJob(job.id, (s) => {
      if (loadId === state.loadId) setNoticeProgress(s.total ? s.frames / s.total : null);
    });
    if (loadId !== state.loadId) return;
    if (done.state !== 'done') throw new Error(done.error || 'Converting stopped.');
    showNotice(`Loading ${state.name}…`);
    setSource(`/api/jobs/${job.id}/file?inline=1`, false);
  } catch (e) {
    if (loadId === state.loadId) {
      showNotice(e.message, { actions: [['Choose another video', openPicker]] });
    }
  }
}

/** Copies the open file to the server once; exports and conversions reuse it. */
function ensureUpload(onProgress) {
  if (state.upload) {
    state.upload.listeners.add(onProgress);
    return state.upload.promise;
  }
  const file = state.file;
  const listeners = new Set([onProgress]);
  const promise = new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open('POST', '/api/upload');
    xhr.setRequestHeader('X-File-Name', encodeURIComponent(file.name));
    xhr.upload.addEventListener('progress', (e) => {
      if (e.lengthComputable) listeners.forEach((fn) => fn(e.loaded / e.total));
    });
    xhr.addEventListener('load', () => {
      let body = null;
      try {
        body = JSON.parse(xhr.responseText);
      } catch {
        // handled below
      }
      if (xhr.status >= 200 && xhr.status < 300 && body) resolve(body);
      else reject(new Error(body?.error || `Copying the video failed (${xhr.status}).`));
    });
    xhr.addEventListener('error', () => reject(new Error(OFFLINE)));
    xhr.send(file);
  });
  const entry = { promise, listeners };
  state.upload = entry;
  promise.catch(() => {
    if (state.upload === entry) state.upload = null;
  });
  return promise;
}

async function pollJob(id, onUpdate) {
  for (;;) {
    const status = await api(`/api/jobs/${id}`);
    onUpdate(status);
    if (status.state !== 'running') return status;
    await sleep(300);
  }
}

// ---------------------------------------------------------------------------
// Notices and toasts
// ---------------------------------------------------------------------------

function showNotice(text, { progress, actions = [] } = {}) {
  el.noticeText.textContent = text;
  el.noticeBar.hidden = progress === undefined;
  setNoticeProgress(progress ?? 0);
  el.noticeActions.replaceChildren(
    ...actions.map(([label, fn], i) => {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = i === 0 && label !== 'Cancel' ? 'btn btn-primary' : 'btn';
      b.textContent = label;
      b.addEventListener('click', fn);
      return b;
    }),
  );
  el.notice.hidden = false;
}

function setNoticeProgress(fraction) {
  const fill = el.noticeBar.firstElementChild;
  el.noticeBar.classList.toggle('indeterminate', fraction === null);
  fill.style.setProperty('--f', String(clamp(fraction ?? 0, 0, 1)));
}

function hideNotice() {
  el.notice.hidden = true;
}

let toastTimer = 0;
function toast(text) {
  el.toast.textContent = text;
  el.toast.classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => el.toast.classList.remove('show'), TOAST_TIME);
}

// ---------------------------------------------------------------------------
// Controls
// ---------------------------------------------------------------------------

let hideTimer = 0;

/** Shows the controls and restarts their hide timer. */
function poke() {
  el.stage.classList.remove('idle');
  clearTimeout(hideTimer);
  hideTimer = setTimeout(() => {
    if (canHideControls()) el.stage.classList.add('idle');
  }, HIDE_AFTER);
}

function canHideControls() {
  return (
    state.file &&
    state.ready &&
    !el.video.paused &&
    !state.scrubbing &&
    !el.bar.matches(':hover') &&
    !el.bar.contains(document.activeElement) &&
    !el.look.matches(':popover-open')
  );
}

function setIcon(button, name) {
  button.querySelector('use').setAttribute('href', `#i-${name}`);
}

function updateControls() {
  const v = el.video;
  const hasVideo = Boolean(state.file);
  const playing = hasVideo && !v.paused && !v.ended;
  const label = v.ended ? 'Replay' : playing ? 'Pause' : 'Play';
  setIcon(el.play, v.ended ? 'replay' : playing ? 'pause' : 'play');
  el.play.setAttribute('aria-label', label);
  el.play.title = `${label} (Space)`;

  const quiet = state.muted || state.volume === 0;
  setIcon(el.mute, quiet ? 'mute' : 'volume');
  el.mute.setAttribute('aria-label', quiet ? 'Unmute' : 'Mute');
  el.mute.title = `${quiet ? 'Unmute' : 'Mute'} (M)`;
  el.volume.value = state.muted ? 0 : state.volume;
  el.volume.style.setProperty('--p', `${(state.muted ? 0 : state.volume) * 100}%`);

  el.loop.setAttribute('aria-pressed', String(state.looping));
  el.timeTotal.textContent = fmtTime(v.duration);
  el.exportBtn.disabled = !hasVideo;
  if (state.showStats) renderStats();
  syncTime(true);
}

function syncTime(force = false) {
  const d = el.video.duration;
  const t = el.video.currentTime;
  if (!Number.isFinite(d) || d <= 0) {
    el.seek.value = 0;
    el.seek.style.setProperty('--p', '0%');
    el.timeNow.textContent = fmtTime(t);
    return;
  }
  const value = Math.round((t / d) * 1000);
  if (force || Number(el.seek.value) !== value) {
    el.seek.value = value;
    el.seek.style.setProperty('--p', `${value / 10}%`);
    el.seek.setAttribute('aria-valuetext', `${fmtTime(t)} of ${fmtTime(d)}`);
  }
  const now = fmtTime(t);
  if (el.timeNow.textContent !== now) el.timeNow.textContent = now;
}

function togglePlay() {
  if (!state.file) return;
  const v = el.video;
  if (v.paused || v.ended) v.play().catch(() => {});
  else v.pause();
}

function seekTo(t) {
  const v = el.video;
  if (!state.file || !Number.isFinite(v.duration)) return;
  v.currentTime = clamp(t, 0, Math.max(0, v.duration - 0.05));
  syncTime(true);
}

function seekBy(delta) {
  seekTo(el.video.currentTime + delta);
}

function applyVolume() {
  el.video.volume = state.volume;
  el.video.muted = state.muted;
  store.set('volume', state.volume);
  store.set('muted', state.muted);
}

function changeVolume(delta) {
  state.volume = clamp(round(state.volume + delta, 0.05), 0, 1);
  state.muted = false;
  applyVolume();
  updateControls();
  toast(`volume ${Math.round(state.volume * 100)}%`);
}

function toggleMute() {
  state.muted = !state.muted;
  if (!state.muted && state.volume === 0) state.volume = 0.5;
  applyVolume();
  updateControls();
  toast(state.muted ? 'muted' : 'sound on');
}

function toggleLoop() {
  state.looping = !state.looping;
  el.video.loop = state.looping;
  store.set('loop', state.looping);
  updateControls();
  toast(`loop ${state.looping ? 'on' : 'off'}`);
}

function toggleFullscreen() {
  if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  else document.documentElement.requestFullscreen().catch(() => toast("fullscreen isn’t available here"));
}

document.addEventListener('fullscreenchange', () => {
  const on = Boolean(document.fullscreenElement);
  setIcon(el.fullscreen, on ? 'fullscreen-exit' : 'fullscreen');
  el.fullscreen.setAttribute('aria-label', on ? 'Exit fullscreen' : 'Fullscreen');
  el.fullscreen.title = `${on ? 'Exit fullscreen' : 'Fullscreen'} (F)`;
});

function toggleStats() {
  state.showStats = !state.showStats;
  el.stats.hidden = !state.showStats;
  renderStats();
}

function renderStats() {
  const g = state.grid;
  if (!g) return;
  const paused = state.file && el.video.paused;
  const parts = [
    `${g.cols} x ${g.rows} characters`,
    `${g.cw} x ${g.ch} px cells`,
    paused ? 'paused' : `${Math.round(state.fps.value)} fps`,
  ];
  if (state.file) parts.push(`source ${g.srcW} x ${g.srcH}`);
  const text = parts.join('  |  ');
  if (el.stats.textContent !== text) el.stats.textContent = text;
}

async function saveSnapshot() {
  const source = state.file ? (state.ready ? { video: el.video, upload: false } : null) : { plasma: 4 };
  if (!source) return;
  const blob = await engine.snapshot(source, state.look);
  if (!blob) return;
  const t = state.file ? el.video.currentTime : 0;
  const s = Math.floor(t);
  const name = `${state.file ? stem(state.name) : 'video-code'}-frame-${String(Math.floor(s / 60)).padStart(2, '0')}-${String(s % 60).padStart(2, '0')}.png`;
  const url = URL.createObjectURL(blob);
  download(url, name);
  setTimeout(() => URL.revokeObjectURL(url), 10_000);
  toast(`saved ${name}`);
}

// Seek bar
el.seek.addEventListener('input', () => {
  const d = el.video.duration;
  if (!Number.isFinite(d)) return;
  const t = (Number(el.seek.value) / 1000) * d;
  el.seek.style.setProperty('--p', `${Number(el.seek.value) / 10}%`);
  el.timeNow.textContent = fmtTime(t);
  if (typeof el.video.fastSeek === 'function' && state.scrubbing) el.video.fastSeek(t);
  else el.video.currentTime = t;
});
el.seek.addEventListener('pointerdown', () => {
  state.scrubbing = true;
});
el.seek.addEventListener('change', () => {
  state.scrubbing = false;
  const d = el.video.duration;
  if (Number.isFinite(d)) el.video.currentTime = (Number(el.seek.value) / 1000) * d;
});
window.addEventListener('pointerup', () => {
  state.scrubbing = false;
});
el.seek.addEventListener('pointermove', (e) => {
  const d = el.video.duration;
  if (!Number.isFinite(d)) return;
  const r = el.seek.getBoundingClientRect();
  const frac = clamp((e.clientX - r.left) / r.width, 0, 1);
  el.seekTip.textContent = fmtTime(frac * d);
  el.seekTip.style.setProperty('--x', `${frac * r.width}px`);
  el.seekTip.hidden = false;
});
el.seek.addEventListener('pointerleave', () => {
  el.seekTip.hidden = true;
});

el.play.addEventListener('click', togglePlay);
el.mute.addEventListener('click', toggleMute);
el.volume.addEventListener('input', () => {
  state.volume = Number(el.volume.value);
  state.muted = state.volume === 0;
  applyVolume();
  updateControls();
});
el.loop.addEventListener('click', toggleLoop);
el.open.addEventListener('click', openPicker);
el.choose.addEventListener('click', openPicker);
el.file.addEventListener('change', () => openFile(el.file.files[0]));
el.fullscreen.addEventListener('click', toggleFullscreen);
el.helpBtn.addEventListener('click', () => toggleHelp());
el.exportBtn.addEventListener('click', () => openExport());
el.pill.addEventListener('click', () => openExport());

// Clicking the picture plays/pauses (a touch first just shows the controls).
el.canvas.addEventListener('click', (e) => {
  if (!state.file) return;
  if (e.pointerType === 'touch' && el.stage.classList.contains('idle')) return;
  togglePlay();
});
el.canvas.addEventListener('dblclick', toggleFullscreen);

// Scrolling over the picture changes the character size, like the desktop player.
let wheelSum = 0;
el.canvas.addEventListener(
  'wheel',
  (e) => {
    e.preventDefault();
    wheelSum += e.deltaMode === 1 ? e.deltaY * 40 : e.deltaY;
    if (Math.abs(wheelSum) >= 60) {
      zoom(wheelSum < 0 ? 1 : -1);
      wheelSum = 0;
    }
  },
  { passive: false },
);

for (const type of ['pointermove', 'pointerdown']) el.stage.addEventListener(type, poke);
el.bar.addEventListener('focusin', poke);

// After a mouse click, give focus back to the page so Space still means play/pause
// (keyboard users keep focus where it is).
el.bar.addEventListener('pointerup', (e) => {
  if (e.pointerType !== 'mouse') return;
  setTimeout(() => {
    const active = document.activeElement;
    if (active instanceof HTMLElement && el.bar.contains(active) && active !== el.lookBtn) active.blur();
  }, 0);
});

// ---------------------------------------------------------------------------
// Drag and drop
// ---------------------------------------------------------------------------

let dragDepth = 0;
const hasFiles = (e) => [...(e.dataTransfer?.types || [])].includes('Files');

function setDragging(on) {
  state.dragging = on;
  state.dirty = true;
  el.drop.hidden = !on || !state.file;
  el.stage.classList.toggle('dragging', on);
  el.dropHint.textContent = on ? 'Release to play it' : 'or drop one anywhere on this page';
}

window.addEventListener('dragenter', (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  dragDepth++;
  setDragging(true);
});
window.addEventListener('dragover', (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = 'copy';
});
window.addEventListener('dragleave', (e) => {
  if (!hasFiles(e)) return;
  dragDepth = Math.max(0, dragDepth - 1);
  if (dragDepth === 0) setDragging(false);
});
window.addEventListener('drop', (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  dragDepth = 0;
  setDragging(false);
  if (el.exportDlg.open && state.job?.state === 'running') {
    toast('wait for the export to finish first');
    return;
  }
  el.help.close();
  el.exportDlg.close();
  openFile(e.dataTransfer.files[0]);
});

// ---------------------------------------------------------------------------
// Look
// ---------------------------------------------------------------------------

function setLook(patch, message) {
  const before = state.look;
  state.look = sanitizeLook({ ...state.look, ...patch });
  store.set('look', state.look);
  state.dirty = true;
  if (before.fontSize !== state.look.fontSize || before.chars !== state.look.chars) loadGlyphs();
  syncLookPanel();
  updateExportLook();
  if (message) toast(message(state.look));
}

function zoom(steps) {
  const px = clamp(state.look.fontSize * 1.15 ** steps, FONT_MIN, FONT_MAX);
  if (Math.abs(px - state.look.fontSize) < 0.01) return;
  setLook({ fontSize: px }, (l) => `font ${Math.round(l.fontSize)}px`);
}

function cycle(list, value) {
  return list[(list.indexOf(value) + 1) % list.length];
}

function buildSegment(container, name, values, current, onPick) {
  container.replaceChildren(
    ...values.map(([value, label]) => {
      const wrap = document.createElement('label');
      wrap.className = 'seg-item';
      wrap.dataset.value = value;
      const input = document.createElement('input');
      input.type = 'radio';
      input.name = name;
      input.value = value;
      input.checked = value === current;
      input.addEventListener('change', () => input.checked && onPick(value));
      const span = document.createElement('span');
      span.textContent = label;
      wrap.append(input, span);
      return wrap;
    }),
  );
}

function setSegment(container, value) {
  for (const input of container.querySelectorAll('input')) input.checked = input.value === value;
}

buildSegment(el.modeSeg, 'mode', MODES.map((m) => [m, m]), state.look.mode, (mode) => setLook({ mode }));
buildSegment(el.styleSeg, 'style', STYLES.map((s) => [s, s]), state.look.style, (style) => setLook({ style }));
el.presets.replaceChildren(
  ...CHAR_PRESETS.map(([label, chars]) => {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = 'chip';
    b.textContent = label;
    b.dataset.chars = chars;
    b.addEventListener('click', () => setLook({ chars }));
    return b;
  }),
);

const sliders = [
  [el.size, el.sizeOut, 'fontSize', (v) => `${Math.round(v)}${NBSP}px`],
  [el.edges, el.edgesOut, 'contrast', (v) => v.toFixed(2)],
  [el.light, el.lightOut, 'brightness', (v) => v.toFixed(1)],
  [el.sat, el.satOut, 'saturation', (v) => v.toFixed(1)],
];
for (const [input, , key] of sliders) {
  input.addEventListener('input', () => setLook({ [key]: Number(input.value) }));
}

let charsTimer = 0;
el.chars.addEventListener('input', () => {
  clearTimeout(charsTimer);
  charsTimer = setTimeout(() => setLook({ chars: el.chars.value }), 250);
});

el.reset.addEventListener('click', () => setLook({ ...DEFAULT_LOOK }, () => 'look reset'));

function syncLookPanel() {
  const l = state.look;
  setSegment(el.modeSeg, l.mode);
  setSegment(el.styleSeg, l.style);
  el.styleInfo.textContent = STYLE_INFO[l.style];
  for (const [input, out, key, format] of sliders) {
    if (document.activeElement !== input) input.value = l[key];
    out.textContent = format(l[key]);
    const min = Number(input.min);
    const max = Number(input.max);
    input.style.setProperty('--p', `${((clamp(l[key], min, max) - min) / (max - min)) * 100}%`);
  }
  if (document.activeElement !== el.chars) el.chars.value = l.chars;
  for (const chip of el.presets.children) chip.setAttribute('aria-pressed', String(chip.dataset.chars === l.chars));
}

el.look.addEventListener('toggle', (e) => {
  el.lookBtn.setAttribute('aria-expanded', String(e.newState === 'open'));
  poke();
});

// ---------------------------------------------------------------------------
// Help
// ---------------------------------------------------------------------------

function toggleHelp() {
  if (el.help.open) el.help.close();
  else {
    el.look.hidePopover?.();
    el.help.showModal();
  }
}

for (const dialog of [el.help, el.exportDlg]) {
  dialog.addEventListener('click', (e) => {
    // A click on the dimmed backdrop lands on the dialog element itself.
    if (e.target === dialog) dialog.close();
  });
  for (const b of dialog.querySelectorAll('[data-close]')) b.addEventListener('click', () => dialog.close());
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

const FORMATS = [
  ['mp4', 'MP4'],
  ['webm', 'WebM'],
  ['gif', 'GIF'],
  ['png', 'PNG frame'],
];

buildSegment(el.exFormat, 'format', FORMATS, state.exportFormat, (format) => {
  storeExportFields();
  state.exportFormat = format;
  if (!state.sizeTouched) state.exportSize = format === 'gif' ? 640 : 1920;
  el.exSize.value = state.exportSize;
  loadExportFields();
  syncExportForm();
});

// A video export keeps its start/end; a PNG frame has its own time.
const fields = { start: '', end: '', frameAt: '', forLoad: -1 };

function storeExportFields() {
  if (state.exportFormat === 'png') fields.frameAt = el.exStart.value;
  else {
    fields.start = el.exStart.value;
    fields.end = el.exEnd.value;
  }
}

function loadExportFields() {
  el.exStart.value = state.exportFormat === 'png' ? fields.frameAt : fields.start;
  el.exEnd.value = fields.end;
  el.exError.hidden = true;
  el.exStart.removeAttribute('aria-invalid');
  el.exEnd.removeAttribute('aria-invalid');
}

el.exSize.replaceChildren(
  ...EXPORT_SIZES.map(([value, label]) => {
    const o = document.createElement('option');
    o.value = value;
    o.textContent = label;
    return o;
  }),
);
el.exSize.value = state.exportSize;
el.exSize.addEventListener('change', () => {
  state.exportSize = Number(el.exSize.value);
  state.sizeTouched = true;
  updateExportDims();
});

for (const b of el.exForm.querySelectorAll('[data-now]')) {
  b.addEventListener('click', () => {
    $(b.dataset.now).value = fmtPrecise(el.video.currentTime);
    validateRange();
  });
}
for (const input of [el.exStart, el.exEnd]) input.addEventListener('blur', () => validateRange());

function openExport() {
  if (!state.file) {
    toast('open a video first');
    return;
  }
  el.look.hidePopover?.();
  el.help.close();
  // A running or finished export keeps its progress view.
  if (!state.job) {
    resetExportView();
    if (fields.forLoad !== state.loadId) {
      fields.forLoad = state.loadId;
      fields.start = fmtPrecise(0);
      fields.end = Number.isFinite(el.video.duration) ? fmtPrecise(el.video.duration) : '';
    }
    fields.frameAt = fmtPrecise(el.video.currentTime);
    loadExportFields();
  }
  syncExportForm();
  el.exportDlg.showModal();
}

function syncExportForm() {
  const png = state.exportFormat === 'png';
  el.exStartLabel.textContent = png ? 'Frame at' : 'Start';
  el.exEndField.hidden = png;
  el.exRangeHelp.textContent = png
    ? 'Times are minutes:seconds. Now uses where the video is paused.'
    : 'The whole video unless you change these. Times are minutes:seconds.';
  updateExportDims();
  updateExportLook();
  if (!state.job || state.job.state !== 'running') {
    el.exPrimary.textContent = `Export ${FORMATS.find(([f]) => f === state.exportFormat)[1]}`;
  }
}

function updateExportDims() {
  const g = state.glyphs;
  const vw = el.video.videoWidth;
  const vh = el.video.videoHeight;
  if (!g || !vw || !vh) {
    el.exDims.textContent = '';
    return;
  }
  const aspect = vw / vh;
  const size = Math.max(64, state.exportSize);
  const targetW = aspect >= 1 ? size : size * aspect;
  const cols = Math.max(1, Math.round(targetW / g.cw));
  const rows = Math.max(1, Math.round((cols * g.cw) / aspect / g.ch));
  const even = (x) => x + (x & 1);
  el.exDims.textContent = `${even(cols * g.cw)} x ${even(rows * g.ch)} px, ${cols} x ${rows} characters`;
}

function updateExportLook() {
  const l = state.look;
  const chars = l.chars ? `, characters “${l.chars}”` : '';
  el.exLook.textContent = `Uses the current look: ${l.mode}, ${l.style}, ${Math.round(l.fontSize)}${NBSP}px characters${chars}.`;
}

/** Checks the time fields. Returns { start, end } or null (and shows why). */
function validateRange(focusError = false) {
  const png = state.exportFormat === 'png';
  const d = el.video.duration;
  const start = parseTime(el.exStart.value);
  const end = png ? NaN : parseTime(el.exEnd.value);
  let error = '';
  if (!Number.isFinite(start)) error = `${png ? 'Frame at' : 'Start'} needs a time like 1:05.0 or 65.`;
  else if (Number.isFinite(d) && start >= d) error = `${png ? 'Frame at' : 'Start'} is past the end of the video (${fmtPrecise(d)}).`;
  else if (!png && el.exEnd.value.trim() && !Number.isFinite(end)) error = 'End needs a time like 1:05.0 or 65.';
  else if (!png && Number.isFinite(end) && end <= start) error = 'End has to be after the start.';
  el.exError.textContent = error;
  el.exError.hidden = !error;
  if (error && focusError) (error.startsWith('End') ? el.exEnd : el.exStart).focus();
  el.exStart.setAttribute('aria-invalid', String(error.startsWith('Start') || error.startsWith('Frame')));
  el.exEnd.setAttribute('aria-invalid', String(error.startsWith('End')));
  return error ? null : { start, end: Number.isFinite(end) ? end : null };
}

function resetExportView() {
  el.exFields.hidden = false;
  el.exProgress.hidden = true;
  el.exError.hidden = true;
  el.exSecondary.textContent = 'Cancel';
  el.exPrimary.hidden = false;
  el.exPrimary.disabled = false;
}

function setExportProgress(fraction, text) {
  el.exProgress.hidden = false;
  el.exBar.classList.toggle('indeterminate', fraction === null);
  el.exBar.firstElementChild.style.setProperty('--f', String(clamp(fraction ?? 0, 0, 1)));
  el.exStatus.textContent = text;
}

function updatePill() {
  const job = state.job;
  const show = job && (job.state === 'running' || job.state === 'done') && !el.exportDlg.open;
  el.pill.hidden = !show;
  if (!show) return;
  el.pill.textContent = job.state === 'done' ? `Exported ${job.file}` : `Exporting ${job.percent ?? 0}%`;
}

el.exportDlg.addEventListener('close', () => {
  if (!state.job) storeExportFields();
  if (state.job?.state === 'done') state.job = null;
  updatePill();
});

el.exSecondary.addEventListener('click', () => {
  const job = state.job;
  if (job?.state === 'done') {
    download(`/api/jobs/${job.id}/file`, job.file);
    return;
  }
  if (job?.state !== 'running') {
    el.exportDlg.close();
    return;
  }
  job.cancelRequested = true;
  setExportProgress(null, 'Cancelling…');
  if (job.id) api(`/api/jobs/${job.id}/cancel`, { method: 'POST' }).catch(() => {});
});

el.exForm.addEventListener('submit', async (e) => {
  e.preventDefault();
  if (state.job?.state === 'done') {
    el.exportDlg.close();
    return;
  }
  if (state.job?.state === 'running') return;
  const range = validateRange(true);
  if (!range) return;
  storeExportFields();
  if (!state.ffmpeg) {
    el.exError.textContent = "Exporting needs FFmpeg, and it isn’t installed. Install it, restart video-code and try again.";
    el.exError.hidden = false;
    return;
  }
  await runExport(range);
});

async function runExport(range) {
  const format = state.exportFormat;
  const look = { ...state.look };
  const job = { state: 'running', id: null, file: '', percent: 0 };
  state.job = job;
  el.exFields.hidden = true;
  el.exError.hidden = true;
  el.exPrimary.hidden = true;
  el.exSecondary.textContent = 'Cancel export';
  setExportProgress(0, 'Copying the video to video-code');
  try {
    const upload = await ensureUpload((p) => {
      if (!job.cancelRequested) setExportProgress(p, `Copying the video to video-code: ${Math.round(p * 100)}%`);
    });
    if (job.cancelRequested) {
      finishCancelled();
      return;
    }
    const started = await api('/api/jobs', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({
        upload: upload.id,
        kind: 'export',
        format,
        start: range.start,
        end: range.end,
        size: state.exportSize,
        fontSize: look.fontSize,
        chars: look.chars,
        color: look.mode,
        style: look.style,
        contrast: look.contrast,
        brightness: look.brightness,
        saturation: look.saturation,
      }),
    });
    job.id = started.id;
    if (job.cancelRequested) api(`/api/jobs/${job.id}/cancel`, { method: 'POST' }).catch(() => {});
    else setExportProgress(null, 'Starting…');
    const result = await pollJob(job.id, (s) => {
      job.file = s.file;
      if (s.state !== 'running' || job.cancelRequested) return;
      const frac = s.total ? Math.min(1, s.frames / s.total) : null;
      job.percent = Math.round((frac ?? 0) * 100);
      const speed = s.elapsed > 0.5 ? s.frames / s.elapsed : 0;
      const left = s.total && speed > 0 ? Math.max(0, (s.total - s.frames) / speed) : null;
      const parts = [frac === null ? `frame ${s.frames}` : `${job.percent}%`];
      if (speed) parts.push(`${Math.round(speed)} fps`);
      if (left !== null && s.frames > 5) parts.push(`about ${fmtTime(left)} left`);
      if (frac !== null && frac >= 1) setExportProgress(1, 'Finishing the file…');
      else setExportProgress(frac, `Rendering ${parts.join(', ')}`);
      updatePill();
    });

    if (result.state === 'done') {
      job.state = 'done';
      download(`/api/jobs/${job.id}/file`, result.file);
      setExportProgress(1, `${result.file} is ready (${fmtBytes(result.bytes)}) and downloading.`);
      el.exPrimary.hidden = false;
      el.exPrimary.textContent = 'Done';
      el.exSecondary.textContent = 'Download again';
      if (!el.exportDlg.open) toast(`exported ${result.file}`);
    } else if (result.state === 'cancelled') {
      finishCancelled();
    } else {
      throw new Error(result.error || 'The export failed.');
    }
  } catch (err) {
    state.job = null;
    resetExportView();
    syncExportForm();
    el.exError.textContent = err.message;
    el.exError.hidden = false;
    if (!el.exportDlg.open) toast('export failed: open Export to see why');
  }
  updatePill();
}

function finishCancelled() {
  state.job = null;
  resetExportView();
  syncExportForm();
  updatePill();
  toast('export cancelled');
}

// ---------------------------------------------------------------------------
// Keyboard
// ---------------------------------------------------------------------------

const KEYS = {
  ' ': togglePlay,
  ArrowLeft: () => seekBy(-SEEK_STEP),
  ArrowRight: () => seekBy(SEEK_STEP),
  ArrowUp: () => changeVolume(0.1),
  ArrowDown: () => changeVolume(-0.1),
  Home: () => seekTo(0),
  End: () => seekTo(el.video.duration),
  '+': () => zoom(1),
  '=': () => zoom(1),
  '-': () => zoom(-1),
  _: () => zoom(-1),
  c: () => setLook({ mode: cycle(MODES, state.look.mode) }, (l) => `colour: ${l.mode}`),
  s: () => setLook({ style: cycle(STYLES, state.look.style) }, (l) => `style: ${l.style}`),
  '[': () => setLook({ contrast: state.look.contrast - 0.25 }, (l) => `edges ${l.contrast.toFixed(2)}`),
  '<': () => setLook({ contrast: state.look.contrast - 0.25 }, (l) => `edges ${l.contrast.toFixed(2)}`),
  ']': () => setLook({ contrast: state.look.contrast + 0.25 }, (l) => `edges ${l.contrast.toFixed(2)}`),
  '>': () => setLook({ contrast: state.look.contrast + 0.25 }, (l) => `edges ${l.contrast.toFixed(2)}`),
  ',': () => setLook({ brightness: round(state.look.brightness - 0.1, 0.1) }, (l) => `light ${l.brightness.toFixed(1)}`),
  '.': () => setLook({ brightness: round(state.look.brightness + 0.1, 0.1) }, (l) => `light ${l.brightness.toFixed(1)}`),
  r: () => setLook({ ...DEFAULT_LOOK }, () => 'look reset'),
  m: toggleMute,
  l: toggleLoop,
  f: toggleFullscreen,
  h: toggleStats,
  o: openPicker,
  e: openExport,
  p: saveSnapshot,
  t: () => el.look.togglePopover?.(),
  '?': toggleHelp,
};

window.addEventListener('keydown', (e) => {
  if (e.defaultPrevented || e.isComposing) return;
  const altGr = e.getModifierState?.('AltGraph');
  if ((e.ctrlKey || e.metaKey) && !altGr) {
    if (e.key.toLowerCase() === 'o') {
      e.preventDefault();
      openPicker();
    }
    return;
  }
  if (e.altKey && !altGr) return;

  // While a dialog is open only it gets keys (Esc closes it), except ? for help.
  if (el.exportDlg.open || el.help.open) {
    if (e.key === '?' && el.help.open) {
      e.preventDefault();
      el.help.close();
    }
    return;
  }

  const target = e.target;
  const typing = target instanceof HTMLElement && target.matches('input:not([type=range]):not([type=radio]), textarea, select');
  if (typing) return;
  // Sliders keep their arrow keys, except the seek bar, where arrows jump 5 seconds.
  const onRange = target instanceof HTMLElement && target.matches('input[type=range]') && target !== el.seek;
  if (onRange && ['ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown', 'Home', 'End', 'PageUp', 'PageDown'].includes(e.key)) return;
  const onButton = target instanceof HTMLElement && target.matches('button, a, label, input[type=radio]');
  if (onButton && (e.key === ' ' || e.key === 'Enter')) return;

  if (e.key === 'Escape') {
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    return;
  }

  let action = KEYS[e.key] ?? KEYS[e.key.toLowerCase()];
  if (!action && /^[0-9]$/.test(e.key)) {
    const n = Number(e.key);
    action = () => {
      if (Number.isFinite(el.video.duration)) seekTo((el.video.duration * n) / 10);
    };
  }
  if (!action) return;
  e.preventDefault();
  action();
  poke();
});

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

async function start() {
  el.video.volume = state.volume;
  el.video.muted = state.muted;
  syncLookPanel();
  updateExportLook();
  updateControls();
  watchFrames();

  if (!engine.ok) {
    el.emptyText.textContent = "This browser can’t draw the preview because WebGL 2 is turned off or missing. Try a current Chrome, Edge or Firefox.";
    el.choose.disabled = true;
    return;
  }
  requestAnimationFrame(frame);
  loadGlyphs();

  try {
    const info = await api('/api/info');
    state.ffmpeg = info.ffmpeg;
    if (!info.ffmpeg) {
      el.warn.textContent = "FFmpeg isn’t installed, so exporting won’t work. Install it (winget install Gyan.FFmpeg) and restart video-code.";
      el.warn.hidden = false;
    }
  } catch (e) {
    showNotice(e.message);
  }
}

start();
