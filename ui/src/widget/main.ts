/**
 * Widget bar controller.
 *
 * Three concerns live here:
 *
 * 1. The **adaptive width**. The bar is a real window, so its width has to be
 *    echoed back to Rust. The lyric is measured with the canvas API rather than
 *    estimated from character counts, and the transition is tweened in
 *    JavaScript because `SetWindowPos` has no easing of its own.
 * 2. The **idle / hover split** for the audio component: lyrics when the
 *    pointer is away, transport controls when it is over the component.
 * 3. The **spectrum canvas**, driven straight from the FFT frames Rust pushes.
 */

import {
  api,
  on,
  measureText,
  clamp,
  withAlpha,
  type Config,
  type Lyrics,
  type MediaSnapshot,
  type SpectrumFrame,
} from "../shared/bridge";

// --- layout constants ------------------------------------------------------
// These are CSS pixels, matching the stylesheet. The window, on the other hand,
// is sized in physical pixels, so everything is converted on the way out —
// without that, a 125% display clips the spectrum off the right edge.
const COVER = 26;
const GAP = 8;
const LAUNCHER = 34;
const PAD = 6; // .bar horizontal padding
const INSET = 2; // .bar margin, keeps room for the drop shadow
const SPECTRUM_HEIGHT = 22;
/** Fixed band count and geometry; see the native renderer for the rationale. */
const SPECTRUM_BARS = 8;
const SPECTRUM_BAR_WIDTH = 2.5;
const SPECTRUM_BAR_GAP = 1.5;
/** Ignore width changes below this, so sub-pixel jitter does not resize. */
const WIDTH_EPSILON = 2;

const el = {
  bar: document.getElementById("bar") as HTMLDivElement,
  launcher: document.getElementById("launcher") as HTMLButtonElement,
  badge: document.getElementById("badge") as HTMLSpanElement,
  audio: document.getElementById("audio") as HTMLDivElement,
  artwork: document.getElementById("artwork") as HTMLImageElement,
  coverFallback: document.getElementById("cover-fallback") as HTMLDivElement,
  slot: document.querySelector(".slot") as HTMLDivElement,
  lyric: document.getElementById("lyric") as HTMLDivElement,
  controls: document.getElementById("controls") as HTMLDivElement,
  spectrum: document.getElementById("spectrum") as HTMLCanvasElement,
  iconPlay: document.getElementById("icon-play") as unknown as SVGSVGElement,
  iconPause: document.getElementById("icon-pause") as unknown as SVGSVGElement,
};

let config: Config | null = null;
let media: MediaSnapshot = emptyMedia();
let lyrics: Lyrics = { lines: [], source: "" };
let lyricIndex: number | null = null;
let latestSpectrum: SpectrumFrame | null = null;
let openTaskCount = 0;
let flyoutOpen = false;

/** Width currently applied to the window, so we can skip no-op resizes. */
let appliedWidth = -1;
let widthTween: number | null = null;

function emptyMedia(): MediaSnapshot {
  return {
    has_session: false,
    title: "",
    artist: "",
    album: "",
    source_app: "",
    status: "closed",
    position_ms: 0,
    duration_ms: 0,
    can_play: false,
    can_pause: false,
    can_skip_next: false,
    can_skip_previous: false,
    artwork: "",
  };
}

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

async function init() {
  config = await api.getConfig();
  applyTheme(config);
  syncSpectrumCanvas();

  const [snapshot, lines, index, openCount] = await Promise.all([
    api.getMedia(),
    api.getLyrics(),
    api.getLyricIndex(),
    api.todoOpenCount().catch(() => 0),
  ]);
  media = snapshot;
  lyrics = lines;
  lyricIndex = index;
  openTaskCount = openCount;
  wire();
  render();
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function render() {
  renderAudioVisibility();
  renderLyric();
  renderControls();
  renderBadge();
  scheduleWidth();
}

// ---------------------------------------------------------------------------
// Events and DOM wiring
// ---------------------------------------------------------------------------

function wire() {
  on("media-changed", (next) => {
    // Rust resolves lyrics per track and pushes them separately. Blanking the
    // old document here stops the bar from briefly sizing itself to the
    // previous song's longest line.
    if (next.title !== media.title || next.artist !== media.artist) {
      lyrics = { lines: [], source: "" };
      lyricIndex = null;
    }
    media = next;
    render();
  });
  on("lyrics-changed", (next) => {
    lyrics = next;
    render();
  });
  on("lyric-index", (payload) => {
    lyricIndex = payload.index;
    renderLyric();
    scheduleWidth();
  });
  on("spectrum", (frame) => {
    latestSpectrum = frame;
  });
  on("config-changed", (next) => {
    config = next;
    applyTheme(next);
    syncSpectrumCanvas();
    render();
  });
  on("todo-changed", async () => {
    openTaskCount = await api.todoOpenCount().catch(() => 0);
    renderBadge();
  });

  el.launcher.addEventListener("click", async (event) => {
    event.stopPropagation();
    flyoutOpen = !flyoutOpen;
    setLauncherExpanded(flyoutOpen);
    if (flyoutOpen) {
      await api.showFlyout(await api.flyoutTab());
    } else {
      await api.hideFlyout();
    }
  });

  // A click anywhere else on the bar dismisses an open flyout, matching the
  // "click outside closes it" contract.
  document.addEventListener("pointerdown", () => {
    if (flyoutOpen) {
      flyoutOpen = false;
      setLauncherExpanded(false);
      void api.hideFlyout();
    }
  });

  el.controls.addEventListener("click", async (event) => {
    const button = (event.target as HTMLElement).closest("button");
    const action = button?.dataset.action;
    if (!action) return;
    event.stopPropagation();
    await api.mediaControl(action as "play" | "pause" | "toggle" | "next" | "previous");
  });

  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && flyoutOpen) {
      flyoutOpen = false;
      setLauncherExpanded(false);
      void api.hideFlyout();
    }
  });

  el.audio.addEventListener("pointerenter", () => render());
  el.audio.addEventListener("pointerleave", () => render());

  window.addEventListener("resize", () => {
    syncSpectrumCanvas();
    render();
  });

  startSpectrumLoop();
}

function renderAudioVisibility() {
  const show = media.has_session && config?.media.enabled !== false;
  el.audio.hidden = !show;
  el.bar.dataset.empty = show ? "false" : "true";
}

function currentLine(): string {
  if (!media.has_session) return "";
  if (!config?.media.show_lyrics) return "";
  if (lyrics.lines.length === 0) return "";
  if (lyricIndex === null) return "";
  const line = lyrics.lines[lyricIndex];
  return line ? line.text : "";
}

function renderLyric() {
  const line = currentLine();
  if (line) {
    if (el.lyric.textContent !== line) el.lyric.textContent = line;
    el.lyric.dataset.fallback = "false";
    return;
  }
  // No lyric for this track (or before the first timestamp): fall back to
  // "title - artist" as the spec requires.
  const fallback = media.has_session ? displayName() : "";
  if (el.lyric.textContent !== fallback) el.lyric.textContent = fallback;
  el.lyric.dataset.fallback = "true";
}

function displayName(): string {
  if (media.title && media.artist) return `${media.title} - ${media.artist}`;
  return media.title || media.artist || "";
}

function renderControls() {
  const playing = media.status === "playing";
  setHidden(el.iconPlay, playing);
  setHidden(el.iconPause, !playing);

  button("previous").disabled = !media.can_skip_previous;
  button("next").disabled = !media.can_skip_next;
  const toggle = button("toggle");
  toggle.disabled = playing ? !media.can_pause : !media.can_play;
}

function button(action: string): HTMLButtonElement {
  return el.controls.querySelector<HTMLButtonElement>(`[data-action="${action}"]`)!;
}

function renderBadge() {
  const wanted = config?.todo.enabled && config?.todo.show_badge;
  if (!wanted || openTaskCount <= 0) {
    el.badge.hidden = true;
    return;
  }
  el.badge.hidden = false;
  el.badge.textContent = openTaskCount > 99 ? "99+" : String(openTaskCount);
}

/** `[hidden]` is a global attribute, so it works on SVG elements too. */
function setHidden(node: Element, hidden: boolean) {
  if (hidden) node.setAttribute("hidden", "");
  else node.removeAttribute("hidden");
}

function setLauncherExpanded(open: boolean) {
  el.launcher.setAttribute("aria-expanded", String(open));
}

function applyTheme(cfg: Config) {
  document.documentElement.dataset.theme = cfg.ui.theme;
  document.documentElement.style.setProperty("--accent", cfg.ui.accent);

  // The pill follows the *taskbar* tint, not the app accent: it sits on the
  // taskbar and should read as part of it.
  el.bar.style.setProperty("--pill-bg", withAlpha(cfg.widget.background, cfg.widget.opacity));
  el.bar.style.borderRadius = `${cfg.widget.corner_radius}px`;
  el.bar.style.margin = `${cfg.widget.corner_radius > 0 ? 2 : 0}px`;
  document.documentElement.style.setProperty("--widget-anim", `${cfg.widget.animation_ms}ms`);
  el.bar.dataset.light = isLightTaskbar(cfg) ? "true" : "false";
}

/** The taskbar tint is "light" when the chosen colour is bright. */
function isLightTaskbar(cfg: Config): boolean {
  const hex = cfg.widget.background.replace("#", "");
  const r = parseInt(hex.slice(0, 2), 16) || 0;
  const g = parseInt(hex.slice(2, 4), 16) || 0;
  const b = parseInt(hex.slice(4, 6), 16) || 0;
  return (0.299 * r + 0.587 * g + 0.114 * b) / 255 > 0.6;
}

// ---------------------------------------------------------------------------
// Adaptive width
// ---------------------------------------------------------------------------

/**
 * Geometry of the bar for the current content, in CSS pixels.
 *
 * Mirrors the contract in the spec:
 *   content = max(measureText(lyric), controls, audio_min_content)
 *   width   = clamp(cover + gap + content + gap + spectrum, min, max)
 *
 * The content width is returned rather than recomputed by the stylesheet,
 * because the slot has to be exactly that wide: left to size itself from the
 * lyric, a short line would leave the spectrum stranded in the middle of the
 * bar instead of against its right edge.
 */
/// Physical pixels per CSS pixel in this webview.
function scale(): number {
  return window.devicePixelRatio || 1;
}

/// Config geometry is documented in physical pixels; the DOM wants CSS ones.
function fromPhysical(value: number): number {
  return value / scale();
}

interface Metrics {
  content: number;
  audio: number;
  bar: number;
}

function computeMetrics(cfg: Config): Metrics {
  const font = getComputedStyle(el.lyric).font || "12px sans-serif";
  const line = currentLine() || displayName();
  const lyricWidth = clamp(
    Math.ceil(measureText(line, font)),
    fromPhysical(cfg.widget.lyric_min_width),
    fromPhysical(cfg.widget.lyric_max_width),
  );

  // The transport controls need a fixed minimum; below this they look cramped.
  const controlsWidth = 3 * 24 + 4;
  // The bar hugs the lyric, but never narrower than the transport controls so
  // switching to the hover state cannot clip them.
  const content = Math.max(lyricWidth, controlsWidth);

  const spectrum = spectrumWidth(cfg);
  const spectrumPart = spectrum > 0 ? GAP + spectrum : 0;

  // The audio component is cover + slot + spectrum, and `PAD` belongs to the
  // bar around it — folding it in here would just leave a band of dead space
  // between the spectrum and the right edge.
  const audio = clamp(
    COVER + GAP + content + spectrumPart,
    fromPhysical(cfg.widget.audio_min_width),
    fromPhysical(cfg.widget.audio_max_width),
  );

  // When the clamp bites, the slot takes up the slack so the row stays packed
  // against the right edge instead of leaving a hole before the spectrum.
  const contentWidth = Math.max(0, audio - COVER - GAP - spectrumPart);

  const empty = el.bar.dataset.empty === "true";
  const bar = empty
    ? LAUNCHER + 2 * INSET + 4
    : LAUNCHER + GAP + audio + 2 * PAD + 2 * INSET;

  return { content: contentWidth, audio, bar };
}

function spectrumWidth(cfg: Config): number {
  if (!cfg.media.show_spectrum) return 0;
  return Math.round(SPECTRUM_BARS * SPECTRUM_BAR_WIDTH + (SPECTRUM_BARS - 1) * SPECTRUM_BAR_GAP);
}

function scheduleWidth() {
  if (!config) return;
  const metrics = computeMetrics(config);
  // `appliedWidth` and the resize calls are physical pixels; the CSS layout
  // above is in CSS pixels.
  const target = Math.round(metrics.bar * scale());
  if (Math.abs(target - appliedWidth) < WIDTH_EPSILON) return;

  el.slot.style.width = `${Math.round(metrics.content)}px`;
  el.audio.style.width = `${Math.round(metrics.audio)}px`;

  // Resizing the window is instant, so the easing has to come from us: step the
  // width over `animation_ms` at roughly 30 fps. Only track changes and
  // hover-in/out land here, so this is a handful of `SetWindowPos` calls, not a
  // continuous stream.
  const from = appliedWidth < 0 ? target : appliedWidth;
  appliedWidth = target;
  tweenWidth(from, target, config.widget.animation_ms);
}

function tweenWidth(from: number, to: number, durationMs: number) {
  if (widthTween !== null) cancelAnimationFrame(widthTween);
  if (durationMs <= 0 || Math.abs(to - from) < WIDTH_EPSILON) {
    void api.resizeWidget(to);
    return;
  }

  const start = performance.now();
  const step = (now: number) => {
    const t = clamp((now - start) / durationMs, 0, 1);
    // Same easing as the CSS transition, so the content and the window move
    // together instead of fighting.
    const eased = 1 - Math.pow(1 - t, 3);
    const width = Math.round(from + (to - from) * eased);
    void api.resizeWidget(width);
    if (t < 1) {
      widthTween = requestAnimationFrame(step);
    } else {
      widthTween = null;
      void api.resizeWidget(to);
    }
  };
  widthTween = requestAnimationFrame(step);
}

// ---------------------------------------------------------------------------
// Spectrum
// ---------------------------------------------------------------------------

function syncSpectrumCanvas() {
  if (!config) return;
  const cssWidth = spectrumWidth(config);
  const cssHeight = SPECTRUM_HEIGHT;
  el.spectrum.hidden = !config.media.show_spectrum;
  if (el.spectrum.hidden) return;

  const dpr = window.devicePixelRatio || 1;
  el.spectrum.style.width = `${cssWidth}px`;
  el.spectrum.width = Math.max(1, Math.round(cssWidth * dpr));
  el.spectrum.height = Math.max(1, Math.round(cssHeight * dpr));
}

function startSpectrumLoop() {
  const ctx = el.spectrum.getContext("2d");
  if (!ctx) return;

  const draw = () => {
    requestAnimationFrame(draw);
    if (!config || !config.media.show_spectrum || el.spectrum.hidden) return;
    renderSpectrum(ctx);
  };
  requestAnimationFrame(draw);
}

function renderSpectrum(ctx: CanvasRenderingContext2D) {
  const { width, height } = el.spectrum;
  ctx.clearRect(0, 0, width, height);

  const bands = latestSpectrum?.bands ?? [];
  const count = SPECTRUM_BARS;
  const gap = Math.max(1, Math.round(SPECTRUM_BAR_GAP * scale()));
  const barWidth = Math.max(1, (width - gap * (count - 1)) / count);
  const radius = Math.min(barWidth / 2, 2 * (window.devicePixelRatio || 1));

  const light = el.bar.dataset.light === "true";
  const base = light ? "27, 31, 42" : "255, 255, 255";

  for (let i = 0; i < count; i += 1) {
    // When a band is missing (fewer FFT bands than bars) draw a resting stub so
    // the component does not look broken.
    const value = clamp(bands[i] ?? 0, 0, 1);
    const barHeight = Math.max(barWidth * 0.75, value * height);
    const x = i * (barWidth + gap);
    const y = height - barHeight;
    // Quiet bars stay faint; loud ones go to full opacity. Reads better than a
    // uniform colour when the music is quiet.
    const alpha = 0.28 + value * 0.72;
    ctx.fillStyle = `rgba(${base}, ${alpha.toFixed(3)})`;
    ctx.beginPath();
    const r = Math.min(radius, barWidth / 2, barHeight / 2);
    roundedRect(ctx, x, y, barWidth, barHeight, r);
    ctx.fill();
  }
}

function roundedRect(
  ctx: CanvasRenderingContext2D,
  x: number,
  y: number,
  w: number,
  h: number,
  r: number,
) {
  ctx.moveTo(x + r, y);
  ctx.arcTo(x + w, y, x + w, y + h, r);
  ctx.arcTo(x + w, y + h, x, y + h, r);
  ctx.arcTo(x, y + h, x, y, r);
  ctx.arcTo(x, y, x + w, y, r);
  ctx.closePath();
}

init().catch((error) => {
  console.error("widget bar failed to start", error);
});
