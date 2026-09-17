/**
 * Typed wrapper around the Tauri IPC surface.
 *
 * Every call into Rust goes through here, so the command names and payload
 * shapes live in exactly one place and the three windows cannot drift apart.
 */

import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

// ---------------------------------------------------------------------------
// Types mirrored from beautify-core / the app crate
// ---------------------------------------------------------------------------

export type TaskbarMode =
  | "normal"
  | "opaque"
  | "clear"
  | "blur"
  | "acrylic"
  | "mica";

export type WidgetAnchor =
  | "taskbar-left"
  | "taskbar-center"
  | "taskbar-right"
  | "bottom-left"
  | "bottom-center"
  | "bottom-right";

/**
 * Where lyrics come from. Every value except `off` sends the track name and
 * artist to a third party, which is why the choice is explicit.
 */
export type LyricProvider =
  | "off"
  | "netease"
  | "qq"
  | "kugou"
  | "lrclib"
  | "custom";
export type Theme = "dark" | "light" | "auto";
export type PlaybackStatus =
  | "closed"
  | "stopped"
  | "paused"
  | "playing"
  | "unknown";

export interface Config {
  version: number;
  general: {
    autostart: boolean;
    start_minimized: boolean;
    keep_alive_on_pause: boolean;
  };
  taskbar: {
    enabled: boolean;
    mode: TaskbarMode;
    color: string;
    opacity: number;
    apply_to_secondary: boolean;
    dynamic_mode: boolean;
    dynamic_mode_override: TaskbarMode;
    hide_on_fullscreen: boolean;
    restore_on_exit: boolean;
  };
  media: {
    enabled: boolean;
    show_lyrics: boolean;
    show_spectrum: boolean;
    spectrum_sensitivity: number;
    spectrum_smoothing: number;
    lyric_provider: LyricProvider;
    online_api: string;
    lyric_offset_ms: number;
    poll_interval_ms: number;
    demo_mode: boolean;
  };
  clipboard: {
    enabled: boolean;
    max_entries: number;
    capture_images: boolean;
    max_image_bytes: number;
    hotkey: string;
    capture_sensitive: boolean;
  };
  todo: {
    enabled: boolean;
    hotkey: string;
    carry_over: boolean;
    show_badge: boolean;
  };
  widget: {
    enabled: boolean;
    anchor: WidgetAnchor;
    offset_x: number;
    offset_y: number;
    margin: number;
    background: string;
    opacity: number;
    corner_radius: number;
    animation_ms: number;
    hide_with_autohide: boolean;
    audio_min_width: number;
    audio_max_width: number;
    lyric_min_width: number;
    lyric_max_width: number;
    flyout_width: number;
    flyout_height: number;
    flyout_flip: boolean;
    remember_tab: boolean;
  };
  ui: {
    theme: Theme;
    accent: string;
    file_logging: boolean;
    log_level: string;
  };
}

export interface MediaSnapshot {
  has_session: boolean;
  title: string;
  artist: string;
  album: string;
  source_app: string;
  status: PlaybackStatus;
  position_ms: number;
  duration_ms: number;
  can_play: boolean;
  can_pause: boolean;
  can_skip_next: boolean;
  can_skip_previous: boolean;
  /** `data:image/...;base64,...`, empty when the session has no artwork. */
  artwork: string;
}

export interface LyricLine {
  time_ms: number;
  text: string;
}

export interface Lyrics {
  lines: LyricLine[];
  source: string;
}

export interface SpectrumFrame {
  bands: number[];
  level: number;
}

export type TaskbarVisualState =
  | "applied"
  | "dynamic"
  | "hidden"
  | "fullscreen"
  | "disabled";

export interface TaskbarState {
  state: TaskbarVisualState;
  mode: string;
  secondary_bars: number;
  rect: { left: number; top: number; right: number; bottom: number } | null;
  autohide: boolean;
  /** This Windows build draws the taskbar itself, so `mode` has no effect. */
  shell_managed: boolean;
}

export type ClipKind = "text" | "link" | "image" | "files";

/** The category selector under the clipboard search box. */
export type ClipFilter = "all" | "image" | "link" | "text" | "files";

export interface ClipEntry {
  id: number;
  kind: ClipKind;
  text: string;
  preview: string;
  image_path: string;
  width: number;
  height: number;
  bytes: number;
  pinned: boolean;
  created_at: number;
  /** Text recognised inside an image; empty until OCR has run. */
  ocr_text: string;
}

export interface ClipboardStats {
  total: number;
  pinned: number;
  bytes: number;
}

export type Priority = "none" | "low" | "medium" | "high";

export interface Task {
  id: number;
  title: string;
  note: string;
  done: boolean;
  priority: Priority;
  due_at: number | null;
  list: string;
  position: number;
  created_at: number;
  completed_at: number | null;
}

export type TaskFilter = "today" | "open" | "all" | "done";
export type FlyoutTab = "todo" | "clipboard";

/** Which optional pieces of the UI are available right now. */
export interface ModuleStatus {
  taskbar: boolean;
  media: boolean;
  spectrum: boolean;
  clipboard: boolean;
  todo: boolean;
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

export const api = {
  // --- config ---------------------------------------------------------
  getConfig: () => invoke<Config>("get_config"),
  updateConfig: (config: Config) => invoke<Config>("update_config", { config }),
  getModuleStatus: () => invoke<ModuleStatus>("get_module_status"),

  // --- taskbar --------------------------------------------------------
  getTaskbarState: () => invoke<TaskbarState>("get_taskbar_state"),

  // --- media ----------------------------------------------------------
  getMedia: () => invoke<MediaSnapshot>("get_media"),
  getLyrics: () => invoke<Lyrics>("get_lyrics"),
  getLyricIndex: () => invoke<number | null>("get_lyric_index"),
  mediaControl: (action: "play" | "pause" | "toggle" | "next" | "previous") =>
    invoke<void>("media_control", { action }),

  // --- clipboard ------------------------------------------------------
  clipboardList: (query: string, filter: ClipFilter, limit: number, offset: number) =>
    invoke<ClipEntry[]>("clipboard_list", { query, filter, limit, offset }),
  clipboardCopy: (id: number) => invoke<void>("clipboard_copy", { id }),
  clipboardPin: (id: number, pinned: boolean) =>
    invoke<void>("clipboard_pin", { id, pinned }),
  clipboardDelete: (id: number) => invoke<void>("clipboard_delete", { id }),
  clipboardClear: (includePinned: boolean) =>
    invoke<number>("clipboard_clear", { includePinned }),
  clipboardStats: () => invoke<ClipboardStats>("clipboard_stats"),

  // --- todo -----------------------------------------------------------
  todoList: (filter: TaskFilter) => invoke<Task[]>("todo_list", { filter }),
  todoCreate: (title: string, list: string) =>
    invoke<Task>("todo_create", { title, list }),
  todoUpdate: (id: number, patch: Record<string, unknown>) =>
    invoke<Task>("todo_update", { id, patch }),
  todoDelete: (id: number) => invoke<void>("todo_delete", { id }),
  todoClearCompleted: () => invoke<number>("todo_clear_completed"),
  todoReorder: (ids: number[]) => invoke<void>("todo_reorder", { ids }),
  todoOpenCount: () => invoke<number>("todo_open_count"),
  todoExport: (format: "markdown" | "json") =>
    invoke<string>("todo_export", { format }),
  todoImport: (json: string) => invoke<number>("todo_import", { json }),

  // --- windows --------------------------------------------------------
  showFlyout: (tab: FlyoutTab) => invoke<void>("show_flyout", { tab }),
  hideFlyout: () => invoke<void>("hide_flyout"),
  toggleFlyout: (tab: FlyoutTab) => invoke<void>("toggle_flyout", { tab }),
  flyoutTab: () => invoke<FlyoutTab>("flyout_tab"),
  resizeWidget: (width: number) => invoke<void>("resize_widget", { width }),
  widgetHeight: () => invoke<number>("widget_height"),
  openSettings: () => invoke<void>("open_settings"),
  minimizeSettings: () => invoke<void>("minimize_settings"),
  closeSettings: () => invoke<void>("close_settings"),
  quit: () => invoke<void>("quit_app"),
  setClipboardText: (text: string) => invoke<void>("set_clipboard_text", { text }),
  /**
   * Turn a stored image path into something an `<img>` can load.
   *
   * `convertFileSrc` builds the `http://asset.localhost/...` form the webview
   * can fetch; a plain `file://` URL is blocked by the page's CSP.
   */
  assetUrl: (path: string) => Promise.resolve(convertFileSrc(path)),
  /** Fetch the configured lyric provider and report what came back. */
  testLyricProvider: () => invoke<string>("test_lyric_provider"),

  /** Reveal a directory in Explorer. */
  openPath: (path: string) => invoke<void>("open_path", { path }),

  // --- app ------------------------------------------------------------
  /** Version, config path, data directory and friends, for the About page. */
  getAppInfo: () => invoke<Record<string, string>>("get_app_info"),
};

// ---------------------------------------------------------------------------
// Events from Rust
// ---------------------------------------------------------------------------

export interface LyricIndexPayload {
  index: number | null;
}

export interface EventMap {
  "media-changed": MediaSnapshot;
  "lyrics-changed": Lyrics;
  "lyric-index": LyricIndexPayload;
  spectrum: SpectrumFrame;
  "taskbar-changed": TaskbarState;
  "clipboard-changed": null;
  "todo-changed": null;
  "config-changed": Config;
}

export function on<K extends keyof EventMap>(
  event: K,
  handler: (payload: EventMap[K]) => void,
): Promise<UnlistenFn> {
  return listen<EventMap[K]>(event, (e) => handler(e.payload));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** `rgb()`/`rgba()` string for a `#RRGGBB` config colour. */
export function withAlpha(hex: string, alpha: number): string {
  const { r, g, b } = parseHex(hex);
  return `rgba(${r}, ${g}, ${b}, ${Math.max(0, Math.min(1, alpha))})`;
}

export function parseHex(hex: string): { r: number; g: number; b: number } {
  const h = (hex ?? "").replace("#", "").trim();
  const full =
    h.length === 3
      ? h
          .split("")
          .map((c) => c + c)
          .join("")
      : h.padEnd(6, "0").slice(0, 6);
  return {
    r: parseInt(full.slice(0, 2), 16) || 0,
    g: parseInt(full.slice(2, 4), 16) || 0,
    b: parseInt(full.slice(4, 6), 16) || 0,
  };
}

/** Human-friendly byte size. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** Relative time for clipboard rows, in Chinese to match the UI. */
export function formatRelative(timestampMs: number): string {
  const delta = Date.now() - timestampMs;
  if (delta < 60_000) return "刚刚";
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} 分钟前`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} 小时前`;
  const days = Math.floor(delta / 86_400_000);
  if (days === 1) return "昨天";
  if (days < 30) return `${days} 天前`;
  return new Date(timestampMs).toLocaleDateString("zh-CN");
}

export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return "0:00";
  const total = Math.floor(ms / 1000);
  const minutes = Math.floor(total / 60);
  const seconds = total % 60;
  return `${minutes}:${String(seconds).padStart(2, "0")}`;
}

/**
 * Width of `text` when rendered in `font`, measured with a canvas.
 *
 * The widget bar's adaptive width depends on this: estimating from character
 * counts gets CJK and mixed-script titles badly wrong, and the bar visibly
 * jitters as it settles.
 */
const measureCanvas = document.createElement("canvas");
const measureCtx = measureCanvas.getContext("2d");

export function measureText(text: string, font: string): number {
  if (!measureCtx) return text.length * 8;
  measureCtx.font = font;
  return measureCtx.measureText(text).width;
}

export function clamp(value: number, min: number, max: number): number {
  return Math.min(Math.max(value, min), max);
}

/** Debounce helper for the settings sliders. */
export function debounce<A extends unknown[]>(
  fn: (...args: A) => void,
  ms: number,
): (...args: A) => void {
  let timer: number | undefined;
  return (...args: A) => {
    if (timer !== undefined) window.clearTimeout(timer);
    timer = window.setTimeout(() => fn(...args), ms);
  };
}
