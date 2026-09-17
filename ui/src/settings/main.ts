/**
 * Settings centre.
 *
 * The whole UI is generated from the declarative field list in `SECTIONS`, so
 * adding a setting means adding one entry here plus the matching field in
 * `beautify_core::config`. There is no framework: each control writes straight
 * into the in-memory `Config` and a debounced flush pushes it to Rust, which
 * applies it to the live modules and writes `config.toml`.
 */

import {
  api,
  debounce,
  on,
  type Config,
  type ModuleStatus,
  type TaskbarMode,
} from "../shared/bridge";

// ---------------------------------------------------------------------------
// Field schema
// ---------------------------------------------------------------------------

type Field =
  | { kind: "switch"; path: string; label: string; hint?: string; when?: (c: Config) => boolean }
  | {
      kind: "slider";
      path: string;
      label: string;
      min: number;
      max: number;
      step: number;
      hint?: string;
      format?: (v: number) => string;
      when?: (c: Config) => boolean;
    }
  | {
      kind: "select";
      path: string;
      label: string;
      options: { value: string; label: string }[];
      hint?: string;
      when?: (c: Config) => boolean;
    }
  | { kind: "color"; path: string; label: string; hint?: string; when?: (c: Config) => boolean }
  | {
      kind: "number";
      path: string;
      label: string;
      min?: number;
      max?: number;
      suffix?: string;
      hint?: string;
      when?: (c: Config) => boolean;
    }
  | {
      kind: "text";
      path: string;
      label: string;
      placeholder?: string;
      hint?: string;
      when?: (c: Config) => boolean;
    }
  | { kind: "status"; label: string; hint?: string; render: () => HTMLElement }
  | { kind: "action"; label: string; hint?: string; buttons: ActionButton[]; when?: (c: Config) => boolean }
  | { kind: "custom"; label: string; hint?: string; render: () => HTMLElement };

interface ActionButton {
  text: string;
  variant?: "primary" | "danger";
  onClick: () => void | Promise<void>;
}

interface Section {
  id: string;
  title: string;
  icon: string;
  description: string;
  cards: { title?: string; fields: Field[] }[];
}

// Field paths are dotted lookups into the config object; `get`/`set` below are
// the only place that knows about the shape.
const MODE_OPTIONS: { value: TaskbarMode; label: string }[] = [
  { value: "normal", label: "跟随系统（不做修改）" },
  { value: "clear", label: "全透明" },
  { value: "blur", label: "模糊" },
  { value: "acrylic", label: "亚克力 Acrylic" },
  { value: "mica", label: "云母 Mica（Win11 22H2+）" },
  { value: "opaque", label: "纯色不透明" },
];

const COLOR_MODES: TaskbarMode[] = ["clear", "blur", "acrylic", "mica", "opaque"];
const tintNeeded = (c: Config) => COLOR_MODES.includes(c.taskbar.mode);

function SECTIONS(status: () => ModuleStatus): Section[] {
  return [
    {
      id: "appearance",
      title: "外观",
      icon: ICON_PALETTE,
      description: "主题、强调色，以及小组件外观。这些设置只影响 WinBeautify 自己的窗口。",
      cards: [
        {
          fields: [
            {
              kind: "select",
              path: "ui.theme",
              label: "主题",
              hint: "跟随系统时使用 Windows 的浅色/深色应用模式。",
              options: [
                { value: "dark", label: "深色" },
                { value: "light", label: "浅色" },
                { value: "auto", label: "跟随系统" },
              ],
            },
            { kind: "color", path: "ui.accent", label: "强调色", hint: "用于选中项、开关与焦点框。" },
          ],
        },
        {
          title: "小组件",
          fields: [
            { kind: "color", path: "widget.background", label: "背景色" },
            {
              kind: "slider",
              path: "widget.opacity",
              label: "背景不透明度",
              min: 0,
              max: 1,
              step: 0.01,
              format: percent,
            },
            {
              kind: "slider",
              path: "widget.corner_radius",
              label: "圆角",
              min: 0,
              max: 8,
              step: 1,
              format: (v) => (v === 0 ? "直角" : `${v} px`),
              hint: "窗口圆角由 Windows 绘制，最大 8px。",
            },
            {
              kind: "slider",
              path: "widget.animation_ms",
              label: "宽度过渡时长",
              min: 60,
              max: 400,
              step: 10,
              format: (v) => `${v} ms`,
              hint:
                "组件宽度会跟随歌词长度变化，这里控制变化的动画时长，建议 120–180ms。",
            },
          ],
        },
      ],
    },

    {
      id: "taskbar",
      title: "任务栏",
      icon: ICON_TASKBAR,
      description:
        "通过 DWM 与合成 API 为任务栏叠加透明、模糊或材质背景，不修改任何系统文件。" +
        "注意：Windows 11 22H2 之后的任务栏由 XAML 绘制，可能忽略这些设置，此时下方状态会显示「已应用」但外观不变。",
      cards: [
        {
          fields: [
            { kind: "switch", path: "taskbar.enabled", label: "启用任务栏美化" },
            {
              kind: "status",
              label: "当前状态",
              hint: "实时反映任务栏模块正在做什么。",
              render: taskbarStatus,
            },
          ],
        },
        {
          title: "效果",
          fields: [
            {
              kind: "select",
              path: "taskbar.mode",
              label: "背景模式",
              options: MODE_OPTIONS,
              when: (c) => c.taskbar.enabled,
            },
            {
              kind: "color",
              path: "taskbar.color",
              label: "着色",
              when: (c) => c.taskbar.enabled && tintNeeded(c),
            },
            {
              kind: "slider",
              path: "taskbar.opacity",
              label: "着色不透明度",
              min: 0,
              max: 1,
              step: 0.01,
              format: percent,
              when: (c) => c.taskbar.enabled && tintNeeded(c),
            },
            {
              kind: "switch",
              path: "taskbar.apply_to_secondary",
              label: "应用到副屏任务栏",
              when: (c) => c.taskbar.enabled,
            },
          ],
        },
        {
          title: "行为",
          fields: [
            {
              kind: "switch",
              path: "taskbar.dynamic_mode",
              label: "动态模式",
              hint: "当显示器上有窗口最大化时切换为指定效果，窗口还原后恢复。",
              when: (c) => c.taskbar.enabled,
            },
            {
              kind: "select",
              path: "taskbar.dynamic_mode_override",
              label: "最大化时使用",
              options: MODE_OPTIONS,
              when: (c) => c.taskbar.enabled && c.taskbar.dynamic_mode,
            },
            {
              kind: "switch",
              path: "taskbar.hide_on_fullscreen",
              label: "全屏应用时还原为系统默认",
              hint: "避免在游戏或全屏视频上叠加异常的背景。",
              when: (c) => c.taskbar.enabled,
            },
            {
              kind: "switch",
              path: "taskbar.restore_on_exit",
              label: "退出时还原任务栏",
              hint: "强烈建议保持开启；关闭后退出程序会保留当前效果直到注销。",
              when: (c) => c.taskbar.enabled,
            },
          ],
        },
      ],
    },

    {
      id: "widget",
      title: "小组件栏",
      icon: ICON_WIDGET,
      description: "嵌入任务栏的 Widget Bar：启动器、音频组件与 Flyout。",
      cards: [
        {
          fields: [
            { kind: "switch", path: "widget.enabled", label: "显示小组件栏" },
            {
              kind: "select",
              path: "widget.renderer",
              label: "渲染方式",
              hint:
                "原生渲染由 Direct2D 直接绘制，内存占用约 30MB；WebView2 是早期实现，" +
                "功能相同但会额外拉起 Chromium 进程树（实测约 350MB）。仅在原生渲染出现问题时才建议切换。",
              options: [
                { value: "native", label: "原生渲染（推荐，占用低）" },
                { value: "webview", label: "WebView2（兼容回退）" },
              ],
              when: (c) => c.widget.enabled,
            },
            {
              kind: "switch",
              path: "widget.hide_with_autohide",
              label: "任务栏自动隐藏时一并隐藏",
              when: (c) => c.widget.enabled,
            },
          ],
        },
        {
          title: "位置",
          fields: [
            {
              kind: "select",
              path: "widget.anchor",
              label: "锚点",
              hint: "任务栏内锚点会贴住开始按钮或通知区域；屏幕锚点则使用工作区底部。",
              options: [
                { value: "taskbar-right", label: "任务栏 · 通知区域左侧" },
                { value: "taskbar-center", label: "任务栏 · 居中" },
                { value: "taskbar-left", label: "任务栏 · 左侧" },
                { value: "bottom-right", label: "屏幕 · 右下" },
                { value: "bottom-center", label: "屏幕 · 底部居中" },
                { value: "bottom-left", label: "屏幕 · 左下" },
              ],
              when: (c) => c.widget.enabled,
            },
            {
              kind: "number",
              path: "widget.offset_x",
              label: "水平偏移",
              min: -400,
              max: 400,
              suffix: "px",
              when: (c) => c.widget.enabled,
            },
            {
              kind: "number",
              path: "widget.offset_y",
              label: "垂直偏移",
              min: -60,
              max: 60,
              suffix: "px",
              when: (c) => c.widget.enabled,
            },
            {
              kind: "number",
              path: "widget.margin",
              label: "边距",
              min: 0,
              max: 40,
              suffix: "px",
              hint: "小组件与任务栏上下边缘之间留出的空隙。",
              when: (c) => c.widget.enabled,
            },
          ],
        },
        {
          title: "音频组件宽度",
          fields: [
            {
              kind: "number",
              path: "widget.audio_min_width",
              label: "最小宽度",
              min: 96,
              max: 900,
              suffix: "px",
              when: (c) => c.widget.enabled,
            },
            {
              kind: "number",
              path: "widget.audio_max_width",
              label: "最大宽度",
              min: 96,
              max: 1600,
              suffix: "px",
              when: (c) => c.widget.enabled,
            },
            {
              kind: "number",
              path: "widget.lyric_min_width",
              label: "歌词最小宽度",
              min: 0,
              max: 1200,
              suffix: "px",
              when: (c) => c.widget.enabled && c.media.show_lyrics,
            },
            {
              kind: "number",
              path: "widget.lyric_max_width",
              label: "歌词最大宽度",
              min: 0,
              max: 1600,
              suffix: "px",
              when: (c) => c.widget.enabled && c.media.show_lyrics,
            },
          ],
        },
        {
          title: "Flyout",
          fields: [
            {
              kind: "number",
              path: "widget.flyout_width",
              label: "宽度",
              min: 260,
              max: 900,
              suffix: "px",
              when: (c) => c.widget.enabled,
            },
            {
              kind: "number",
              path: "widget.flyout_height",
              label: "高度",
              min: 240,
              max: 1200,
              suffix: "px",
              when: (c) => c.widget.enabled,
            },
            {
              kind: "switch",
              path: "widget.flyout_flip",
              label: "空间不足时向上展开",
              when: (c) => c.widget.enabled,
            },
            {
              kind: "switch",
              path: "widget.remember_tab",
              label: "记住上次打开的标签页",
              when: (c) => c.widget.enabled,
            },
          ],
        },
      ],
    },

    {
      id: "media",
      title: "媒体与歌词",
      icon: ICON_MUSIC,
      description: "通过 Windows 媒体会话（GSMTC）读取正在播放的内容，并从系统音频回放中提取频谱。",
      cards: [
        {
          fields: [
            { kind: "switch", path: "media.enabled", label: "启用媒体模块" },
            {
              kind: "status",
              label: "频谱采集",
              hint: "频谱来自默认播放设备的回环捕获，不需要任何虚拟声卡。",
              render: () => spectrumStatus(status()),
            },
          ],
        },
        {
          title: "显示",
          fields: [
            { kind: "switch", path: "media.show_lyrics", label: "显示歌词", when: (c) => c.media.enabled },
            { kind: "switch", path: "media.show_spectrum", label: "显示频谱", when: (c) => c.media.enabled },
            {
              kind: "slider",
              path: "media.spectrum_sensitivity",
              label: "灵敏度",
              min: 0.2,
              max: 4,
              step: 0.1,
              format: (v) => `${v.toFixed(1)}×`,
              when: (c) => c.media.enabled && c.media.show_spectrum,
            },
            {
              kind: "slider",
              path: "media.spectrum_smoothing",
              label: "回落平滑",
              min: 0,
              max: 0.95,
              step: 0.05,
              format: (v) => v.toFixed(2),
              hint: "数值越大，频谱柱回落越慢。",
              when: (c) => c.media.enabled && c.media.show_spectrum,
            },
          ],
        },
        {
          title: "歌词来源",
          fields: [
            {
              kind: "select",
              path: "media.lyric_provider",
              label: "歌词来源",
              options: [
                { value: "off", label: "关闭（只读本地 .lrc）" },
                { value: "netease", label: "网易云音乐" },
                { value: "qq", label: "QQ 音乐" },
                { value: "kugou", label: "酷狗音乐" },
                { value: "lrclib", label: "LRCLIB（国际曲库）" },
                { value: "custom", label: "自定义接口" },
              ],
              hint:
                "除「关闭」外都会把当前歌曲的名称与歌手发送给对应平台以查询歌词，找不到时会" +
                "自动尝试其它平台。查询结果只存在内存里，不会写入磁盘；介意联网就选「关闭」，" +
                "把 .lrc 文件放进歌词目录即可离线显示。",
              when: (c) => c.media.enabled,
            },
            {
              kind: "text",
              path: "media.online_api",
              label: "自定义接口地址",
              placeholder: "https://example.com/lrc?title={title}&artist={artist}",
              hint: "支持 {title} / {artist} / {album} 占位符；返回 LRC，或带 lyric/lrc 字段的 JSON。",
              when: (c) => c.media.enabled && c.media.lyric_provider === "custom",
            },
            {
              kind: "action",
              label: "歌词排错",
              hint:
                "歌词目录里的 .lrc 文件名需为「歌手 - 歌名.lrc」，优先于在线结果。" +
                "点「测试当前来源」会用正在播放的歌实际请求一次并告诉你结果。",
              buttons: [
                { text: "测试当前来源", onClick: () => testLyricProvider() },
                { text: "打开歌词目录", onClick: () => openPath("lyrics") },
              ],
              when: (c) => c.media.enabled,
            },
            {
              kind: "slider",
              path: "media.lyric_offset_ms",
              label: "歌词偏移",
              min: -5000,
              max: 5000,
              step: 50,
              format: (v) => `${v > 0 ? "+" : ""}${v} ms`,
              hint: "正值让歌词提前显示；用于补偿不同平台的进度上报延迟。",
              when: (c) => c.media.enabled && c.media.show_lyrics,
            },
            {
              kind: "slider",
              path: "media.poll_interval_ms",
              label: "兜底刷新间隔",
              min: 500,
              max: 10000,
              step: 250,
              format: (v) => `${v} ms`,
              hint: "播放状态由系统事件推送，这里只是防止遗漏的兜底轮询。",
              when: (c) => c.media.enabled,
            },
          ],
        },
        {
          title: "预览",
          fields: [
            {
              kind: "switch",
              path: "media.demo_mode",
              label: "预览模式",
              hint: "显示一段演示曲目、歌词与频谱，用来在没有播放任何内容时调整外观。",
              when: (c) => c.media.enabled,
            },
            {
              kind: "action",
              label: "歌词文件",
              hint: "歌词目录中的 .lrc 文件会被优先使用；在线获取的歌词不会写入这里。",
              buttons: [
                { text: "打开歌词目录", onClick: () => openPath("lyrics") },
                { text: "打开数据目录", onClick: () => openPath("data") },
              ],
            },
          ],
        },
      ],
    },

    {
      id: "clipboard",
      title: "剪贴板",
      icon: ICON_CLIPBOARD,
      description: "监听系统剪贴板变化并保存历史，支持文本、图片与文件列表。",
      cards: [
        {
          fields: [
            { kind: "switch", path: "clipboard.enabled", label: "启用剪贴板历史" },
            {
              kind: "status",
              label: "占用",
              render: clipboardSummary,
            },
          ],
        },
        {
          title: "记录规则",
          fields: [
            {
              kind: "number",
              path: "clipboard.max_entries",
              label: "最多保存条数",
              min: 10,
              max: 5000,
              suffix: "条",
              hint: "收藏的条目不会被自动清除。",
              when: (c) => c.clipboard.enabled,
            },
            {
              kind: "switch",
              path: "clipboard.capture_images",
              label: "保存图片",
              when: (c) => c.clipboard.enabled,
            },
            {
              kind: "number",
              path: "clipboard.max_image_bytes",
              label: "图片大小上限",
              min: 0,
              max: 67108864,
              suffix: "字节",
              hint: "超过上限的图片会被忽略。0 表示不限制。",
              when: (c) => c.clipboard.enabled && c.clipboard.capture_images,
            },
            {
              kind: "switch",
              path: "clipboard.capture_sensitive",
              label: "记录被标记为「不记录」的内容",
              hint: "密码管理器等程序会主动标记这类内容。默认忽略，仅在确实需要时开启。",
              when: (c) => c.clipboard.enabled,
            },
          ],
        },
        {
          title: "数据",
          fields: [
            {
              kind: "action",
              label: "清理历史",
              hint: "收藏的条目默认保留。",
              buttons: [
                { text: "清空未收藏", variant: "danger", onClick: () => clearClips(false) },
                { text: "全部清空", variant: "danger", onClick: () => clearClips(true) },
              ],
            },
          ],
        },
      ],
    },

    {
      id: "todo",
      title: "任务清单",
      icon: ICON_CHECK,
      description: "轻量的待办列表，可从小组件栏的 Flyout 直接查看与勾选。",
      cards: [
        {
          fields: [
            { kind: "switch", path: "todo.enabled", label: "启用任务清单" },
            {
              kind: "switch",
              path: "todo.show_badge",
              label: "在启动器上显示未完成数量",
              when: (c) => c.todo.enabled,
            },
            {
              kind: "switch",
              path: "todo.carry_over",
              label: "启动时把未完成任务顺延到今天",
              when: (c) => c.todo.enabled,
            },
          ],
        },
        {
          title: "导出",
          fields: [
            {
              kind: "action",
              label: "导出任务",
              hint: "复制到剪贴板，方便粘贴进笔记或提交信息。",
              buttons: [
                { text: "复制 Markdown", onClick: () => exportTodos("markdown") },
                { text: "复制 JSON", onClick: () => exportTodos("json") },
              ],
            },
          ],
        },
      ],
    },

    {
      id: "system",
      title: "系统",
      icon: ICON_SYSTEM,
      description: "开机启动、快捷键与日志。",
      cards: [
        {
          fields: [
            {
              kind: "switch",
              path: "general.autostart",
              label: "开机自动启动",
              hint: "写入当前用户的启动项，不需要管理员权限。",
            },
            {
              kind: "text",
              path: "clipboard.hotkey",
              label: "剪贴板快捷键",
              placeholder: "Ctrl+Alt+V",
              hint: "格式如 Ctrl+Alt+V；留空则不注册。修改后立即生效。",
            },
            {
              kind: "text",
              path: "todo.hotkey",
              label: "任务清单快捷键",
              placeholder: "Ctrl+Alt+T",
              hint: "留空则不注册。",
            },
          ],
        },
        {
          title: "日志",
          fields: [
            { kind: "switch", path: "ui.file_logging", label: "写入日志文件" },
            {
              kind: "select",
              path: "ui.log_level",
              label: "日志级别",
              options: [
                { value: "error", label: "仅错误" },
                { value: "warn", label: "警告及以上" },
                { value: "info", label: "常规" },
                { value: "debug", label: "调试" },
              ],
              hint: "修改后需要重启 WinBeautify 才会生效。",
              when: (c) => c.ui.file_logging,
            },
            {
              kind: "action",
              label: "打开位置",
              buttons: [
                { text: "打开数据目录", onClick: () => openPath("data") },
                { text: "打开日志目录", onClick: () => openPath("logs") },
              ],
            },
          ],
        },
        {
          title: "运行",
          fields: [
            {
              kind: "action",
              label: "WinBeautify 在后台托盘运行",
              hint: "小组件栏与全部模块都运行在这个进程里；关闭设置窗口不会退出程序。",
              buttons: [
                { text: "打开设置窗口", onClick: () => api.openSettings() },
                { text: "退出 WinBeautify", variant: "danger", onClick: () => api.quit() },
              ],
            },
          ],
        },
      ],
    },

    {
      id: "about",
      title: "关于",
      icon: ICON_INFO,
      description: "",
      cards: [],
    },
  ];
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

let config: Config | null = null;
let moduleStatus: ModuleStatus = {
  taskbar: false,
  media: false,
  spectrum: false,
  clipboard: false,
  todo: false,
};
let activeId = "appearance";
let appInfo: Record<string, string> = {};

const el = {
  sidebar: document.getElementById("sidebar") as HTMLElement,
  content: document.getElementById("content") as HTMLElement,
  saveState: document.getElementById("save-state") as HTMLDivElement,
  minimize: document.getElementById("win-min") as HTMLButtonElement,
  close: document.getElementById("win-close") as HTMLButtonElement,
};

// ---------------------------------------------------------------------------
// Config path access
// ---------------------------------------------------------------------------

function get(cfg: Config, path: string): unknown {
  return path.split(".").reduce<unknown>((acc, key) => {
    if (acc && typeof acc === "object") return (acc as Record<string, unknown>)[key];
    return undefined;
  }, cfg);
}

function set(cfg: Config, path: string, value: unknown) {
  const parts = path.split(".");
  const last = parts.pop()!;
  const target = parts.reduce<Record<string, unknown>>((acc, key) => {
    return acc[key] as Record<string, unknown>;
  }, cfg as unknown as Record<string, unknown>);
  target[last] = value;
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

const flush = debounce(async () => {
  if (!config) return;
  setSaveState("saving", "正在保存…");
  try {
    // Rust clamps out-of-range values and echoes back what it actually stored,
    // so the UI and the running modules can never disagree.
    const applied = await api.updateConfig(config);
    config = applied;
    setSaveState("saved", "已保存");
    window.setTimeout(() => setSaveState("idle", ""), 1400);
  } catch (error) {
    console.error("failed to save config", error);
    setSaveState("error", "保存失败，详见日志");
  }
}, 250);

/** Change a field, re-render (for conditional rows) and schedule a save. */
function commit(path: string, value: unknown) {
  if (!config) return;
  set(config, path, value);
  applyLiveStyles();
  render();
  flush();
}

function setSaveState(state: string, text: string) {
  el.saveState.dataset.state = state;
  el.saveState.textContent = text;
}

function applyLiveStyles() {
  if (!config) return;
  document.documentElement.dataset.theme = config.ui.theme;
  document.documentElement.style.setProperty("--accent", config.ui.accent);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

function render() {
  if (!config) return;
  const sections = SECTIONS(() => moduleStatus);
  renderSidebar(sections);

  const section = sections.find((s) => s.id === activeId) ?? sections[0];
  renderContent(section);
}

function renderSidebar(sections: Section[]) {
  el.sidebar.replaceChildren();
  for (const section of sections) {
    if (section.id === "about") {
      const spacer = document.createElement("div");
      spacer.className = "nav-spacer";
      el.sidebar.append(spacer);
    }
    const button = document.createElement("button");
    button.type = "button";
    button.className = "nav-item";
    button.dataset.id = section.id;
    if (section.id === activeId) button.setAttribute("aria-current", "page");
    button.innerHTML = section.icon;
    button.append(document.createTextNode(section.title));
    button.addEventListener("click", () => {
      activeId = section.id;
      render();
      el.content.scrollTop = 0;
    });
    el.sidebar.append(button);
  }
}

function renderContent(section: Section) {
  el.content.replaceChildren();

  if (section.id === "about") {
    el.content.append(aboutPage());
    return;
  }

  const head = document.createElement("div");
  head.className = "section-head";
  const title = document.createElement("h2");
  title.textContent = section.title;
  const description = document.createElement("p");
  description.textContent = section.description;
  head.append(title, description);
  el.content.append(head);

  for (const card of section.cards) {
    el.content.append(renderCard(card));
  }
}

function renderCard(card: { title?: string; fields: Field[] }): HTMLElement {
  const wrapper = document.createElement("section");
  wrapper.className = "card";
  if (card.title) {
    const heading = document.createElement("h3");
    heading.textContent = card.title;
    wrapper.append(heading);
  }
  for (const field of card.fields) {
    // `custom` has no condition; the rest — including `action` — may.
    if (field.kind !== "custom" && "when" in field) {
      if (field.when && !field.when(config!)) continue;
    }
    wrapper.append(renderRow(field));
  }
  return wrapper;
}

function renderRow(field: Field): HTMLElement {
  const row = document.createElement("div");
  row.className = "row";

  const labelBox = document.createElement("div");
  labelBox.className = "row-label";
  if (field.kind !== "custom") {
    const label = document.createElement("div");
    label.className = "label";
    label.textContent = field.label;
    labelBox.append(label);
  }
  if ("hint" in field && field.hint) {
    const hint = document.createElement("div");
    hint.className = "hint";
    hint.textContent = field.hint;
    labelBox.append(hint);
  }

  const control = document.createElement("div");
  control.className = "row-control";

  switch (field.kind) {
    case "switch":
      control.append(buildSwitch(field.path));
      break;
    case "slider":
      control.append(buildSlider(field));
      break;
    case "select":
      control.append(buildSelect(field));
      break;
    case "color":
      control.append(buildColor(field.path));
      break;
    case "number":
      control.append(buildNumber(field));
      break;
    case "text":
      control.append(buildText(field));
      break;
    case "status":
      labelBox.replaceChildren();
      control.append(field.render());
      break;
    case "action":
      labelBox.replaceChildren();
      control.classList.add("buttons");
      for (const button of field.buttons) {
        control.append(buildButton(button));
      }
      break;
    case "custom":
      labelBox.replaceChildren();
      control.append(field.render());
      break;
  }

  if (field.kind === "status" || field.kind === "action") {
    // These rows still want a description, just not a title.
    row.append(labelBox, control);
  } else {
    row.append(labelBox, control);
  }
  return row;
}

function buildSwitch(path: string): HTMLElement {
  const value = Boolean(get(config!, path));
  const button = document.createElement("button");
  button.type = "button";
  button.className = "switch";
  button.setAttribute("role", "switch");
  button.setAttribute("aria-checked", String(value));
  button.addEventListener("click", () => commit(path, !value));
  return button;
}

function buildSlider(field: Extract<Field, { kind: "slider" }>): HTMLElement {
  const value = Number(get(config!, field.path));
  const wrapper = document.createElement("div");
  wrapper.className = "slider";

  const input = document.createElement("input");
  input.type = "range";
  input.min = String(field.min);
  input.max = String(field.max);
  input.step = String(field.step);
  input.value = String(value);

  const readout = document.createElement("span");
  readout.className = "value";
  const format = field.format ?? ((v: number) => String(v));
  readout.textContent = format(value);

  input.addEventListener("input", () => {
    const next = Number(input.value);
    readout.textContent = format(next);
    // Live-apply while dragging without re-rendering the whole pane, which
    // would steal the drag from the slider.
    if (config) set(config, field.path, next);
    applyLiveStyles();
  });
  input.addEventListener("change", () => commit(field.path, Number(input.value)));

  wrapper.append(input, readout);
  return wrapper;
}

function buildSelect(field: Extract<Field, { kind: "select" }>): HTMLElement {
  const select = document.createElement("select");
  select.className = "select";
  for (const option of field.options) {
    const node = document.createElement("option");
    node.value = option.value;
    node.textContent = option.label;
    if (get(config!, field.path) === option.value) node.selected = true;
    select.append(node);
  }
  select.addEventListener("change", () => commit(field.path, select.value));
  return select;
}

function buildColor(path: string): HTMLElement {
  const current = String(get(config!, path) ?? "#000000");
  const wrapper = document.createElement("div");
  wrapper.style.cssText = "display:flex;gap:8px;align-items:center;";

  const picker = document.createElement("input");
  picker.type = "color";
  picker.className = "color-input";
  picker.value = hexToInputColor(current);

  const text = document.createElement("input");
  text.type = "text";
  text.className = "color-hex";
  text.value = current.toUpperCase();
  text.maxLength = 7;

  picker.addEventListener("input", () => {
    text.value = picker.value.toUpperCase();
    if (config) set(config, path, picker.value.toUpperCase());
    applyLiveStyles();
  });
  picker.addEventListener("change", () => commit(path, picker.value.toUpperCase()));

  text.addEventListener("change", () => {
    const normalized = normalizeHex(text.value);
    if (!normalized) {
      text.value = String(get(config!, path)).toUpperCase();
      return;
    }
    picker.value = normalized;
    commit(path, normalized.toUpperCase());
  });

  wrapper.append(picker, text);
  return wrapper;
}

function buildNumber(field: Extract<Field, { kind: "number" }>): HTMLElement {
  const wrapper = document.createElement("div");
  wrapper.style.cssText = "display:flex;gap:6px;align-items:center;";

  const input = document.createElement("input");
  input.type = "number";
  input.className = "number-input";
  if (field.min !== undefined) input.min = String(field.min);
  if (field.max !== undefined) input.max = String(field.max);
  input.value = String(get(config!, field.path));

  input.addEventListener("change", () => {
    let next = Number(input.value);
    if (!Number.isFinite(next)) {
      input.value = String(get(config!, field.path));
      return;
    }
    if (field.min !== undefined) next = Math.max(field.min, next);
    if (field.max !== undefined) next = Math.min(field.max, next);
    input.value = String(next);
    commit(field.path, next);
  });

  wrapper.append(input);
  if (field.suffix) {
    const suffix = document.createElement("span");
    suffix.className = "value";
    suffix.textContent = field.suffix;
    wrapper.append(suffix);
  }
  return wrapper;
}

function buildText(field: Extract<Field, { kind: "text" }>): HTMLElement {
  const input = document.createElement("input");
  input.type = "text";
  input.className = "text-input";
  input.value = String(get(config!, field.path) ?? "");
  if (field.placeholder) input.placeholder = field.placeholder;
  input.spellcheck = false;
  // Text fields are the one place where every keystroke should not hit the
  // disk; `change` fires on blur or Enter.
  input.addEventListener("change", () => commit(field.path, input.value.trim()));
  return input;
}

function buildButton(spec: ActionButton): HTMLButtonElement {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "btn" + (spec.variant ? ` ${spec.variant}` : "");
  button.textContent = spec.text;
  button.addEventListener("click", () => void spec.onClick());
  return button;
}

// ---------------------------------------------------------------------------
// Status rows
// ---------------------------------------------------------------------------

function taskbarStatus(): HTMLElement {
  const pill = document.createElement("span");
  pill.className = "pill";
  void api.getTaskbarState().then((state) => {
    const labels: Record<string, { text: string; tone: string }> = {
      applied: { text: `已应用 · ${modeLabel(state.mode)}`, tone: "on" },
      dynamic: { text: `动态模式 · ${modeLabel(state.mode)}`, tone: "on" },
      hidden: { text: "任务栏已自动隐藏", tone: "warn" },
      fullscreen: { text: "全屏应用前台，已暂停", tone: "warn" },
      disabled: { text: "未启用", tone: "off" },
    };
    const info = labels[state.state] ?? { text: state.state, tone: "off" };
    // Reading "已应用" while nothing changed on screen is worse than saying
    // nothing: from Windows 11 22H2 the taskbar is drawn by the shell and the
    // request is ignored, so report the limitation instead of the intent.
    if (state.shell_managed && state.mode !== "normal") {
      pill.textContent = "系统自带任务栏不响应（Win11 22H2 起）";
      pill.title =
        "Win11 22H2 起任务栏由系统自己绘制，本程序的透明/亚克力设置会被系统忽略。" +
        "系统设置里的「透明效果」仍然有效。";
      pill.dataset.state = "warn";
      return;
    }
    pill.textContent =
      state.secondary_bars > 0 ? `${info.text} · 副屏 ${state.secondary_bars}` : info.text;
    pill.dataset.state = info.tone;
  });
  pill.textContent = "读取中…";
  return pill;
}

function spectrumStatus(status: ModuleStatus): HTMLElement {
  const pill = document.createElement("span");
  pill.className = "pill";
  if (!config?.media.show_spectrum) {
    pill.textContent = "已关闭";
    pill.dataset.state = "off";
    return pill;
  }
  pill.textContent = status.spectrum ? "正在采集系统音频" : "不可用（无播放设备）";
  pill.dataset.state = status.spectrum ? "on" : "warn";
  return pill;
}

function clipboardSummary(): HTMLElement {
  const pill = document.createElement("span");
  pill.className = "pill";
  pill.textContent = "读取中…";
  void api.clipboardStats().then((stats) => {
    const bytes =
      stats.bytes < 1024 * 1024
        ? `${(stats.bytes / 1024).toFixed(0)} KB`
        : `${(stats.bytes / 1024 / 1024).toFixed(1)} MB`;
    pill.textContent = `${stats.total} 条 · 收藏 ${stats.pinned} · 约 ${bytes}`;
    pill.dataset.state = stats.total > 0 ? "on" : "off";
  });
  return pill;
}

function modeLabel(id: string): string {
  return MODE_OPTIONS.find((m) => m.value === id)?.label ?? id;
}

// ---------------------------------------------------------------------------
// About page
// ---------------------------------------------------------------------------

function aboutPage(): HTMLElement {
  const wrapper = document.createElement("div");

  const hero = document.createElement("div");
  hero.className = "about-hero";
  const logo = document.createElement("img");
  logo.src = "./icons/128x128.png";
  logo.alt = "";
  const text = document.createElement("div");
  const h2 = document.createElement("h2");
  h2.textContent = "WinBeautify";
  const p = document.createElement("p");
  p.textContent = "Windows 桌面美化与增强工具 · Rust + Tauri";
  text.append(h2, p);
  hero.append(logo, text);
  wrapper.append(hero);

  const card = document.createElement("section");
  card.className = "card";
  const list = document.createElement("dl");
  list.className = "about-list";
  for (const [key, value] of Object.entries(appInfo)) {
    const dt = document.createElement("dt");
    dt.textContent = key;
    const dd = document.createElement("dd");
    if (value.startsWith("http") || value.includes(":\\")) {
      dd.className = "mono";
      dd.textContent = value;
    } else {
      dd.textContent = value;
    }
    list.append(dt, dd);
  }
  card.append(list);
  wrapper.append(card);

  const notes = document.createElement("section");
  notes.className = "card";
  const head = document.createElement("h3");
  head.textContent = "说明";
  const body = document.createElement("div");
  body.className = "row";
  const p2 = document.createElement("div");
  p2.className = "row-label";
  p2.innerHTML =
    '<div class="hint">任务栏效果通过 DWM 与未公开的合成 API 实现，不修改任何系统文件，退出时自动还原。' +
    "全部模块按需加载，空闲时不轮询，因此常驻后台的资源占用很低。</div>";
  body.append(p2);
  notes.append(head, body);
  wrapper.append(notes);

  return wrapper;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function percent(v: number): string {
  return `${Math.round(v * 100)}%`;
}

function hexToInputColor(hex: string): string {
  const normalized = normalizeHex(hex);
  return normalized ?? "#000000";
}

function normalizeHex(input: string): string | null {
  const h = input.replace("#", "").trim();
  if (/^[0-9a-fA-F]{6}$/.test(h)) return `#${h.toLowerCase()}`;
  if (/^[0-9a-fA-F]{3}$/.test(h)) {
    return `#${h
      .split("")
      .map((c) => c + c)
      .join("")
      .toLowerCase()}`;
  }
  return null;
}

/// Fetch the configured provider with the current track and report what came back.
///
/// A wrong URL is the most likely reason lyrics stay empty, and the failure is
/// invisible otherwise: the lookup is best-effort and a miss is indistinguishable
/// from a track that simply has no lyrics.
async function testLyricProvider() {
  if (!config) return;
  if (config.media.lyric_provider === "off") {
    setSaveState("error", "当前来源是「关闭」，不会联网查询");
    window.setTimeout(() => setSaveState("idle", ""), 2600);
    return;
  }
  if (config.media.lyric_provider === "custom" && !config.media.online_api.trim()) {
    setSaveState("error", "请先填写自定义接口地址");
    window.setTimeout(() => setSaveState("idle", ""), 2600);
    return;
  }
  // Save first so the backend uses exactly what is on screen.
  await api.updateConfig(config);
  setSaveState("saving", "正在测试…");
  try {
    setSaveState("saved", await api.testLyricProvider());
  } catch (error) {
    setSaveState("error", String(error));
  }
  window.setTimeout(() => setSaveState("idle", ""), 5000);
}

async function openPath(which: "data" | "lyrics" | "logs") {
  const path = appInfo[PATH_KEYS[which]];
  if (!path) return;
  await api.openPath(path);
}

const PATH_KEYS = {
  data: "数据目录",
  lyrics: "歌词目录",
  logs: "日志目录",
} as const;

async function clearClips(includePinned: boolean) {
  await api.clipboardClear(includePinned);
  render();
}

async function exportTodos(format: "markdown" | "json") {
  const text = await api.todoExport(format);
  await api.setClipboardText(text);
  setSaveState("saved", format === "markdown" ? "Markdown 已复制" : "JSON 已复制");
  window.setTimeout(() => setSaveState("idle", ""), 1600);
}

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

const ICON_PALETTE =
  '<svg viewBox="0 0 16 16"><path d="M8 1.6a6.4 6.4 0 0 0 0 12.8c1 0 1.6-.7 1.6-1.5 0-.4-.2-.8-.5-1.1-.2-.3-.4-.6-.4-1 0-.7.6-1.3 1.3-1.3h1.3A3.9 3.9 0 0 0 15 5.6C15 3.4 11.9 1.6 8 1.6zM4.7 8.6a1.2 1.2 0 1 1 0-2.4 1.2 1.2 0 0 1 0 2.4zm1.7-3.4a1.2 1.2 0 1 1 0-2.4 1.2 1.2 0 0 1 0 2.4zm3.4 0a1.2 1.2 0 1 1 0-2.4 1.2 1.2 0 0 1 0 2.4z"/></svg>';
const ICON_TASKBAR =
  '<svg viewBox="0 0 16 16"><path d="M1.5 2.5h13v11h-13zm1.2 8.4v1.4h10.6v-1.4z" opacity=".95"/><path d="M3 11.4h2.4v1.2H3zM6 11.4h2.4v1.2H6z"/></svg>';
const ICON_WIDGET =
  '<svg viewBox="0 0 16 16"><path d="M1.6 3.4h3.2v9.2H1.6zM6.2 3.4h8.2v4.2H6.2zM6.2 8.9h8.2v3.7H6.2z" opacity=".92"/></svg>';
const ICON_MUSIC =
  '<svg viewBox="0 0 16 16"><path d="M12.6 1.8v8.9a2.3 2.3 0 1 1-1.2-2v-4L6.4 5.6v6.6a2.3 2.3 0 1 1-1.2-2V4.2z"/></svg>';
const ICON_CLIPBOARD =
  '<svg viewBox="0 0 16 16"><path d="M6 1.4h4a1 1 0 0 1 1 1v.6h1.2a1 1 0 0 1 1 1v9.6a1 1 0 0 1-1 1H3.8a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1H5v-.6a1 1 0 0 1 1-1zm.6 1.7v.8h2.8v-.8z"/></svg>';
const ICON_CHECK =
  '<svg viewBox="0 0 16 16"><path d="M6.3 11.4 3 8.1l1.2-1.2 2.1 2.1 5.5-5.5L13 4.7z"/></svg>';
const ICON_SYSTEM =
  '<svg viewBox="0 0 16 16"><path d="M8 5.1A2.9 2.9 0 1 0 8 10.9 2.9 2.9 0 0 0 8 5.1zm0 4.4a1.5 1.5 0 1 1 0-3 1.5 1.5 0 0 1 0 3z"/><path d="m13.5 9.1.9.7-1 1.7-1.1-.4a4.9 4.9 0 0 1-1 .6l-.2 1.2H8.9l-.2-1.2a4.9 4.9 0 0 1-1-.6l-1.1.4-1-1.7.9-.7a4.7 4.7 0 0 1 0-1.2l-.9-.7 1-1.7 1.1.4c.3-.2.6-.4 1-.5l.2-1.2h2.1l.2 1.2c.4.1.7.3 1 .5l1.1-.4 1 1.7-.9.7c.1.4.1.8 0 1.2z" opacity=".5"/></svg>';
const ICON_INFO =
  '<svg viewBox="0 0 16 16"><path d="M8 1.4A6.6 6.6 0 1 0 8 14.6 6.6 6.6 0 0 0 8 1.4zm.9 10.2H7.1V7h1.8zM8 5.7a1 1 0 1 1 0-2 1 1 0 0 1 0 2z"/></svg>';

// ---------------------------------------------------------------------------
// Bootstrap
// ---------------------------------------------------------------------------

async function init() {
  const [loaded, info, status] = await Promise.all([
    api.getConfig(),
    api.getAppInfo().catch(() => ({}) as Record<string, string>),
    api.getModuleStatus().catch(() => moduleStatus),
  ]);
  config = loaded;
  appInfo = info;
  moduleStatus = status;
  applyLiveStyles();
  render();

  on("config-changed", (next) => {
    config = next;
    applyLiveStyles();
    render();
  });
  // Status pills read live module state, so refresh when a module reports in.
  on("taskbar-changed", () => {
    if (activeId === "taskbar") render();
  });

  el.minimize.addEventListener("click", () => void api.minimizeSettings());
  el.close.addEventListener("click", () => void api.closeSettings());
}

init().catch((error) => {
  console.error("settings failed to start", error);
});
