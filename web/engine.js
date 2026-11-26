// Draws video frames as ASCII art with WebGL 2.
//
// It runs the same maths as src/ascii.rs, with glyph shapes measured by the
// Rust side (GET /api/glyphs), so the preview picks the same characters as an
// export. Two passes:
//   1. cells: one fragment per character cell measures the six zones of the
//      cell in the video, picks a glyph and works out its colours.
//   2. draw:  one fragment per screen pixel looks up its cell and blends the
//      glyph's coverage between the cell's background and foreground colour.

const MODES = ['color', 'color-bg', 'matrix', 'amber', 'mono'];
const STYLES = ['hybrid', 'shape', 'ramp'];

/** Glyphs per row in the atlas texture. */
const ATLAS_COLS = 16;

/** Longest side of the animated idle picture (it takes the window's shape). */
const PLASMA_SIZE = 320;

const VERT = `#version 300 es
void main() {
  // One triangle that covers the whole target.
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;

// Port of `plasma` in src/player.rs. Row 0 is the top of the picture.
const PLASMA_FRAG = `#version 300 es
precision highp float;
uniform float uTime;
uniform vec2 uSize;
out vec4 outColor;
const float TAU = 6.28318530718;
void main() {
  vec2 f = floor(gl_FragCoord.xy) / uSize;
  float t = uTime;
  float d = length(vec2(f.x - 0.5, (f.y - 0.5) * 0.6));
  float v = (sin(f.x * 9.0 + t) + sin(f.y * 7.0 - t * 0.7)
           + sin((f.x + f.y) * 5.0 + t * 1.3) + sin(d * 22.0 - t * 2.0)) * 0.25;
  float hue = v * 0.5 + t * 0.05;
  float fade = clamp(1.4 - d * 1.5, 0.0, 1.0) * (0.75 + 0.25 * abs(v));
  vec3 c = 0.5 + 0.5 * cos(TAU * (hue + vec3(0.0, 1.0, 2.0) / 3.0));
  outColor = vec4(c * fade, 1.0);
}`;

// Port of `convert` + `shade` + `colorize` in src/ascii.rs.
const CELL_FRAG = `#version 300 es
precision highp float;
precision highp int;
precision highp usampler2D;

uniform sampler2D uSrc;
uniform highp sampler2D uShapes;
uniform usampler2D uRamp;
uniform ivec2 uGrid;
uniform vec2 uSrcSize;
uniform int uCount;
uniform int uMode;
uniform int uStyle;
uniform float uContrast;
uniform float uBrightness;
uniform float uSaturation;

layout(location = 0) out vec4 outFg;
layout(location = 1) out vec4 outBg;

const float GLOW = 0.85;
const float EDGE = 0.14;
const float DITHER = 0.06;
const float GAMMA = 0.8;
const int SAMPLES = 3;

float tone(float v) { return pow(min(v * uBrightness, 1.0), GAMMA); }

// Same integer hash as dither_at, so the dither pattern matches exports.
float ditherAt(uint row, uint col) {
  uint h = (row * 0x9E3779B1u) ^ (col * 0x85EBCA77u);
  h ^= h >> 15u;
  h *= 0x2C1B3C6Du;
  h ^= h >> 12u;
  h *= 0x297A2D39u;
  h ^= h >> 15u;
  return float(h >> 8u) / 16777216.0 - 0.5;
}

vec3 pack(vec3 c) { return floor(clamp(c, 0.0, 255.0)) / 255.0; }

void main() {
  ivec2 cell = ivec2(gl_FragCoord.xy);
  vec2 cellUv = 1.0 / vec2(uGrid);
  vec2 zoneUv = cellUv * vec2(0.5, 1.0 / 3.0);
  vec2 stepUv = zoneUv / float(SAMPLES);
  // Mip level whose texels are about one sample apart: an area average, like the CPU path.
  vec2 texels = stepUv * uSrcSize;
  float lod = max(0.0, log2(max(texels.x, texels.y)));

  float zones[6];
  float mean = 0.0;
  vec3 rgb = vec3(0.0);
  for (int zy = 0; zy < 3; zy++) {
    for (int zx = 0; zx < 2; zx++) {
      vec2 origin = (vec2(cell) + vec2(float(zx) * 0.5, float(zy) / 3.0)) * cellUv;
      vec3 acc = vec3(0.0);
      for (int sy = 0; sy < SAMPLES; sy++) {
        for (int sx = 0; sx < SAMPLES; sx++) {
          acc += textureLod(uSrc, origin + (vec2(float(sx), float(sy)) + 0.5) * stepUv, lod).rgb;
        }
      }
      acc /= float(SAMPLES * SAMPLES);
      float l = dot(acc, vec3(54.0, 183.0, 19.0) / 256.0);
      zones[zy * 2 + zx] = l;
      mean += l;
      rgb += acc;
    }
  }
  mean /= 6.0;
  rgb = rgb / 6.0 * 255.0;

  float light = tone(mean);
  float peak = 0.0;
  float low = 1.0;
  for (int k = 0; k < 6; k++) {
    zones[k] = tone(zones[k]);
    peak = max(peak, zones[k]);
    low = min(low, zones[k]);
  }

  int glyph = 0;
  bool useShape = uStyle == 1 || (uStyle == 0 && peak - low > EDGE);
  if (useShape) {
    if (peak > 1e-3 && uContrast != 1.0) {
      for (int k = 0; k < 6; k++) zones[k] = pow(zones[k] / peak, uContrast) * peak;
    }
    vec4 za = vec4(zones[0], zones[1], zones[2], zones[3]);
    vec2 zb = vec2(zones[4], zones[5]);
    float best = 3.4e38;
    for (int i = 0; i < uCount; i++) {
      vec4 a = texelFetch(uShapes, ivec2(0, i), 0) - za;
      vec2 b = texelFetch(uShapes, ivec2(1, i), 0).xy - zb;
      float d = dot(a, a) + dot(b, b);
      if (d < best) {
        best = d;
        glyph = i;
      }
    }
  } else {
    float x = clamp(light + ditherAt(uint(cell.y), uint(cell.x)) * DITHER, 0.0, 1.0);
    glyph = int(texelFetch(uRamp, ivec2(int(floor(x * 255.0 + 0.5)), 0), 0).r);
  }

  vec3 fg;
  vec3 bg = vec3(0.0);
  if (uMode <= 1) {
    float y = dot(rgb, vec3(0.2126, 0.7152, 0.0722));
    vec3 c = max(vec3(y) + (rgb - y) * uSaturation, 0.0);
    float p = max(max(c.r, c.g), max(c.b, 24.0));
    fg = c * (1.0 + GLOW * (255.0 / p - 1.0));
    if (uMode == 1) bg = c * 0.35 * uBrightness;
  } else if (uMode == 2) {
    fg = vec3(30.0 + 170.0 * light * light, 120.0 + 135.0 * light, 50.0 + 130.0 * light * light);
  } else if (uMode == 3) {
    fg = vec3(210.0 + 45.0 * light, 120.0 + 90.0 * light, 10.0 + 60.0 * light * light);
  } else {
    fg = vec3(150.0 + 105.0 * light);
  }
  outFg = vec4(pack(fg), float(glyph) / 255.0);
  outBg = vec4(pack(bg), 1.0);
}`;

// Port of `render_cells` + `blend` in src/render.rs.
const DRAW_FRAG = `#version 300 es
precision highp float;
precision highp int;
uniform sampler2D uFg;
uniform sampler2D uBg;
uniform sampler2D uAtlas;
uniform ivec2 uCell;
uniform ivec2 uGrid;
uniform ivec2 uOffset;
uniform int uHeight;
out vec4 outColor;
void main() {
  ivec2 p = ivec2(int(gl_FragCoord.x), uHeight - 1 - int(gl_FragCoord.y));
  ivec2 g = p - uOffset;
  ivec2 size = uGrid * uCell;
  if (g.x < 0 || g.y < 0 || g.x >= size.x || g.y >= size.y) {
    outColor = vec4(0.0, 0.0, 0.0, 1.0);
    return;
  }
  ivec2 cell = g / uCell;
  ivec2 local = g - cell * uCell;
  vec4 fg = texelFetch(uFg, cell, 0);
  vec3 bg = texelFetch(uBg, cell, 0).rgb;
  int gi = int(fg.a * 255.0 + 0.5);
  ivec2 a = ivec2((gi % ${ATLAS_COLS}) * uCell.x + local.x, (gi / ${ATLAS_COLS}) * uCell.y + local.y);
  float coverage = texelFetch(uAtlas, a, 0).r;
  outColor = vec4(mix(bg, fg.rgb, coverage), 1.0);
}`;

/** Parses the binary glyph set from GET /api/glyphs (see GlyphSet::to_bytes). */
export function parseGlyphs(buffer) {
  const view = new DataView(buffer);
  const cw = view.getUint32(0, true);
  const ch = view.getUint32(4, true);
  const count = view.getUint32(8, true);
  const ramp = new Uint8Array(256);
  for (let i = 0; i < 256; i++) ramp[i] = view.getUint16(12 + i * 2, true);
  let offset = 12 + 512;
  const shapes = new Float32Array(count * 6);
  for (let i = 0; i < shapes.length; i++) shapes[i] = view.getFloat32(offset + i * 4, true);
  offset += shapes.length * 4;
  const bitmaps = new Uint8Array(buffer, offset, count * cw * ch);
  return { cw, ch, count, ramp, shapes, bitmaps };
}

/** The biggest grid with the picture's aspect ratio that fits, centred (Grid::fit). */
export function fitGrid(aspect, bw, bh, cw, ch) {
  let w = bw;
  let h = bh;
  if (w / h > aspect) w = h * aspect;
  else h = w / aspect;
  const cols = Math.max(1, Math.floor(w / cw));
  const rows = Math.max(1, Math.floor(h / ch));
  return {
    cols,
    rows,
    offX: Math.max(0, Math.floor((bw - cols * cw) / 2)),
    offY: Math.max(0, Math.floor((bh - rows * ch) / 2)),
  };
}

export class Engine {
  constructor(canvas) {
    this.canvas = canvas;
    this.gl = canvas.getContext('webgl2', {
      alpha: false,
      antialias: false,
      depth: false,
      stencil: false,
      premultipliedAlpha: false,
      powerPreference: 'high-performance',
    });
    this.ok = Boolean(this.gl);
    this.glyphs = null;
    this.grid = null;
    if (!this.ok) return;

    canvas.addEventListener('webglcontextlost', (e) => e.preventDefault());
    canvas.addEventListener('webglcontextrestored', () => {
      this.init();
      if (this.glyphs) this.setGlyphs(this.glyphs);
    });
    this.init();
  }

  init() {
    const gl = this.gl;
    this.vao = gl.createVertexArray();
    this.plasma = this.program(PLASMA_FRAG);
    this.cells = this.program(CELL_FRAG);
    this.draw = this.program(DRAW_FRAG);
    this.fbo = gl.createFramebuffer();
    this.src = null;
    this.srcW = 0;
    this.srcH = 0;
    this.cellTex = null;
    this.cellCols = 0;
    this.cellRows = 0;
    this.atlas = null;
    this.shapesTex = null;
    this.rampTex = null;
  }

  program(fragSource) {
    const gl = this.gl;
    const compile = (type, source) => {
      const shader = gl.createShader(type);
      gl.shaderSource(shader, source);
      gl.compileShader(shader);
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
        throw new Error(`shader didn't compile: ${gl.getShaderInfoLog(shader)}`);
      }
      return shader;
    };
    const prog = gl.createProgram();
    gl.attachShader(prog, compile(gl.VERTEX_SHADER, VERT));
    gl.attachShader(prog, compile(gl.FRAGMENT_SHADER, fragSource));
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) {
      throw new Error(`shader didn't link: ${gl.getProgramInfoLog(prog)}`);
    }
    const uniforms = {};
    const n = gl.getProgramParameter(prog, gl.ACTIVE_UNIFORMS);
    for (let i = 0; i < n; i++) {
      const { name } = gl.getActiveUniform(prog, i);
      uniforms[name] = gl.getUniformLocation(prog, name);
    }
    return { prog, u: uniforms };
  }

  texture(w, h, internal, filter, levels = 1) {
    const gl = this.gl;
    const tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.texStorage2D(gl.TEXTURE_2D, levels, internal, w, h);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, filter === gl.LINEAR_MIPMAP_LINEAR ? gl.LINEAR : filter);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return tex;
  }

  /** Uploads a glyph set from parseGlyphs. */
  setGlyphs(glyphs) {
    const gl = this.gl;
    this.glyphs = glyphs;
    if (!this.ok) return;
    const { cw, ch, count, ramp, shapes, bitmaps } = glyphs;
    for (const t of [this.atlas, this.shapesTex, this.rampTex]) if (t) gl.deleteTexture(t);
    gl.pixelStorei(gl.UNPACK_ALIGNMENT, 1);

    const rows = Math.ceil(count / ATLAS_COLS);
    const aw = ATLAS_COLS * cw;
    const ah = rows * ch;
    const atlas = new Uint8Array(aw * ah);
    for (let g = 0; g < count; g++) {
      const x0 = (g % ATLAS_COLS) * cw;
      const y0 = Math.floor(g / ATLAS_COLS) * ch;
      for (let y = 0; y < ch; y++) {
        atlas.set(bitmaps.subarray((g * ch + y) * cw, (g * ch + y + 1) * cw), (y0 + y) * aw + x0);
      }
    }
    this.atlas = this.texture(aw, ah, gl.R8, gl.NEAREST);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, aw, ah, gl.RED, gl.UNSIGNED_BYTE, atlas);

    const packed = new Float32Array(count * 8);
    for (let i = 0; i < count; i++) packed.set(shapes.subarray(i * 6, i * 6 + 6), i * 8);
    this.shapesTex = this.texture(2, count, gl.RGBA32F, gl.NEAREST);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, 2, count, gl.RGBA, gl.FLOAT, packed);

    this.rampTex = this.texture(256, 1, gl.R8UI, gl.NEAREST);
    gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, 256, 1, gl.RED_INTEGER, gl.UNSIGNED_BYTE, ramp);
  }

  ensureSource(w, h) {
    if (this.src && this.srcW === w && this.srcH === h) return;
    const gl = this.gl;
    if (this.src) gl.deleteTexture(this.src);
    const levels = Math.floor(Math.log2(Math.max(w, h))) + 1;
    this.src = this.texture(w, h, gl.RGBA8, gl.LINEAR_MIPMAP_LINEAR, levels);
    this.srcW = w;
    this.srcH = h;
  }

  ensureCells(cols, rows) {
    if (this.cellTex && this.cellCols === cols && this.cellRows === rows) return;
    const gl = this.gl;
    if (this.cellTex) this.cellTex.forEach((t) => gl.deleteTexture(t));
    this.cellTex = [this.texture(cols, rows, gl.RGBA8, gl.NEAREST), this.texture(cols, rows, gl.RGBA8, gl.NEAREST)];
    this.cellCols = cols;
    this.cellRows = rows;
  }

  /**
   * Draws one frame. `source` is { video, upload } to draw a video element
   * (upload: copy its current frame first), or { plasma: seconds } for the
   * idle animation. Returns the character grid, or null if nothing was drawn.
   */
  render(source, look) {
    const gl = this.gl;
    if (!this.ok || !this.glyphs || gl.isContextLost()) return null;
    const { cw, ch, count } = this.glyphs;
    const W = this.canvas.width;
    const H = this.canvas.height;
    if (!W || !H) return null;

    gl.bindVertexArray(this.vao);
    let sw;
    let sh;
    if (source.video) {
      sw = source.video.videoWidth;
      sh = source.video.videoHeight;
      if (!sw || !sh) return null;
      const fresh = !this.src || this.srcW !== sw || this.srcH !== sh;
      this.ensureSource(sw, sh);
      if (source.upload || fresh) {
        gl.bindTexture(gl.TEXTURE_2D, this.src);
        gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
        gl.texSubImage2D(gl.TEXTURE_2D, 0, 0, 0, gl.RGBA, gl.UNSIGNED_BYTE, source.video);
        gl.generateMipmap(gl.TEXTURE_2D);
      }
    } else {
      sw = W >= H ? PLASMA_SIZE : Math.max(16, Math.round((PLASMA_SIZE * W) / H));
      sh = W >= H ? Math.max(16, Math.round((PLASMA_SIZE * H) / W)) : PLASMA_SIZE;
      this.ensureSource(sw, sh);
      gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.src, 0);
      gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT1, gl.TEXTURE_2D, null, 0);
      gl.drawBuffers([gl.COLOR_ATTACHMENT0]);
      gl.viewport(0, 0, sw, sh);
      gl.useProgram(this.plasma.prog);
      gl.uniform1f(this.plasma.u.uTime, source.plasma);
      gl.uniform2f(this.plasma.u.uSize, sw, sh);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      gl.bindTexture(gl.TEXTURE_2D, this.src);
      gl.generateMipmap(gl.TEXTURE_2D);
    }

    const grid = fitGrid(sw / sh, W, H, cw, ch);
    this.ensureCells(grid.cols, grid.rows);

    // Pass 1: characters and colours, one fragment per cell.
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.fbo);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT0, gl.TEXTURE_2D, this.cellTex[0], 0);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.COLOR_ATTACHMENT1, gl.TEXTURE_2D, this.cellTex[1], 0);
    gl.drawBuffers([gl.COLOR_ATTACHMENT0, gl.COLOR_ATTACHMENT1]);
    gl.viewport(0, 0, grid.cols, grid.rows);
    const c = this.cells;
    gl.useProgram(c.prog);
    this.bind(0, this.src, c.u.uSrc);
    this.bind(1, this.shapesTex, c.u.uShapes);
    this.bind(2, this.rampTex, c.u.uRamp);
    gl.uniform2i(c.u.uGrid, grid.cols, grid.rows);
    gl.uniform2f(c.u.uSrcSize, sw, sh);
    gl.uniform1i(c.u.uCount, count);
    gl.uniform1i(c.u.uMode, Math.max(0, MODES.indexOf(look.mode)));
    gl.uniform1i(c.u.uStyle, Math.max(0, STYLES.indexOf(look.style)));
    gl.uniform1f(c.u.uContrast, look.contrast);
    gl.uniform1f(c.u.uBrightness, look.brightness);
    gl.uniform1f(c.u.uSaturation, look.saturation);
    gl.drawArrays(gl.TRIANGLES, 0, 3);

    // Pass 2: glyph pixels, one fragment per screen pixel.
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, W, H);
    const d = this.draw;
    gl.useProgram(d.prog);
    this.bind(0, this.cellTex[0], d.u.uFg);
    this.bind(1, this.cellTex[1], d.u.uBg);
    this.bind(2, this.atlas, d.u.uAtlas);
    gl.uniform2i(d.u.uCell, cw, ch);
    gl.uniform2i(d.u.uGrid, grid.cols, grid.rows);
    gl.uniform2i(d.u.uOffset, grid.offX, grid.offY);
    gl.uniform1i(d.u.uHeight, H);
    gl.drawArrays(gl.TRIANGLES, 0, 3);

    this.grid = { ...grid, cw, ch, srcW: sw, srcH: sh };
    return this.grid;
  }

  /** Paints the canvas black (while a video is loading). */
  clear() {
    const gl = this.gl;
    if (!this.ok || gl.isContextLost()) return;
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.clearColor(0, 0, 0, 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
  }

  bind(unit, tex, location) {
    const gl = this.gl;
    gl.activeTexture(gl.TEXTURE0 + unit);
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.uniform1i(location, unit);
  }

  /** Draws a frame and returns just the character grid as a PNG blob. */
  snapshot(source, look) {
    const grid = this.render(source, look);
    if (!grid) return Promise.resolve(null);
    const w = grid.cols * grid.cw;
    const h = grid.rows * grid.ch;
    const out = document.createElement('canvas');
    out.width = w;
    out.height = h;
    out.getContext('2d').drawImage(this.canvas, grid.offX, grid.offY, w, h, 0, 0, w, h);
    return new Promise((resolve) => out.toBlob(resolve, 'image/png'));
  }
}
