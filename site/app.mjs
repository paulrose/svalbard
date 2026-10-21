import { daylightAt } from "./daylight.mjs";
import * as run from "./timeline.mjs";

const root = document.documentElement;
const scene = document.querySelector(".scene");
const canvas = scene.querySelector("canvas");
const context = canvas.getContext("2d");
const reducedMotion = matchMedia("(prefers-reduced-motion: reduce)");
const anchors = Object.fromEntries(
  [...scene.querySelectorAll("[data-anchor]")].map((el) => [
    el.dataset.anchor,
    el,
  ]),
);
const texts = Object.fromEntries(
  [...scene.querySelectorAll("[data-text]")].map((el) => [el.dataset.text, el]),
);
const daylightNote = document.querySelector("[data-daylight]");
// Local previews only: ?at=<seconds> freezes the run, ?theme= overrides daylight.
const params = new URLSearchParams(location.search);
const frozenAt = params.has("at") ? Number(params.get("at")) : null;
const forcedTheme = params.get("theme");

const TAU = Math.PI * 2;
const FPS = 30;
const STILL = 15.2; // Reduced motion: snapshot closed, upload under way.
const RUNS = [
  {
    trigger: "cron",
    schedule: "0 2 * * *",
    database: "PostgreSQL",
    storage: "Amazon S3",
  },
  {
    trigger: "GitHub Actions",
    schedule: "schedule",
    database: "MySQL",
    storage: "Cloudflare R2",
  },
  {
    trigger: "cron",
    schedule: "0 2 * * *",
    database: "MySQL",
    storage: "SFTP",
  },
  {
    trigger: "GitHub Actions",
    schedule: "schedule",
    database: "PostgreSQL",
    storage: "SCP",
  },
];
// The massif's skyline in fractions of the scene, from foot to foot.
const MASSIF = {
  ridge: [
    [0.265, 1],
    [0.3, 0.635],
    [0.335, 0.6],
    [0.37, 0.555],
    [0.395, 0.51],
    [0.415, 0.455],
    [0.43, 0.445],
    [0.448, 0.43],
    [0.462, 0.37],
    [0.478, 0.3],
    [0.49, 0.235],
    [0.5, 0.165],
    [0.512, 0.2],
    [0.524, 0.235],
    [0.532, 0.228],
    [0.548, 0.29],
    [0.565, 0.355],
    [0.578, 0.395],
    [0.595, 0.37],
    [0.612, 0.335],
    [0.626, 0.318],
    [0.638, 0.296],
    [0.65, 0.272],
    [0.662, 0.3],
    [0.672, 0.312],
    [0.69, 0.345],
    [0.7, 0.38],
    [0.718, 0.392],
    [0.735, 0.43],
    [0.748, 0.47],
    [0.765, 0.49],
    [0.785, 0.53],
    [0.81, 0.585],
    [0.826, 0.625],
    [0.85, 1],
  ],
};

const clamp01 = (v) => (v < 0 ? 0 : v > 1 ? 1 : v);
const clamp = (v, low, high) => Math.min(high, Math.max(low, v));
const mix = (a, b, t) => a + (b - a) * t;
const smooth = (a, b, v) => {
  const t = clamp01((v - a) / (b - a));
  return t * t * (3 - 2 * t);
};
const easeOut = (t) => 1 - (1 - t) ** 3;
const easeInOut = (t) => (t < 0.5 ? 2 * t * t : 1 - 2 * (1 - t) * (1 - t));

function lattice(x, y, z) {
  let h =
    Math.imul(x, 0x27d4eb2d) ^
    Math.imul(y, 0x165667b1) ^
    Math.imul(z, 0x1b873593);
  h = Math.imul(h ^ (h >>> 15), 0x85ebca6b);
  h = Math.imul(h ^ (h >>> 13), 0xc2b2ae35);
  return ((h ^ (h >>> 16)) >>> 0) / 4294967296;
}

// Smooth value noise, roughly in [-1, 1].
function noise(x, y, z) {
  const xi = Math.floor(x);
  const yi = Math.floor(y);
  const zi = Math.floor(z);
  let u = x - xi;
  let v = y - yi;
  let w = z - zi;
  u = u * u * (3 - 2 * u);
  v = v * v * (3 - 2 * v);
  w = w * w * (3 - 2 * w);
  const a = mix(lattice(xi, yi, zi), lattice(xi + 1, yi, zi), u);
  const b = mix(lattice(xi, yi + 1, zi), lattice(xi + 1, yi + 1, zi), u);
  const c = mix(lattice(xi, yi, zi + 1), lattice(xi + 1, yi, zi + 1), u);
  const d = mix(
    lattice(xi, yi + 1, zi + 1),
    lattice(xi + 1, yi + 1, zi + 1),
    u,
  );
  return mix(mix(a, b, v), mix(c, d, v), w) * 2 - 1;
}

function random(seed) {
  return () => {
    seed = (seed + 0x6d2b79f5) | 0;
    let value = Math.imul(seed ^ (seed >>> 15), 1 | seed);
    value = (value + Math.imul(value ^ (value >>> 7), 61 | value)) ^ value;
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}
const gaussian = (rand) => (rand() + rand() + rand() + rand() - 2) / 0.58;

// Arc-length table for a cubic curve, so particles keep an even pace.
function curve(points, samples = 220) {
  const [[ax, ay], [bx, by], [cx, cy], [dx, dy]] = points;
  const raw = new Float64Array((samples + 1) * 3);
  let length = 0;
  for (let i = 0; i <= samples; i++) {
    const t = i / samples;
    const u = 1 - t;
    const x =
      u * u * u * ax + 3 * u * u * t * bx + 3 * u * t * t * cx + t ** 3 * dx;
    const y =
      u * u * u * ay + 3 * u * u * t * by + 3 * u * t * t * cy + t ** 3 * dy;
    if (i) length += Math.hypot(x - raw[i * 3 - 3], y - raw[i * 3 - 2]);
    raw.set([x, y, length], i * 3);
  }
  const table = new Float32Array((samples + 1) * 4);
  for (let i = 0, j = 0; i <= samples; i++) {
    const target = (i / samples) * length;
    while (j < samples - 1 && raw[j * 3 + 5] < target) j++;
    const [x0, y0, l0, x1, y1, l1] = raw.subarray(j * 3, j * 3 + 6);
    const f = l1 > l0 ? (target - l0) / (l1 - l0) : 0;
    const tx = x1 - x0;
    const ty = y1 - y0;
    const tl = Math.hypot(tx, ty) || 1;
    table.set([x0 + tx * f, y0 + ty * f, -ty / tl, tx / tl], i * 4);
  }
  return { table, samples, length };
}
const point = { x: 0, y: 0, nx: 0, ny: 0 };
function sample(path, s) {
  const f = clamp01(s) * path.samples;
  const i = Math.min(path.samples - 1, f | 0);
  const k = f - i;
  const a = i * 4;
  const table = path.table;
  point.x = table[a] + (table[a + 4] - table[a]) * k;
  point.y = table[a + 1] + (table[a + 5] - table[a + 1]) * k;
  point.nx = table[a + 2];
  point.ny = table[a + 3];
  return point;
}

const grid = { width: 0, height: 0, cell: 2, cols: 0, rows: 0 };
let base, threshold, twinkle, ink, accent, image, pixels;
let layout, strata, summit, summitWeight, colours;
let inbound, upload, sparks, snow, traffic;
let falloff = new Float32Array(0);
let clouds = null;
let theme = "";

function computeLayout(width, height) {
  const compact = width < 760;
  const ground = height * (compact ? 0.7 : 0.66);
  // Phones squeeze the massif sideways, so it is also made lower.
  const X = compact
    ? (f) => width * (0.265 + (f - 0.265) * 0.92)
    : (f) => width * f;
  const Y = (f) =>
    f >= 1
      ? ground
      : compact
        ? height * (0.28 + (f - 0.165) * 0.74)
        : height * f;

  // A ragged ridgeline: subdivide each segment with seeded vertical jitter.
  const rand = random(1906);
  let ridge = MASSIF.ridge.map(([x, y]) => ({ x: X(x), y: Y(y) }));
  for (let pass = 0; pass < 5; pass++) {
    const next = [ridge[0]];
    for (let k = 1; k < ridge.length; k++) {
      const a = ridge[k - 1];
      const b = ridge[k];
      const length = Math.hypot(b.x - a.x, b.y - a.y);
      const lift =
        a.y >= ground && b.y >= ground ? 0 : (rand() - 0.5) * length * 0.12;
      next.push(
        { x: (a.x + b.x) / 2, y: Math.min(ground, (a.y + b.y) / 2 + lift) },
        b,
      );
    }
    ridge = next;
  }
  const topAt = (x) => {
    if (x <= ridge[0].x || x >= ridge[ridge.length - 1].x) return Infinity;
    let k = 1;
    while (ridge[k].x < x) k++;
    const a = ridge[k - 1];
    const b = ridge[k];
    return mix(a.y, b.y, (x - a.x) / (b.x - a.x));
  };
  const flank = (from, to, y, inset) => {
    const step = Math.sign(to - from);
    for (let x = from; step * (to - x) > 0; x += step)
      if (topAt(x) <= y) return { x: x + step * inset, y };
    return { x: to, y };
  };
  const peak = { x: X(0.5), y: Y(0.165) };
  const second = { x: X(0.65), y: Y(0.272) };
  const col = X(0.578);
  const feet = [X(0.265), X(0.85)];

  const rx = compact
    ? clamp(width * 0.08, 24, 36)
    : clamp(width * 0.04, 34, 64);
  const database = { x: width * 0.15, rx, ry: rx * 0.3, height: rx * 1.5 };
  database.bottom = ground - database.ry * 0.45;
  database.top = database.bottom - database.height;
  const knot = {
    x: database.x + rx * 0.3,
    y:
      database.top -
      database.ry -
      (compact ? 46 : Math.max(60, height * 0.085)),
    r: rx * 0.85,
  };
  const inlet = flank(feet[0], peak.x, Y(compact ? 0.47 : 0.5), 10);
  const outlet = flank(feet[1], second.x, Y(compact ? 0.5 : 0.53), 10);
  const vaultSpan = compact ? [0.83, 0.97] : [0.872, 0.956];
  const vault = { x0: width * vaultSpan[0], x1: width * vaultSpan[1] };
  vault.cx = (vault.x0 + vault.x1) / 2;
  vault.stack = ground + Math.max(42, height * 0.06);
  const trigger = compact
    ? { x: width * 0.07, y: height * 0.15 }
    : { x: width * 0.3, y: height * 0.12 };
  const glowPoint = { x: peak.x, y: peak.y + 10 };
  return {
    compact,
    ground,
    topAt,
    peak,
    second,
    col,
    feet,
    database,
    knot,
    vault,
    trigger,
    turbulence: compact ? 20 : 36,
    spread: compact ? 9 : 16,
    inbound: compact
      ? curve([
          [knot.x, knot.y],
          [knot.x + width * 0.1, knot.y - height * 0.07],
          [inlet.x - width * 0.04, inlet.y - height * 0.07],
          [inlet.x, inlet.y],
        ])
      : curve([
          [knot.x, knot.y],
          [knot.x + width * 0.09, knot.y - height * 0.09],
          [inlet.x - width * 0.1, inlet.y - height * 0.07],
          [inlet.x, inlet.y],
        ]),
    upload: curve([
      [outlet.x, outlet.y],
      [mix(outlet.x, vault.cx, 0.45), outlet.y - height * 0.035],
      [vault.cx, outlet.y - height * 0.01],
      [vault.cx, ground],
    ]),
    spark: curve([
      [trigger.x, trigger.y],
      [mix(trigger.x, glowPoint.x, 0.35), trigger.y - height * 0.05],
      [mix(trigger.x, glowPoint.x, 0.85), glowPoint.y - height * 0.06],
      [glowPoint.x, glowPoint.y],
    ]),
  };
}

function createParticles() {
  const rand = random(78223);
  const { width, height, cell } = grid;
  const area = width * height;
  const inboundCount = Math.round(clamp(area / 400, 1200, 3000));
  inbound = Array.from({ length: inboundCount }, (_, i) => {
    // Sparse at the start and end of the read, dense in the middle.
    const u = (i + rand()) / inboundCount;
    const read =
      run.READ[0] +
      (run.READ[1] - run.READ[0]) * (u + (0.6 * Math.sin(TAU * u)) / TAU);
    return {
      read,
      gather: mix(run.GATHER[0], run.GATHER[1], rand()),
      carry: mix(run.CARRY[0], run.CARRY[1], rand()),
      fx: (rand() * 2 - 1) * 0.9,
      row: rand() < 0.5 ? 0 : 1 + Math.floor(rand() * 3),
      spin: rand() < 0.8 ? 1 : -1,
      orbit: 0.25 + rand() * 0.75,
      lane: gaussian(rand),
      drift: gaussian(rand),
      seed: rand() * 50,
      big: rand() < 0.12,
      weight: 0.75 + rand() * 0.5,
    };
  });

  // The upload pours out as one dense stream. It lands in the vault and
  // settles outward from where it falls, bottom row first.
  const vault = layout.vault;
  const slotCols = Math.max(6, Math.floor((vault.x1 - vault.x0) / cell) - 3);
  const slots = [];
  for (let row = 0; row < 3; row++)
    for (let col = 0; col < slotCols; col++)
      slots.push({
        col,
        row,
        key:
          Math.abs(col - (slotCols - 1) / 2) / slotCols +
          (2 - row) * 0.35 +
          rand() * 0.3,
      });
  slots.sort((a, b) => a.key - b.key);
  const total = Math.round(clamp(area / 1500, 300, 800));
  upload = Array.from({ length: total }, (_, j) => ({
    launch:
      run.UPLOAD[0] +
      ((run.UPLOAD[1] - run.UPLOAD[0]) * (j + rand() * 0.8)) / total,
    lane: gaussian(rand),
    seed: rand() * 40,
    ...slots[Math.floor((j * slots.length) / total)],
  }));
  layout.slotCols = slotCols;

  sparks = Array.from({ length: 36 }, (_, k) => ({
    delay: k * 0.012,
    offset: gaussian(rand) * 3,
  }));
  snow = Array.from({ length: Math.round(area / 6500) }, () => ({
    x: rand(),
    y: rand(),
    vx: 8 + rand() * 18,
    vy: 5 + rand() * 9,
    wobble: rand() * TAU,
    freq: 0.4 + rand() * 0.8,
    weight: 0.3 + rand() * 0.5,
  }));
  traffic = Array.from({ length: 6 }, (_, k) => ({
    row: k % 4,
    speed: 0.1 + rand() * 0.22,
    dir: rand() < 0.5 ? -1 : 1,
    phase: rand(),
    length: 2 + Math.floor(rand() * 3),
  }));
}

function readPalette() {
  const style = getComputedStyle(root);
  const pack = (name) => {
    const hex = style.getPropertyValue(`--${name}`).trim().slice(1);
    const value = parseInt(hex, 16);
    return (
      (255 << 24) |
      ((value & 255) << 16) |
      (((value >> 8) & 255) << 8) |
      (value >> 16)
    );
  };
  // Dither only between neighbouring tones, from paper up to ink.
  const tones = [0, pack("rule"), pack("faint"), pack("muted"), pack("ink")];
  colours = {
    table: Int32Array.from([
      ...tones,
      tones[4],
      ...Array(6).fill(pack("accent")),
    ]),
  };
}

// The landscape is painted once as fields of dot density, then dithered every frame.
function paintLandscape() {
  const { cols, rows, cell, width, height } = grid;
  const { ground, database: db, vault, compact, topAt } = layout;
  const dark = theme === "dark";
  // Dot density for a surface brightness: ink is dark by day, light by night.
  const fromLight = (lum) =>
    dark ? 0.02 + lum ** 1.5 * 0.78 : 0.97 * (1 - lum);
  const surface = document.createElement("canvas");
  surface.width = cols;
  surface.height = rows;
  const pen = surface.getContext("2d", { willReadFrequently: true });
  pen.scale(1 / cell, 1 / cell);
  // Red holds dot density; blue marks open sky, where stars may appear.
  const code = (density, sky = false) =>
    `rgb(${Math.round(255 * clamp01(density))},0,${sky ? 255 : 0})`;
  const vertical = (y0, y1, stops) => {
    const gradient = pen.createLinearGradient(0, y0, 0, y1);
    for (const [at, colour] of stops) gradient.addColorStop(at, colour);
    return gradient;
  };

  pen.fillStyle = vertical(0, ground, [
    [0, code(fromLight(dark ? 0 : 1), true)],
    [0.6, code(fromLight(dark ? 0.01 : 1), true)],
    [1, code(fromLight(dark ? 0.07 : 0.985), true)],
  ]);
  pen.fillRect(0, 0, width, ground);

  // Two distant ranges: quiet fills under a crisp skyline.
  for (const [lift, scale, seed, fill, edge] of [
    [0.05, 0.06, 1.3, 0.05, 0.5],
    [0.012, 0.045, 4.1, 0.09, 0.72],
  ]) {
    pen.beginPath();
    pen.moveTo(0, ground);
    for (let x = 0; x <= width + 6; x += 4) {
      const n =
        noise(x * 0.0035, seed, 0) * 0.6 +
        Math.abs(noise(x * 0.012, seed, 1)) * 0.3 +
        noise(x * 0.04, seed, 2) * 0.08;
      pen.lineTo(x, ground - height * (lift + scale * (0.5 + n)));
    }
    pen.lineTo(width + 6, ground);
    pen.closePath();
    pen.fillStyle = code(fill);
    pen.fill();
    pen.strokeStyle = code(edge);
    pen.lineWidth = cell * 0.8;
    pen.stroke();
  }

  pen.fillStyle = code(0);
  pen.fillRect(0, ground, width, height - ground);
  pen.fillStyle = code(0.2);
  pen.beginPath();
  pen.ellipse(
    db.x + db.rx * 0.95,
    ground + 2,
    db.rx * 1.3,
    db.ry * 0.5,
    0,
    0,
    TAU,
  );
  pen.fill();
  pen.strokeStyle = code(0.9);
  pen.lineWidth = 1.4;
  pen.beginPath();
  pen.moveTo(0, ground + 0.5);
  pen.lineTo(vault.x0, ground + 0.5);
  pen.moveTo(vault.x1, ground + 0.5);
  pen.lineTo(width, ground + 0.5);
  pen.stroke();

  // The database: an outlined cylinder of stacked rows.
  const body = pen.createLinearGradient(db.x - db.rx, 0, db.x + db.rx, 0);
  for (const [at, lum] of [
    [0, 0.95],
    [0.35, 0.88],
    [0.75, 0.62],
    [1, 0.42],
  ])
    body.addColorStop(at, code(fromLight(lum)));
  pen.fillStyle = body;
  pen.beginPath();
  pen.moveTo(db.x - db.rx, db.top);
  pen.lineTo(db.x - db.rx, db.bottom);
  pen.ellipse(db.x, db.bottom, db.rx, db.ry, 0, Math.PI, 0, true);
  pen.lineTo(db.x + db.rx, db.top);
  pen.fill();
  pen.fillStyle = code(fromLight(0.97));
  pen.beginPath();
  pen.ellipse(db.x, db.top, db.rx, db.ry, 0, 0, TAU);
  pen.fill();
  pen.strokeStyle = code(1);
  pen.lineWidth = cell;
  pen.beginPath();
  pen.ellipse(db.x, db.top, db.rx, db.ry, 0, 0, TAU);
  for (let k = 1; k <= 4; k++) {
    const y = db.top + (db.height * k) / 4;
    pen.moveTo(db.x + db.rx, y);
    pen.ellipse(db.x, y, db.rx, db.ry, 0, 0, Math.PI);
  }
  pen.moveTo(db.x - db.rx, db.top);
  pen.lineTo(db.x - db.rx, db.bottom);
  pen.moveTo(db.x + db.rx, db.top);
  pen.lineTo(db.x + db.rx, db.bottom);
  pen.stroke();

  // The vault: a shaft cut into the frozen ground, fading with depth.
  pen.fillStyle = vertical(ground, height, [
    [0, code(0.07)],
    [0.85, code(0)],
  ]);
  pen.fillRect(vault.x0, ground - 1, vault.x1 - vault.x0, height);
  pen.strokeStyle = vertical(ground, ground + (height - ground) * 0.85, [
    [0, code(0.85)],
    [1, code(0)],
  ]);
  pen.lineWidth = cell;
  pen.beginPath();
  pen.moveTo(vault.x0, ground);
  pen.lineTo(vault.x0, height);
  pen.moveTo(vault.x1, ground);
  pen.lineTo(vault.x1, height);
  pen.stroke();

  const data = pen.getImageData(0, 0, cols, rows).data;
  const tops = new Float32Array(cols);
  for (let x = 0; x < cols; x++) tops[x] = topAt((x + 0.5) * cell);
  const ridgeSlope = (x, reach) =>
    (topAt(x + reach) - topAt(x - reach)) / (2 * reach);

  const strataCells = [];
  const summitCells = [];
  const spacing = compact ? 9 : 12;
  const rise = ground - layout.peak.y;
  for (let y = 0; y < rows; y++) {
    const py = (y + 0.5) * cell;
    for (let x = 0; x < cols; x++) {
      const i = y * cols + x;
      const px = (x + 0.5) * cell;
      let d = data[i * 4] / 255;
      const sky = data[i * 4 + 2] > 127;
      let rate = 0.04 + lattice(x, y, 2) * 0.1;
      const below = py - tops[x];
      if (below >= 0 && py < ground) {
        // Faces fan out from each summit. Follow this cell's fall line back
        // up to the skyline; the ridge's slope there decides the light. The
        // main peak stands in front, and the second peak takes what it leaves.
        const fan = (c, lean, spread) => {
          const f = clamp01((py - c.y) / (ground - c.y));
          return c.x + (px - c.x - lean * f) / (1 + f * spread);
        };
        let source = fan(layout.peak, width * 0.025, 1.1);
        if (source > layout.col)
          source = Math.max(layout.col, fan(layout.second, width * 0.015, 1.3));
        const f = clamp01((py - layout.peak.y) / rise);
        const slope =
          -(
            ridgeSlope(source, 22 + f * 30) * 0.75 +
            ridgeSlope(source, 4) * 0.25
          ) +
          noise(source * 0.06, py * 0.012, 11) * 0.25;
        const sunlit = slope > 0;
        let lum =
          0.62 +
          0.22 * Math.tanh(slope * 2.2) +
          noise(px * 0.02, py * 0.02, 13) * 0.06;
        // Snow caps the summits and fills the gullies.
        const cap =
          rise * (0.13 + 0.07 * noise(px * 0.02, 3, 3)) * (sunlit ? 1.3 : 0.75);
        const gully =
          below < rise * 0.5 && noise(source * 0.12, py * 0.006, 7) > 0.55;
        const snowy = below < cap || gully;
        if (snowy)
          lum = sunlit ? 0.985 : Math.max(lum, below < cap ? 0.72 : 0.56);
        // The lower slopes fade into valley haze.
        lum = mix(lum, 0.9, smooth(ground - rise * 0.2, ground, py) * 0.65);
        d = fromLight(lum);
        // Svalbard's mountains are layered rock; the bands hold the backup.
        const band = (py + 4 * noise(px * 0.008, 0.5, 3)) / spacing;
        const k = band - Math.floor(band);
        if (
          k < cell / spacing &&
          below > cell * 3 &&
          py < ground - spacing &&
          noise(px * 0.05, Math.floor(band) * 1.7, 4) > -0.1
        ) {
          d += 0.08;
          strataCells.push(i, py);
        }
        // A crisp skyline: the edge cells against the sky.
        const edge =
          below < cell * 1.1 ||
          (x > 0 && tops[x - 1] > py) ||
          (x < cols - 1 && tops[x + 1] > py);
        if (edge) d = Math.max(d, 0.88);
        const w1 =
          1 -
          (py - layout.peak.y) / (rise * 0.42) -
          Math.abs(px - layout.peak.x) / (width * 0.5);
        const w2 =
          (1 -
            (py - layout.second.y) / (rise * 0.3) -
            Math.abs(px - layout.second.x) / (width * 0.4)) *
          0.8;
        const w = Math.min(1, Math.max(w1, w2));
        if (w > 0)
          summitCells.push(i, w * (sunlit ? 1 : 0.45) * (snowy ? 1 : 0.6));
      } else if (sky && dark && lattice(x, y, 5) < 0.003) {
        d = 0.75 + lattice(x, y, 6) * 0.25;
        rate = 0.3 + lattice(x, y, 7) * 0.5;
      }
      base[i] = d;
      threshold[i] = lattice(x, y, 1);
      twinkle[i] = rate;
    }
  }
  // Daylight: soft cloud banks, periodic across the width so they can drift.
  clouds = null;
  if (!dark) {
    const skyRows = Math.ceil((ground * 0.6) / cell);
    clouds = new Float32Array(cols * skyRows);
    for (let y = 0; y < skyRows; y++) {
      const py = (y + 0.5) * cell;
      const band = Math.exp(-(((py / ground - 0.3) / 0.13) ** 2));
      if (band < 0.02) continue;
      for (let x = 0; x < cols; x++) {
        const a = (x / cols) * TAU;
        const u = Math.cos(a) * 0.95;
        const v = Math.sin(a) * 0.95;
        const n =
          noise(u, py * 0.012, v) * 0.7 +
          noise(u * 2.4 + 5, py * 0.03, v * 2.4) * 0.3;
        clouds[y * cols + x] = smooth(0.12, 0.6, n) * band * 0.13;
      }
    }
  }

  const pairs = [];
  for (let k = 0; k < strataCells.length; k += 2)
    pairs.push([strataCells[k], strataCells[k + 1]]);
  pairs.sort((a, b) => b[1] - a[1]);
  strata = Int32Array.from(pairs, (pair) => pair[0]);
  summit = new Int32Array(summitCells.length / 2);
  summitWeight = new Float32Array(summitCells.length / 2);
  for (let k = 0; k < summit.length; k++) {
    summit[k] = summitCells[k * 2];
    summitWeight[k] = summitCells[k * 2 + 1];
  }
}

function plot(buffer, x, y, weight) {
  if (x < 0 || y < 0) return;
  const cx = (x / grid.cell) | 0;
  const cy = (y / grid.cell) | 0;
  if (cx >= grid.cols || cy >= grid.rows) return;
  buffer[cy * grid.cols + cx] += weight;
}

function line(buffer, x0, y0, x1, y1, weight) {
  const steps = Math.max(
    1,
    Math.ceil(Math.hypot(x1 - x0, y1 - y0) / grid.cell),
  );
  for (let k = 0; k <= steps; k++)
    plot(buffer, mix(x0, x1, k / steps), mix(y0, y1, k / steps), weight);
}

function drawWeather(clock) {
  const { cols, cell, width } = grid;
  const ground = layout.ground;
  const dark = theme === "dark";
  for (const flake of snow) {
    const x =
      ((flake.x * (width + 40) +
        clock * flake.vx +
        Math.sin(clock * flake.freq + flake.wobble) * 7) %
        (width + 40)) -
      20;
    const y = (flake.y * ground + clock * flake.vy) % ground;
    plot(ink, x, y, dark ? flake.weight + 0.2 : flake.weight * 0.6);
  }
  if (clouds) {
    // Drift with the wind, blending between cells for a smooth crawl.
    const travel = (clock * 5) / cell;
    const shift = Math.floor(travel) % cols;
    const blend = travel - Math.floor(travel);
    for (let row = 0; row < clouds.length; row += cols)
      for (let x = 0; x < cols; x++) {
        // Only open sky: the clouds pass behind the peaks and ranges.
        if (base[row + x] > 0.01) continue;
        let a = x - shift;
        if (a < 0) a += cols;
        const b = a === 0 ? cols - 1 : a - 1;
        ink[row + x] += clouds[row + a] * (1 - blend) + clouds[row + b] * blend;
      }
  }
  if (!dark) return;
  // Aurora curtains, brightest along their lower edge.
  const span = Math.ceil((ground * 0.26) / cell);
  if (falloff.length !== span)
    falloff = Float32Array.from({ length: span }, (_, k) =>
      k * cell < 6 ? (k * cell) / 6 : Math.exp((-2.6 * k) / span),
    );
  for (let x = 0; x < cols; x++) {
    const u = x * cell;
    const strength =
      clamp01(noise(u * 0.0028 + clock * 0.018, 3.3, clock * 0.05) + 0.05) *
      (0.55 + 0.45 * noise(u * 0.05, 9.1, clock * 0.12)) *
      0.4;
    if (strength <= 0) continue;
    const edge = Math.floor(
      (ground * 0.44 + 26 * noise(u * 0.004, clock * 0.04, 1.7)) / cell,
    );
    for (let k = 0; k < span && edge - k >= 0; k++)
      ink[(edge - k) * cols + x] += strength * falloff[k];
  }
}

function drawDatabase(clock) {
  // Ordinary reads and writes carry on throughout the backup.
  const db = layout.database;
  for (const streak of traffic) {
    let phase = (streak.phase + clock * streak.speed * streak.dir) % 1;
    if (phase < 0) phase += 1;
    for (let k = 0; k < streak.length; k++) {
      const x =
        db.x - db.rx * 0.86 + phase * db.rx * 1.72 - streak.dir * k * grid.cell;
      const across = clamp((x - db.x) / db.rx, -1, 1);
      const y =
        db.top +
        (db.height * (streak.row + 0.5)) / 4 +
        db.ry * Math.sqrt(1 - across * across);
      plot(ink, x, y, 1);
    }
  }
}

function drawTrigger(t) {
  const origin = layout.trigger;
  const ring = (t - run.FIRE) / 0.9;
  if (ring > 0 && ring < 1) {
    const r = 4 + 30 * easeOut(ring);
    const steps = Math.ceil((TAU * r) / grid.cell);
    for (let k = 0; k < steps; k++) {
      const a = (k / steps) * TAU;
      plot(
        accent,
        origin.x + Math.cos(a) * r,
        origin.y + Math.sin(a) * r,
        1.2 * (1 - ring),
      );
    }
  }
  for (const spark of sparks) {
    const q = (t - run.FIRE - spark.delay) / run.SPARK;
    if (q <= 0 || q >= 1.15) continue;
    for (let k = 0; k < 5; k++) {
      const at = q - k * 0.035;
      if (at <= 0 || at >= 1) continue;
      const s = easeInOut(at);
      const p = sample(layout.spark, s);
      const off = spark.offset * Math.sin(Math.PI * s);
      plot(accent, p.x + p.nx * off, p.y + p.ny * off, 1.2 - k * 0.22);
    }
  }
}

function drawSnapshot(t) {
  const open = smooth(run.OPEN, run.OPEN + 0.55, t);
  const close = smooth(run.CLOSE, run.CLOSE + 0.45, t);
  const weight = open * (1 - close) * 1.6;
  if (weight < 0.02) return;
  const db = layout.database;
  const pad = mix(28, 11, easeOut(open)) - 8 * close;
  const left = db.x - db.rx - pad;
  const right = db.x + db.rx + pad;
  const top = db.top - db.ry - pad;
  const bottom = db.bottom + db.ry + pad * 0.5;
  const arm = mix(6, 14, open);
  for (const [x, y, sx, sy] of [
    [left, top, 1, 1],
    [right, top, -1, 1],
    [left, bottom, 1, -1],
    [right, bottom, -1, -1],
  ]) {
    line(ink, x, y, x + sx * arm, y, weight);
    line(ink, x, y, x, y + sy * arm, weight);
  }
}

function drawInbound(t, clock) {
  const { knot: K, database: db, turbulence, spread } = layout;
  const cell = grid.cell;
  let arrived = 0;
  for (const p of inbound) {
    const age = t - p.read;
    if (age < 0) continue;
    if (age >= p.gather + p.carry) {
      arrived++;
      continue;
    }
    // Material lifts off the rows and spirals into a knot above the database.
    const sx = db.x + p.fx * db.rx;
    const sy =
      db.top +
      (db.height * p.row) / 4 +
      db.ry * Math.sqrt(1 - p.fx * p.fx) * (p.row ? 1 : p.lane * 0.4);
    const a0 = Math.atan2(sy - K.y, sx - K.x);
    const r0 = Math.hypot(sx - K.x, sy - K.y);
    let x;
    let y;
    let weight = p.weight;
    let buffer = ink;
    if (age < p.gather) {
      // Leave the surface quickly, then join the swirl.
      const q = age / p.gather;
      const e = easeOut(Math.min(1, q * 1.3));
      const a = a0 + p.spin * q * 3.6;
      x = mix(sx, K.x + Math.cos(a) * K.r * p.orbit, e);
      y = mix(sy, K.y + Math.sin(a) * K.r * p.orbit * 0.8, e);
      weight *= smooth(0, 0.15, q);
    } else {
      // Then the wind draws it out across the valley and into the mountain.
      const s = (age - p.gather) / p.carry;
      const q = sample(layout.inbound, easeInOut(s) * 0.85 + s * 0.15);
      const env = Math.sin(Math.PI * s) ** 0.8;
      const swirl =
        noise(q.x * 0.006, q.y * 0.006, clock * 0.32) * turbulence +
        Math.sin(clock * 1.7 + p.seed + s * 9) * turbulence * 0.3;
      const lateral = env * (swirl + p.lane * spread);
      const along = env * p.drift * spread;
      const fade = 1 - smooth(0, 0.35, s);
      const a = a0 + p.spin * (3.6 + s * 4);
      const r = K.r * p.orbit * fade;
      x = q.x + q.nx * lateral + q.ny * along + Math.cos(a) * r;
      y = q.y + q.ny * lateral - q.nx * along + Math.sin(a) * r * 0.8;
      // At the mountain it changes character before it disappears inside.
      if (s > 0.86) {
        buffer = accent;
        weight *= 1 - (s - 0.86) / 0.14;
      }
    }
    plot(buffer, x, y, weight);
    if (p.big) plot(buffer, x + cell, y, weight * 0.8);
  }
  return arrived;
}

function drawUpload(t) {
  const { vault, ground } = layout;
  const cell = grid.cell;
  const pitch = cell * 4;
  const sink = easeInOut(smooth(run.SINK[0], run.SINK[1], t)) * pitch;
  const layer = vault.stack - pitch + sink;
  const cool = smooth(run.COOL[0], run.COOL[1], t);
  const landing = 0.55;
  let launched = 0;
  for (const p of upload) {
    const age = t - p.launch;
    if (age < 0) continue;
    launched++;
    const slotX = vault.x0 + (p.col + 2) * cell;
    const slotY = layer + p.row * cell;
    const lane = vault.cx + p.lane * cell * 1.5;
    if (age < run.FLIGHT) {
      // Released slowly, quickening as it falls, loosening a little.
      const u = age / run.FLIGHT;
      const s = u ** 1.6;
      const q = sample(layout.upload, s);
      const spread =
        (0.6 + s * 3.5) * p.lane + noise(p.seed, s * 3, t * 0.8) * s * 3;
      plot(
        accent,
        q.x + q.nx * spread,
        q.y + q.ny * spread,
        1.2 * smooth(0, 0.06, u),
      );
    } else if (age < run.FLIGHT + run.FALL) {
      const f = (age - run.FLIGHT) / run.FALL;
      if (f < landing) {
        // Down the shaft.
        const g = f / landing;
        plot(
          accent,
          mix(lane, slotX, g * g * 0.3),
          mix(ground, slotY - cell, g * (0.6 + 0.4 * g)),
          1.3,
        );
      } else {
        // Land, hop once, and slide into place.
        const g = (f - landing) / (1 - landing);
        plot(
          accent,
          mix(mix(lane, slotX, 0.3), slotX, easeOut(g)),
          slotY - Math.sin(Math.PI * g) * (1 - g) * cell * 1.6,
          1.3,
        );
      }
    } else {
      plot(accent, slotX, slotY, 1.3 * (1 - cool));
      plot(ink, slotX, slotY, 1.2 * cool);
    }
  }
  // Earlier backups lie beneath, fading into the ground.
  const x0 = vault.x0 + 2 * cell;
  const x1 = x0 + layout.slotCols * cell;
  for (let j = 0; j < 6; j++) {
    const top = vault.stack + j * pitch + sink;
    const density = 1.05 - (j + sink / pitch) * 0.19;
    for (let row = 0; row < 3; row++)
      for (let x = x0; x < x1; x += cell)
        plot(ink, x, top + row * cell, density);
  }
  return launched;
}

function drawMountain(t, arrived, launched) {
  // The rock bands fill as the backup is written, and drain as it uploads.
  const level =
    t < run.UPLOAD[0] ? arrived / inbound.length : 1 - launched / upload.length;
  const filled = Math.floor(level * strata.length);
  const weight = theme === "dark" ? 0.2 : 0.3;
  for (let k = 0; k < filled; k++) ink[strata[k]] += weight;
  if (t < run.UPLOAD[0] && level < 1) {
    const front = Math.ceil(strata.length * 0.025);
    for (let k = Math.max(0, filled - front); k < filled; k++)
      accent[strata[k]] += 0.5;
  }
  // Alpenglow marks svalbard at work.
  const glow =
    smooth(run.FIRE + run.SPARK * 0.85, run.FIRE + run.SPARK + 1.4, t) *
    (1 - smooth(run.COOL[0], run.COOL[1] + 0.6, t));
  if (glow > 0)
    for (let k = 0; k < summit.length; k++)
      accent[summit[k]] += glow * summitWeight[k] * 0.4;
}

// Random thresholds make every comparison a coin flip, so the choice of tone
// is branchless: fixed-point differences give their sign bit, and the bits
// index one table of paper, rule, faint, muted, ink, then accent.
function composite(clock) {
  const table = colours.table;
  const count = grid.cols * grid.rows;
  const dots = base;
  const extra = ink;
  const glow = accent;
  const start = threshold;
  const rate = twinkle;
  const out = pixels;
  for (let i = 0; i < count; i++) {
    const a = glow[i];
    const level = (dots[i] + extra[i]) * 4;
    if (level <= 0 && a <= 0) {
      out[i] = 0;
      continue;
    }
    let th = start[i] + clock * rate[i];
    th -= th | 0;
    const t16 = (th * 65536) | 0;
    const low = level <= 0 ? 0 : level >= 4 ? 4 : level | 0;
    const step = (t16 - (((level - low) * 65536) | 0)) >>> 31;
    const lit = (t16 - ((a * 65536) | 0)) >>> 31;
    out[i] = table[low + step + lit * 6];
  }
  context.putImageData(image, 0, 0);
}

const shown = {};
function setText(name, value) {
  if (shown[name] === value) return;
  shown[name] = value;
  texts[name].textContent = value;
}

function updateLabels(t, cycle) {
  const details = RUNS[cycle % RUNS.length];
  const phase = run.phaseAt(t);
  setText("trigger", details.trigger);
  setText("schedule", details.schedule);
  if (details.trigger === "cron") {
    const seconds = Math.floor(7200 - run.FIRE + t);
    const pad = (n) => String(n).padStart(2, "0");
    setText(
      "clock",
      `${pad(Math.floor(seconds / 3600))}:${pad(Math.floor(seconds / 60) % 60)}:${pad(seconds % 60)}`,
    );
  } else
    setText(
      "clock",
      t < run.FIRE ? "queued" : t < run.STORED ? "in progress" : "completed",
    );
  setText("database", details.database);
  setText("snapshot", phase.snapshot && `snapshot ${phase.snapshot}`);
  setText("svalbard", phase.svalbard);
  setText("storage", details.storage);
  setText("stored", phase.storage);
  anchors.trigger.classList.toggle("fired", phase.started && t < run.STORED);
  texts.snapshot.classList.toggle("active", phase.snapshot === "open");
  texts.svalbard.classList.toggle(
    "active",
    phase.svalbard === "running" || phase.svalbard === "uploading",
  );
  texts.stored.classList.toggle("active", phase.storage === "receiving");
}

function placeLabels() {
  const { trigger, database, ground, feet, vault, compact } = layout;
  const place = (el, x, y, shift) => {
    el.style.left = `${Math.round(x)}px`;
    el.style.top = `${Math.round(y)}px`;
    el.style.transform = `translateX(${shift}%)`;
  };
  place(anchors.trigger, trigger.x - 8.5, trigger.y - 11, 0);
  const below = ground + 10 + database.ry;
  place(anchors.database, database.x, below, -50);
  place(
    anchors.svalbard,
    mix(feet[0], feet[1], compact ? 0.4 : 0.47),
    below,
    -50,
  );
  place(anchors.storage, vault.x0 - 10, below, -100);
  scene.dataset.ready = "";
}

let elapsed = 0;
let last = 0;
let frame = 0;
let onScreen = true;

function draw() {
  if (!layout || !colours) return;
  const still = reducedMotion.matches || frozenAt !== null;
  const clock = still ? 40 : elapsed / 1000;
  const t = still ? (frozenAt ?? STILL) : clock % run.CYCLE;
  const cycle = still ? 0 : Math.floor(clock / run.CYCLE);
  ink.fill(0);
  accent.fill(0);
  drawWeather(clock);
  drawDatabase(clock);
  drawTrigger(t);
  drawSnapshot(t);
  const arrived = drawInbound(t, clock);
  const launched = drawUpload(t);
  drawMountain(t, arrived, launched);
  composite(clock);
  updateLabels(t, cycle);
}

function tick(now) {
  frame = requestAnimationFrame(tick);
  if (!last) last = now;
  const delta = now - last;
  if (delta < 1000 / FPS - 4) return;
  last = now;
  elapsed += Math.min(delta, 100);
  draw();
}

function updateLoop() {
  cancelAnimationFrame(frame);
  frame = 0;
  last = 0;
  if (
    onScreen &&
    !document.hidden &&
    !reducedMotion.matches &&
    frozenAt === null
  )
    frame = requestAnimationFrame(tick);
  else draw();
}

function resize() {
  const width = scene.clientWidth;
  const height = scene.clientHeight;
  if (width === grid.width && height === grid.height) return;
  const ratio = devicePixelRatio || 1;
  // Whole device pixels per cell keep the grain crisp; large screens get coarser cells.
  const device = Math.max(
    2,
    Math.round(Math.max(2, Math.sqrt((width * height) / 330000)) * ratio),
  );
  const cell = device / ratio;
  const cols = Math.ceil(width / cell);
  const rows = Math.ceil(height / cell);
  Object.assign(grid, { width, height, cell, cols, rows });
  canvas.width = cols;
  canvas.height = rows;
  canvas.style.width = `${cols * cell}px`;
  canvas.style.height = `${rows * cell}px`;
  const count = cols * rows;
  base = new Float32Array(count);
  threshold = new Float32Array(count);
  twinkle = new Float32Array(count);
  ink = new Float32Array(count);
  accent = new Float32Array(count);
  image = context.createImageData(cols, rows);
  pixels = new Int32Array(image.data.buffer);
  layout = computeLayout(width, height);
  createParticles();
  paintLandscape();
  placeLabels();
  draw();
}

function applyDaylight() {
  const now = new Date();
  const sun = daylightAt(now);
  const time = new Intl.DateTimeFormat("en-GB", {
    timeZone: "Arctic/Longyearbyen",
    hour: "2-digit",
    minute: "2-digit",
  }).format(now);
  daylightNote.textContent = `${time}, ${sun.phase.toLowerCase()}`;
  const next =
    forcedTheme === "light" || forcedTheme === "dark" ? forcedTheme : sun.theme;
  if (next === theme) return;
  theme = next;
  root.dataset.theme = theme;
  readPalette();
  if (layout) {
    paintLandscape();
    draw();
  }
}

applyDaylight();
resize();
let pending = 0;
new ResizeObserver(() => {
  cancelAnimationFrame(pending);
  pending = requestAnimationFrame(resize);
}).observe(scene);
new IntersectionObserver(([entry]) => {
  onScreen = entry.isIntersecting;
  updateLoop();
}).observe(scene);
reducedMotion.addEventListener("change", updateLoop);
document.addEventListener("visibilitychange", updateLoop);
setInterval(applyDaylight, 60000);
updateLoop();
