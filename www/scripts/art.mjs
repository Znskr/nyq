// Draws the site's soft black-and-white pixel art: the hero night scene and the three
// feature illustrations. Run: node scripts/art.mjs
// Everything is drawn at low resolution, then scaled up 4x with hard edges.
import sharp from "sharp";
import { drawText, textWidth } from "./pixelfont.mjs";

const SCALE = 4;

/** A seeded PRNG, so the art is the same on every run. */
function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6d2b79f5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const BAYER = [
  [0, 8, 2, 10],
  [12, 4, 14, 6],
  [3, 11, 1, 9],
  [15, 7, 13, 5],
];

class Canvas {
  constructor(w, h) {
    this.w = w;
    this.h = h;
    this.px = new Uint8Array(w * h);
  }
  get(x, y) {
    return this.px[Math.max(0, Math.min(this.h - 1, y)) * this.w + Math.max(0, Math.min(this.w - 1, x))];
  }
  set(x, y, v) {
    x = Math.round(x);
    y = Math.round(y);
    if (x < 0 || y < 0 || x >= this.w || y >= this.h) return;
    this.px[y * this.w + x] = Math.max(0, Math.min(255, Math.round(v)));
  }
  rect(x, y, w, h, v) {
    for (let j = 0; j < h; j++) for (let i = 0; i < w; i++) this.set(x + i, y + j, v);
  }
  /** Ordered dither between a and b: t = 0 is all a, t = 1 is all b. */
  dither(x, y, a, b, t) {
    this.set(x, y, t * 16 > BAYER[y & 3][x & 3] + 0.5 ? b : a);
  }
  /** A box with a 1px border; `fill` may be null to keep what is behind. */
  box(x, y, w, h, border, fill) {
    if (fill != null) this.rect(x, y, w, h, fill);
    this.rect(x, y, w, 1, border);
    this.rect(x, y + h - 1, w, 1, border);
    this.rect(x, y, 1, h, border);
    this.rect(x + w - 1, y, 1, h, border);
  }
  text(s, x, y, v) {
    drawText(s, x, y, (px, py) => this.set(px, py, v));
  }
  /** Brighten (v > 0) a pixel smoothly. */
  lift(x, y, v) {
    this.set(x, y, this.get(x, y) + v);
  }
  async save(file, scale = SCALE) {
    await sharp(Buffer.from(this.px), { raw: { width: this.w, height: this.h, channels: 1 } })
      .resize(this.w * scale, this.h * scale, { kernel: "nearest" })
      .png({ compressionLevel: 9, palette: true, colours: 64 })
      .toFile(file);
    console.log("wrote", file);
  }
}

const lerp = (a, b, t) => a + (b - a) * t;

/** Night sky: a banded, dithered gradient with stars and streaky clouds. */
function sky(c, horizon, { seed, top = 2, bottom = 30, stars = 140, clouds = [] }) {
  const r = rng(seed);
  const bands = 28;
  for (let y = 0; y < horizon; y++) {
    const t = Math.pow(y / horizon, 1.8) * bands;
    const band = Math.floor(t);
    const a = lerp(top, bottom, band / bands);
    const b = lerp(top, bottom, (band + 1) / bands);
    for (let x = 0; x < c.w; x++) c.dither(x, y, a, b, t - band);
  }
  for (let i = 0; i < stars; i++) {
    const x = Math.floor(r() * c.w);
    const y = Math.floor(Math.pow(r(), 1.3) * (horizon - 12));
    const v = 50 + r() * 90;
    c.set(x, y, v);
    if (v > 125 && r() > 0.5) {
      c.lift(x - 1, y, v * 0.2);
      c.lift(x + 1, y, v * 0.2);
      c.lift(x, y - 1, v * 0.2);
      c.lift(x, y + 1, v * 0.2);
    }
  }
  // Clouds: stacks of thin horizontal streaks, lighter than the sky behind.
  for (const cl of clouds) {
    for (let row = 0; row < cl.rows; row++) {
      const y = cl.y + row * 2 + (r() > 0.6 ? 1 : 0);
      let x = cl.x + Math.floor((r() - 0.5) * cl.w * 0.4);
      const end = cl.x + cl.w * (0.5 + r() * 0.5);
      while (x < end) {
        const len = 3 + Math.floor(r() * cl.w * 0.25);
        const lift = cl.lift * (0.5 + r() * 0.5);
        for (let i = 0; i < len; i++) c.lift(x + i, y, lift);
        x += len + 2 + Math.floor(r() * 10);
      }
    }
  }
}

function moon(c, cx, cy, r, glow = 26) {
  for (let y = cy - r - glow; y <= cy + r + glow; y++) {
    for (let x = cx - r - glow; x <= cx + r + glow; x++) {
      const d = Math.hypot(x - cx, y - cy);
      if (d <= r) {
        // Lit disc with a soft terminator on the lower left.
        const shade = ((x - cx) + (cy - y)) / (r * 2);
        let v = 196 + shade * 16;
        c.set(x, y, v);
      } else if (d <= r + glow) {
        const t = 1 - (d - r) / glow;
        c.lift(x, y, 34 * t * t * t);
      }
    }
  }
  // Craters.
  for (const [dx, dy, cr] of [[-5, -4, 3], [6, 3, 2], [-2, 7, 2], [4, -8, 1.5], [-9, 3, 1.5]]) {
    for (let y = -cr; y <= cr; y++)
      for (let x = -cr; x <= cr; x++)
        if (Math.hypot(x, y) <= cr && Math.hypot(cx + dx + x - cx, cy + dy + y - cy) < r - 1)
          c.set(cx + dx + x, cy + dy + y, 168);
  }
}

/** A ridge line from midpoint displacement, `width` samples long. */
function ridge(width, seed, rough) {
  const r = rng(seed);
  let n = 1;
  while (n < width) n *= 2;
  const h = new Float32Array(n + 1);
  h[0] = r();
  h[n] = r();
  for (let step = n; step > 1; step /= 2) {
    const amp = rough * (step / n);
    for (let i = step / 2; i < n; i += step) h[i] = (h[i - step / 2] + h[i + step / 2]) / 2 + (r() - 0.5) * amp;
  }
  return h;
}

/** Fill below a ridge: heights in pixels above `base`, shaped by `mask(x)`. */
function mountains(c, base, { seed, height, rough = 2.2, value, edge = null, mask = () => 1 }) {
  const h = ridge(c.w, seed, rough);
  let lo = Infinity, hi = -Infinity;
  for (let x = 0; x < c.w; x++) { lo = Math.min(lo, h[x]); hi = Math.max(hi, h[x]); }
  for (let x = 0; x < c.w; x++) {
    const top = Math.round(base - ((h[x] - lo) / (hi - lo || 1)) * height * mask(x));
    for (let y = top; y < base; y++) c.set(x, y, value);
    if (edge != null) c.set(x, top, edge);
  }
}

function pine(c, x, base, height, value) {
  const w = Math.max(3, Math.round(height * 0.42));
  for (let y = 0; y < height; y++) {
    const t = y / height; // 0 at the tip
    // Three stacked tiers give the silhouette its notches.
    const tier = (t * 3) % 1;
    const half = Math.round((w / 2) * (0.25 + t * 0.75) * (0.55 + tier * 0.45));
    for (let i = -half; i <= half; i++) c.set(x + i, base - height + y, value);
  }
  c.rect(x, base - 1, 1, 2, value);
}

/** Water below `horizon`: a rippled reflection of the scene, darkened. */
function water(c, horizon, { seed, darken = 0.5, base = 14, glintX = null, glint = 0 }) {
  const r = rng(seed);
  for (let y = horizon; y < c.h; y++) {
    const depth = (y - horizon) / (c.h - horizon);
    const srcY = horizon - 1 - (y - horizon) * 1.15;
    const wobble = Math.round(Math.sin(y * 0.9) * (1 + depth * 2));
    for (let x = 0; x < c.w; x++) {
      const src = srcY >= 0 ? c.get(x + wobble, Math.floor(srcY)) : base;
      const v = lerp(base, src, darken * (1 - depth * 0.6));
      c.set(x, y, v);
    }
  }
  // Ripple streaks.
  for (let i = 0; i < c.w * 0.3; i++) {
    const y = horizon + 2 + Math.floor(r() * (c.h - horizon - 2));
    const x = Math.floor(r() * c.w);
    const len = 2 + Math.floor(r() * 10);
    for (let k = 0; k < len; k++) c.lift(x + k, y, 10);
  }
  // A broken column of light under the moon.
  if (glintX != null) {
    for (let y = horizon + 1; y < c.h; y += 1 + (r() > 0.5 ? 1 : 0)) {
      const depth = (y - horizon) / (c.h - horizon);
      const half = Math.round((2 + depth * 9) * (0.4 + r() * 0.8));
      const off = Math.round((r() - 0.5) * 4 * (1 + depth * 2));
      const v = lerp(glint, glint * 0.5, depth);
      for (let k = -half; k <= half; k++) if (r() > 0.18) c.set(glintX + off + k, y, Math.max(c.get(glintX + off + k, y), v * (1 - Math.abs(k) / (half + 2) * 0.6)));
    }
  }
}

function cabin(c, x, base) {
  const W = 15, H = 9;
  c.rect(x, base - H, W, H, 0);
  for (let i = 0; i < 6; i++) {
    c.rect(x - 2 + i, base - H - 1 - i, W + 4 - i * 2, 1, 0);
    c.set(x - 2 + i, base - H - 1 - i, 46);
    c.set(x + W + 1 - i, base - H - 1 - i, 46);
  }
  c.rect(x + 11, base - H - 8, 2, 5, 0); // chimney
  // Lit window with a little glow.
  for (let y = -4; y <= 6; y++)
    for (let xx = -5; xx <= 7; xx++) {
      const d = Math.hypot(xx - 1, y - 1);
      if (d < 6) c.lift(x + 4 + xx, base - 7 + y, 22 * (1 - d / 6) ** 2);
    }
  c.rect(x + 3, base - 7, 4, 3, 190);
  c.rect(x + 5, base - 7, 1, 3, 0);
  c.rect(x + 10, base - 5, 3, 5, 0); // door
  // Smoke.
  const r = rng(7);
  for (let i = 0; i < 26; i++) {
    const t = i / 26;
    const sx = x + 12 + Math.round(Math.sin(t * 5) * 3 + t * 10);
    const sy = base - H - 9 - i;
    if (r() > t * 0.8) c.lift(sx, sy, 22 * (1 - t));
  }
}

function jetty(c, x, base, len) {
  c.rect(x, base, len, 1, 54);
  for (let i = 0; i < len; i += 5) c.rect(x + i, base + 1, 1, 3, 30);
  // A lantern at the end.
  c.rect(x + len - 2, base - 6, 1, 6, 54);
  c.rect(x + len - 3, base - 8, 3, 2, 180);
  for (let y = -6; y <= 6; y++)
    for (let xx = -6; xx <= 6; xx++) {
      const d = Math.hypot(xx, y);
      const px = x + len - 2 + xx, py = base - 7 + y;
      if (d < 6 && d > 1.5) c.lift(px, py, 26 * (1 - d / 6) ** 2);
    }
}

// --- Hero: a night lake with a cabin, moonlight and mountains -------------
async function hero() {
  const c = new Canvas(480, 250);
  const horizon = 182;
  sky(c, horizon, {
    seed: 11,
    clouds: [
      { x: 250, y: 46, w: 210, rows: 6, lift: 7 },
      { x: 290, y: 96, w: 190, rows: 5, lift: 6 },
      { x: 10, y: 120, w: 160, rows: 4, lift: 5 },
      { x: 320, y: 140, w: 160, rows: 4, lift: 5 },
    ],
  });
  moon(c, 372, 72, 18, 40);
  const sides = (x) => {
    const t = x / c.w;
    // High at both edges, low in the middle so the moon shows over the water.
    return Math.min(1, 0.18 + Math.pow(Math.abs(t - 0.58) * 2.1, 1.6));
  };
  mountains(c, horizon, { seed: 3, height: 70, value: 20, edge: 38, mask: sides });
  mountains(c, horizon, { seed: 9, height: 38, rough: 3, value: 9, edge: 22, mask: sides });
  // Tree lines on both shores.
  const r = rng(21);
  for (let x = 0; x < 120; x += 3 + Math.floor(r() * 3)) pine(c, x, horizon, 10 + Math.floor(r() * 18) * (1 - x / 160), 4);
  for (let x = 400; x < 480; x += 3 + Math.floor(r() * 3)) pine(c, x, horizon, 8 + Math.floor(r() * 22) * ((x - 380) / 100), 4);
  c.rect(0, horizon - 2, 130, 2, 4);
  c.rect(392, horizon - 2, 88, 2, 4);
  cabin(c, 132, horizon);
  water(c, horizon, { seed: 5, darken: 0.5, base: 2, glintX: 372, glint: 120 });
  c.rect(0, horizon, c.w, 1, 34);
  jetty(c, 140, horizon + 3, 34);
  await c.save("public/art/hero.png");
}

// --- Feature illustrations -----------------------------------------------
// 280x156 drawn, saved at 3x (840x468) so the pixel text stays small.
function scene(seed, { moonX = 230, moonY = 34 } = {}) {
  const c = new Canvas(280, 156);
  const horizon = 116;
  sky(c, horizon, {
    seed,
    stars: 90,
    clouds: [
      { x: 10, y: 26, w: 130, rows: 4, lift: 7 },
      { x: 150, y: 74, w: 130, rows: 4, lift: 6 },
    ],
  });
  moon(c, moonX, moonY, 10, 24);
  mountains(c, horizon, { seed: seed + 1, height: 28, value: 20, edge: 38 });
  mountains(c, horizon, { seed: seed + 7, height: 14, rough: 3, value: 9, edge: 22 });
  water(c, horizon, { seed: seed + 2, darken: 0.5, base: 2, glintX: moonX, glint: 110 });
  c.rect(0, horizon, c.w, 1, 34);
  return c;
}

/** A pixel terminal window with a title bar. */
function window_(c, x, y, w, h, title) {
  c.rect(x + 3, y + 3, w, h, 0); // soft drop shadow
  c.box(x, y, w, h, 70, 10);
  c.rect(x + 1, y + 11, w - 2, 1, 44);
  for (let i = 0; i < 3; i++) c.rect(x + 4 + i * 5, y + 4, 3, 3, 70);
  if (title) c.text(title, x + w - 4 - textWidth(title), y + 2, 110);
}

/** A label tag: inverted text on a light block, or light text in a frame. */
function tag(c, s, x, y, inverted = true) {
  const w = textWidth(s) + 6;
  c.rect(x, y, w, 11, inverted ? 200 : 6);
  if (!inverted) c.box(x, y, w, 11, 80, null);
  c.text(s, x + 3, y + 1, inverted ? 10 : 190);
  return w;
}

async function models() {
  const c = scene(41);
  const x = 34, y = 30, w = 122;
  window_(c, x, y, w, 72, "ctrl+l");
  const rows = ["claude-opus-5-5", "gpt-5.5", "grok-5", "qwen3 · ollama", "openrouter/..."];
  rows.forEach((name, i) => {
    const ry = y + 15 + i * 11;
    if (i === 0) {
      c.rect(x + 2, ry - 1, w - 4, 11, 200);
      c.text("›", x + 4, ry, 10);
      c.text(name, x + 11, ry, 10);
    } else c.text(name, x + 11, ry, 150);
  });
  tag(c, "any model", 176, 136, false);
  await c.save("public/art/models.png", 3);
}

async function sandbox() {
  const c = scene(52, { moonX: 40, moonY: 30 });
  // A walled yard (the workspace) with the terminal inside it.
  const x0 = 70, x1 = 250, top = 54, base = 126;
  for (let y = top; y < base; y++)
    for (let x = x0; x < x1; x++) {
      const wall = x < x0 + 8 || x >= x1 - 8 || y >= base - 7;
      if (!wall) continue;
      const row = (y - top) >> 2;
      const mortar = (y - top) % 4 === 3 || (x - x0 + (row % 2) * 3) % 6 === 5;
      c.set(x, y, mortar ? 8 : 52);
    }
  for (let x = x0; x < x1 - 2; x += 6) c.rect(x, top - 3, 4, 3, 52); // crenels
  window_(c, x0 + 22, top + 6, 136, 52, "~/code/ledger");
  c.text("$ cargo test", x0 + 28, top + 22, 190);
  c.text("38 passed", x0 + 28, top + 34, 150);
  c.rect(x0 + 28 + textWidth("38 passed") + 3, top + 35, 4, 7, 190);
  tag(c, "workspace-write", 8, 140);
  tag(c, "network: off", 186, 140, false);
  await c.save("public/art/sandbox.png", 3);
}

async function steer() {
  const c = scene(63, { moonX: 234, moonY: 30 });
  const x = 26, y = 22;
  window_(c, x, y, 160, 86, "nyq");
  [
    ["✓", "read_file src/parse.rs", 130],
    ["✓", "bash cargo test parse", 130],
    ["•", "editing src/parse.rs", 200],
  ].forEach(([mark, s, v], i) => {
    c.text(mark, x + 6, y + 16 + i * 11, v);
    c.text(s, x + 14, y + 16 + i * 11, v);
  });
  // The input stays open: the user steers while the agent works.
  c.box(x + 4, y + 66, 152, 14, 70, 14);
  const msg = "› also update the docs";
  c.text(msg, x + 8, y + 68, 190);
  c.rect(x + 8 + textWidth(msg) + 2, y + 68, 4, 8, 190);
  tag(c, "queued", 196, 112);
  await c.save("public/art/steer.png", 3);
}

await import("node:fs").then((fs) => fs.mkdirSync("public/art", { recursive: true }));
await hero();
await models();
await sandbox();
await steer();
