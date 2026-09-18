//! The settings page, as data.
//!
//! Every control the settings window draws comes from the tables in this file.
//! That is deliberate: the page has around sixty of them, and hand-writing a
//! paint routine, a hit test and a write-back branch per control is how a port
//! like this rots. Here the *only* per-field code is a row in a table, and the
//! drawing, layout and interaction code is shared.
//!
//! It also makes the page testable without a window: [`crate::access`] can be
//! driven over every path in these tables, which is what
//! `every_field_round_trips` does — a typo in a path fails the test suite rather
//! than silently doing nothing when the user drags a slider.

use beautify_core::config::{Config, LyricProvider, TaskbarMode, Theme, WidgetAnchor};

/// How a numeric value is rendered next to its slider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `0.78` → `78%`
    Percent,
    /// `150` → `150 ms`
    Milliseconds,
    /// `-250` → `-250 ms`, with an explicit sign so "earlier" reads as positive.
    SignedMilliseconds,
    /// `2.5` → `2.5×`
    Multiplier,
    /// `0.65` → `0.65`
    Decimal,
    /// `8` → `8 px`, and `0` → `直角`
    PixelsOrSquare,
    /// `120` → `120 px`
    Pixels,
}

impl Format {
    /// Render `value` the way the row shows it.
    pub fn render(self, value: f64) -> String {
        match self {
            Format::Percent => format!("{}%", (value * 100.0).round() as i64),
            Format::Milliseconds => format!("{} ms", value.round() as i64),
            Format::SignedMilliseconds => {
                let ms = value.round() as i64;
                if ms > 0 {
                    format!("+{ms} ms")
                } else {
                    format!("{ms} ms")
                }
            }
            Format::Multiplier => format!("{value:.1}×"),
            Format::Decimal => format!("{value:.2}"),
            Format::PixelsOrSquare => {
                if value.round() as i64 == 0 {
                    "直角".to_string()
                } else {
                    format!("{} px", value.round() as i64)
                }
            }
            Format::Pixels => format!("{} px", value.round() as i64),
        }
    }
}

/// A numeric slider.
#[derive(Debug, Clone, Copy)]
pub struct Slider {
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub format: Format,
}

/// One entry of a dropdown.
#[derive(Debug, Clone, Copy)]
pub struct Choice {
    /// The stored value.
    pub value: &'static str,
    /// What the row displays.
    pub label: &'static str,
}

/// Something a button does. The window turns these into host calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionId {
    TestLyricProvider,
    OpenLyricsDir,
    OpenDataDir,
    OpenLogsDir,
    ClearClipboardUnpinned,
    ClearClipboardAll,
    ExportTodosMarkdown,
    ExportTodosJson,
    /// Start a region capture.
    StartSnip,
    /// Dismiss every pinned image.
    CloseAllPins,
    Quit,
}

/// A live value the 关于 page reports but cannot change.
///
/// Only the *key* is static; the value comes from the host at paint time, which
/// is what keeps the about page inside the same tables-plus-live-values scheme
/// as every other read-only row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoKey {
    Version,
    Renderer,
    Build,
    ConfigPath,
    DataDir,
    LogsDir,
    /// What the last action row did, so a button that reports a result has
    /// somewhere to report it.
    LastAction,
}

/// A button under an action row.
#[derive(Debug, Clone, Copy)]
pub struct Button {
    pub label: &'static str,
    pub action: ActionId,
    /// Styled as destructive.
    pub danger: bool,
}

/// Which live value a status row shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusKind {
    Taskbar,
    Spectrum,
    Clipboard,
}

/// The control a field renders as.
#[derive(Debug, Clone, Copy)]
pub enum Kind {
    /// An on/off pill.
    Switch,
    Slider(Slider),
    /// A dropdown. The first choice is the default when nothing matches.
    Select(&'static [Choice]),
    /// `#RRGGBB`, with a colour picker and a hex box.
    Color,
    /// A text box whose contents are a number.
    Number {
        min: i64,
        max: i64,
        suffix: &'static str,
    },
    Text {
        placeholder: &'static str,
    },
    /// A key combination, recorded by pressing it rather than typed. Typing was
    /// the wrong affordance: the string is not the thing the user has in mind,
    /// the key press is, and a typo silently registers nothing.
    Hotkey,
    /// A live read-out; not editable.
    Status(StatusKind),
    /// A read-only key/value line whose value the host supplies. The label comes
    /// from the table; the value does not exist until something is running.
    Info(InfoKey),
    Action(&'static [Button]),
}

impl Kind {
    /// Does this control bind to a config value?
    ///
    /// Status and info rows only report, and action rows only run something, so
    /// none of them has a path — which is what the schema tests rely on.
    pub fn holds_value(&self) -> bool {
        !matches!(
            self,
            Kind::Status(_) | Kind::Info(_) | Kind::Action(_)
        )
    }

    /// Is this control something the pointer can act on?
    ///
    /// A read-only row still occupies a rectangle, but there is nothing to click
    /// and nothing to type, so the hit tester must not offer a hover state for
    /// it.
    pub fn is_interactive(&self) -> bool {
        !matches!(self, Kind::Info(_))
    }
}

/// One row.
#[derive(Debug, Clone, Copy)]
pub struct Field {
    /// The config path, e.g. `widget.opacity`. Empty for status and action rows.
    pub path: &'static str,
    pub label: &'static str,
    pub hint: Option<&'static str>,
    pub kind: Kind,
    /// Extra visibility rule beyond "the module is on".
    pub when: Option<fn(&Config) -> bool>,
}

/// A group of rows under an optional heading.
#[derive(Debug, Clone, Copy)]
pub struct Card {
    pub title: Option<&'static str>,
    pub fields: &'static [Field],
}

/// One page of the sidebar.
#[derive(Debug, Clone, Copy)]
pub struct Section {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub cards: &'static [Card],
    /// The about page is not a form and is rendered separately.
    pub is_about: bool,
}

// ---------------------------------------------------------------------------
// Shared option lists
// ---------------------------------------------------------------------------

/// The taskbar backdrop modes. Reused by `taskbar.mode` and the dynamic
/// override, which is why it is one table rather than two.
pub const MODES: &[Choice] = &[
    Choice {
        value: "normal",
        label: "跟随系统（不做修改）",
    },
    Choice {
        value: "clear",
        label: "全透明",
    },
    Choice {
        value: "blur",
        label: "模糊",
    },
    Choice {
        value: "acrylic",
        label: "亚克力 Acrylic",
    },
    Choice {
        value: "mica",
        label: "云母 Mica（Win11 22H2+）",
    },
    Choice {
        value: "opaque",
        label: "纯色不透明",
    },
];

const THEMES: &[Choice] = &[
    Choice {
        value: "dark",
        label: "深色",
    },
    Choice {
        value: "light",
        label: "浅色",
    },
    Choice {
        value: "auto",
        label: "跟随系统",
    },
];

const ANCHORS: &[Choice] = &[
    Choice {
        value: "taskbar-right",
        label: "任务栏 · 通知区域左侧",
    },
    Choice {
        value: "taskbar-center",
        label: "任务栏 · 居中",
    },
    Choice {
        value: "taskbar-left",
        label: "任务栏 · 左侧",
    },
    Choice {
        value: "bottom-right",
        label: "屏幕 · 右下",
    },
    Choice {
        value: "bottom-center",
        label: "屏幕 · 底部居中",
    },
    Choice {
        value: "bottom-left",
        label: "屏幕 · 左下",
    },
];

const LYRIC_PROVIDERS: &[Choice] = &[
    Choice {
        value: "off",
        label: "关闭（只读本地 .lrc）",
    },
    Choice {
        value: "netease",
        label: "网易云音乐",
    },
    Choice {
        value: "qq",
        label: "QQ 音乐",
    },
    Choice {
        value: "kugou",
        label: "酷狗音乐",
    },
    Choice {
        value: "lrclib",
        label: "LRCLIB（国际曲库）",
    },
    Choice {
        value: "custom",
        label: "自定义接口",
    },
];

const LOG_LEVELS: &[Choice] = &[
    Choice {
        value: "error",
        label: "仅错误",
    },
    Choice {
        value: "warn",
        label: "警告及以上",
    },
    Choice {
        value: "info",
        label: "常规",
    },
    Choice {
        value: "debug",
        label: "调试",
    },
];

// ---------------------------------------------------------------------------
// Visibility rules
// ---------------------------------------------------------------------------

fn taskbar_on(c: &Config) -> bool {
    c.taskbar.enabled
}
fn taskbar_tinted(c: &Config) -> bool {
    c.taskbar.enabled && c.taskbar.mode != TaskbarMode::Normal
}
fn taskbar_dynamic(c: &Config) -> bool {
    c.taskbar.enabled && c.taskbar.dynamic_mode
}
fn widget_on(c: &Config) -> bool {
    c.widget.enabled
}
fn widget_lyrics(c: &Config) -> bool {
    c.widget.enabled && c.media.show_lyrics
}
fn media_on(c: &Config) -> bool {
    c.media.enabled
}
fn media_spectrum(c: &Config) -> bool {
    c.media.enabled && c.media.show_spectrum
}
fn media_lyrics(c: &Config) -> bool {
    c.media.enabled && c.media.show_lyrics
}
fn media_custom(c: &Config) -> bool {
    c.media.enabled && c.media.lyric_provider == LyricProvider::Custom
}
fn clipboard_on(c: &Config) -> bool {
    c.clipboard.enabled
}
fn clipboard_images(c: &Config) -> bool {
    c.clipboard.enabled && c.clipboard.capture_images
}
fn todo_on(c: &Config) -> bool {
    c.todo.enabled
}
fn snip_on(c: &Config) -> bool {
    c.snip.enabled
}
fn file_logging(c: &Config) -> bool {
    c.ui.file_logging
}

/// Shorthand for a switch row.
const fn switch(path: &'static str, label: &'static str) -> Field {
    Field {
        path,
        label,
        hint: None,
        kind: Kind::Switch,
        when: None,
    }
}

/// Shorthand for a switch row with a hint.
const fn switch_hint(path: &'static str, label: &'static str, hint: &'static str) -> Field {
    Field {
        path,
        label,
        hint: Some(hint),
        kind: Kind::Switch,
        when: None,
    }
}

/// Shorthand for a number row.
const fn number(
    path: &'static str,
    label: &'static str,
    min: i64,
    max: i64,
    suffix: &'static str,
) -> Field {
    Field {
        path,
        label,
        hint: None,
        kind: Kind::Number { min, max, suffix },
        when: None,
    }
}

/// Shorthand for a slider row.
const fn slider(
    path: &'static str,
    label: &'static str,
    hint: Option<&'static str>,
    min: f64,
    max: f64,
    step: f64,
    format: Format,
) -> Field {
    Field {
        path,
        label,
        hint,
        kind: Kind::Slider(Slider {
            min,
            max,
            step,
            format,
        }),
        when: None,
    }
}

/// Shorthand for a dropdown row.
const fn select(path: &'static str, label: &'static str, options: &'static [Choice]) -> Field {
    Field {
        path,
        label,
        hint: None,
        kind: Kind::Select(options),
        when: None,
    }
}

/// Shorthand for a colour row.
const fn color(path: &'static str, label: &'static str) -> Field {
    Field {
        path,
        label,
        hint: None,
        kind: Kind::Color,
        when: None,
    }
}

/// Shorthand for a hotkey row.
const fn hotkey(path: &'static str, label: &'static str, hint: &'static str) -> Field {
    Field {
        path,
        label,
        hint: Some(hint),
        kind: Kind::Hotkey,
        when: None,
    }
}

/// Shorthand for a read-only key/value row.
const fn info(key: InfoKey, label: &'static str) -> Field {
    Field {
        path: "",
        label,
        hint: None,
        kind: Kind::Info(key),
        when: None,
    }
}

/// Attach a visibility rule.
const fn when(mut field: Field, rule: fn(&Config) -> bool) -> Field {
    field.when = Some(rule);
    field
}

/// Attach a hint.
const fn hint(mut field: Field, text: &'static str) -> Field {
    field.hint = Some(text);
    field
}

// ---------------------------------------------------------------------------
// The page
// ---------------------------------------------------------------------------

const APPEARANCE: &[Card] = &[
    Card {
        title: None,
        fields: &[
            select("ui.theme", "主题", THEMES),
            hint(
                color("ui.accent", "强调色"),
                "用于选中项、开关与焦点框。",
            ),
        ],
    },
    Card {
        title: Some("小组件"),
        fields: &[
            color("widget.background", "背景色"),
            slider("widget.opacity", "背景不透明度", None, 0.0, 1.0, 0.01, Format::Percent),
            hint(
                slider(
                    "widget.corner_radius",
                    "圆角",
                    Some("窗口圆角由 Windows 绘制，最大 8px。"),
                    0.0,
                    8.0,
                    1.0,
                    Format::PixelsOrSquare,
                ),
                "窗口圆角由 Windows 绘制，最大 8px。",
            ),
            slider(
                "widget.animation_ms",
                "宽度过渡时长",
                Some("组件宽度会跟随歌词长度变化，这里控制变化的动画时长，建议 120–180ms。"),
                60.0,
                400.0,
                10.0,
                Format::Milliseconds,
            ),
        ],
    },
];

const TASKBAR: &[Card] = &[
    Card {
        title: None,
        fields: &[
            switch("taskbar.enabled", "启用任务栏美化"),
            Field {
                path: "",
                label: "当前状态",
                hint: Some("实时反映任务栏模块正在做什么。"),
                kind: Kind::Status(StatusKind::Taskbar),
                when: None,
            },
        ],
    },
    Card {
        title: Some("效果"),
        fields: &[
            when(select("taskbar.mode", "背景模式", MODES), taskbar_on),
            when(color("taskbar.color", "着色"), taskbar_tinted),
            when(
                slider("taskbar.opacity", "着色不透明度", None, 0.0, 1.0, 0.01, Format::Percent),
                taskbar_tinted,
            ),
            when(
                switch("taskbar.apply_to_secondary", "应用到副屏任务栏"),
                taskbar_on,
            ),
        ],
    },
    Card {
        title: Some("行为"),
        fields: &[
            when(
                switch_hint(
                    "taskbar.dynamic_mode",
                    "动态模式",
                    "当显示器上有窗口最大化时切换为指定效果，窗口还原后恢复。",
                ),
                taskbar_on,
            ),
            when(
                select("taskbar.dynamic_mode_override", "最大化时使用", MODES),
                taskbar_dynamic,
            ),
            when(
                switch_hint(
                    "taskbar.hide_on_fullscreen",
                    "全屏应用时还原为系统默认",
                    "避免在游戏或全屏视频上叠加异常的背景。",
                ),
                taskbar_on,
            ),
            when(
                switch_hint(
                    "taskbar.restore_on_exit",
                    "退出时还原任务栏",
                    "强烈建议保持开启；关闭后退出程序会保留当前效果直到注销。",
                ),
                taskbar_on,
            ),
        ],
    },
];

const WIDGET: &[Card] = &[
    Card {
        title: None,
        fields: &[
            switch("widget.enabled", "显示小组件栏"),
            when(
                switch_hint(
                    "widget.hide_with_autohide",
                    "任务栏自动隐藏时一并隐藏",
                    "任务栏收起时小组件栏也一起消失。",
                ),
                widget_on,
            ),
        ],
    },
    Card {
        title: Some("位置"),
        fields: &[
            when(
                hint(
                    select("widget.anchor", "锚点", ANCHORS),
                    "任务栏内锚点会贴住开始按钮或通知区域；屏幕锚点则使用工作区底部。",
                ),
                widget_on,
            ),
            when(number("widget.offset_x", "水平偏移", -400, 400, "px"), widget_on),
            when(number("widget.offset_y", "垂直偏移", -60, 60, "px"), widget_on),
            when(
                hint(
                    number("widget.margin", "边距", 0, 40, "px"),
                    "小组件与任务栏上下边缘之间留出的空隙。",
                ),
                widget_on,
            ),
        ],
    },
    Card {
        title: Some("音频组件宽度"),
        fields: &[
            when(number("widget.audio_min_width", "最小宽度", 96, 900, "px"), widget_on),
            when(number("widget.audio_max_width", "最大宽度", 96, 1600, "px"), widget_on),
            when(number("widget.lyric_min_width", "歌词最小宽度", 0, 1200, "px"), widget_lyrics),
            when(number("widget.lyric_max_width", "歌词最大宽度", 0, 1600, "px"), widget_lyrics),
        ],
    },
    Card {
        title: Some("Flyout"),
        fields: &[
            when(number("widget.flyout_width", "宽度", 260, 900, "px"), widget_on),
            when(number("widget.flyout_height", "高度", 240, 1200, "px"), widget_on),
            when(switch("widget.flyout_flip", "空间不足时向上展开"), widget_on),
            when(switch("widget.remember_tab", "记住上次打开的标签页"), widget_on),
        ],
    },
];

const MEDIA: &[Card] = &[
    Card {
        title: None,
        fields: &[
            switch("media.enabled", "启用媒体模块"),
            Field {
                path: "",
                label: "频谱采集",
                hint: Some("频谱来自默认播放设备的回环捕获，不需要任何虚拟声卡。"),
                kind: Kind::Status(StatusKind::Spectrum),
                when: None,
            },
        ],
    },
    Card {
        title: Some("显示"),
        fields: &[
            when(switch("media.show_lyrics", "显示歌词"), media_on),
            when(switch("media.show_spectrum", "显示频谱"), media_on),
            when(
                slider(
                    "media.spectrum_sensitivity",
                    "灵敏度",
                    None,
                    0.2,
                    4.0,
                    0.1,
                    Format::Multiplier,
                ),
                media_spectrum,
            ),
            when(
                slider(
                    "media.spectrum_smoothing",
                    "回落平滑",
                    Some("数值越大，频谱柱回落越慢。"),
                    0.0,
                    0.95,
                    0.05,
                    Format::Decimal,
                ),
                media_spectrum,
            ),
        ],
    },
    Card {
        title: Some("歌词来源"),
        fields: &[
            when(
                hint(
                    select("media.lyric_provider", "歌词来源", LYRIC_PROVIDERS),
                    "除「关闭」外都会把当前歌曲的名称与歌手发送给对应平台以查询歌词，找不到时会自动尝试其它平台。查询结果只存在内存里，不会写入磁盘；介意联网就选「关闭」。",
                ),
                media_on,
            ),
            when(
                Field {
                    path: "media.online_api",
                    label: "自定义接口地址",
                    hint: Some(
                        "支持 {title} / {artist} / {album} 占位符；返回 LRC，或带 lyric/lrc 字段的 JSON。",
                    ),
                    kind: Kind::Text {
                        placeholder: "https://example.com/lrc?title={title}&artist={artist}",
                    },
                    when: None,
                },
                media_custom,
            ),
            when(
                Field {
                    path: "",
                    label: "歌词排错",
                    hint: Some(
                        "歌词目录里的 .lrc 文件名需为「歌手 - 歌名.lrc」，优先于在线结果。点「测试当前来源」会用正在播放的歌实际请求一次并告诉你结果。",
                    ),
                    kind: Kind::Action(&[
                        Button {
                            label: "测试当前来源",
                            action: ActionId::TestLyricProvider,
                            danger: false,
                        },
                        Button {
                            label: "打开歌词目录",
                            action: ActionId::OpenLyricsDir,
                            danger: false,
                        },
                    ]),
                    when: None,
                },
                media_on,
            ),
            when(
                slider(
                    "media.lyric_offset_ms",
                    "歌词偏移",
                    Some("正值让歌词提前显示；用于补偿不同平台的进度上报延迟。"),
                    -5000.0,
                    5000.0,
                    50.0,
                    Format::SignedMilliseconds,
                ),
                media_lyrics,
            ),
            when(
                slider(
                    "media.poll_interval_ms",
                    "兜底刷新间隔",
                    Some("播放状态由系统事件推送，这里只是防止遗漏的兜底轮询。"),
                    500.0,
                    10000.0,
                    250.0,
                    Format::Milliseconds,
                ),
                media_on,
            ),
        ],
    },
    Card {
        title: Some("预览"),
        fields: &[
            when(
                switch_hint(
                    "media.demo_mode",
                    "预览模式",
                    "显示一段演示曲目、歌词与频谱，用来在没有播放任何内容时调整外观。",
                ),
                media_on,
            ),
            Field {
                path: "",
                label: "歌词文件",
                hint: Some("歌词目录中的 .lrc 文件会被优先使用；在线获取的歌词不会写入这里。"),
                kind: Kind::Action(&[
                    Button {
                        label: "打开歌词目录",
                        action: ActionId::OpenLyricsDir,
                        danger: false,
                    },
                    Button {
                        label: "打开数据目录",
                        action: ActionId::OpenDataDir,
                        danger: false,
                    },
                ]),
                when: None,
            },
        ],
    },
];

const CLIPBOARD: &[Card] = &[
    Card {
        title: None,
        fields: &[
            switch("clipboard.enabled", "启用剪贴板历史"),
            Field {
                path: "",
                label: "占用",
                hint: None,
                kind: Kind::Status(StatusKind::Clipboard),
                when: None,
            },
        ],
    },
    Card {
        title: Some("记录规则"),
        fields: &[
            when(
                hint(
                    number("clipboard.max_entries", "最多保存条数", 10, 5000, "条"),
                    "收藏的条目不会被自动清除。",
                ),
                clipboard_on,
            ),
            when(switch("clipboard.capture_images", "保存图片"), clipboard_on),
            when(
                hint(
                    number("clipboard.max_image_bytes", "图片大小上限", 0, 67108864, "字节"),
                    "超过上限的图片会被忽略。0 表示不限制。",
                ),
                clipboard_images,
            ),
            when(
                switch_hint(
                    "clipboard.capture_sensitive",
                    "记录被标记为「不记录」的内容",
                    "密码管理器等程序会主动标记这类内容。默认忽略，仅在确实需要时开启。",
                ),
                clipboard_on,
            ),
        ],
    },
    Card {
        title: Some("数据"),
        fields: &[Field {
            path: "",
            label: "清理历史",
            hint: Some("收藏的条目默认保留。"),
            kind: Kind::Action(&[
                Button {
                    label: "清空未收藏",
                    action: ActionId::ClearClipboardUnpinned,
                    danger: true,
                },
                Button {
                    label: "全部清空",
                    action: ActionId::ClearClipboardAll,
                    danger: true,
                },
            ]),
            when: None,
        }],
    },
];

const TODO: &[Card] = &[
    Card {
        title: None,
        fields: &[
            switch("todo.enabled", "启用任务清单"),
            when(switch("todo.show_badge", "在启动器上显示未完成数量"), todo_on),
            when(
                switch_hint(
                    "todo.carry_over",
                    "启动时把未完成任务顺延到今天",
                    "未完成的旧任务会出现在今天的列表里。",
                ),
                todo_on,
            ),
        ],
    },
    Card {
        title: Some("导出"),
        fields: &[Field {
            path: "",
            label: "导出任务",
            hint: Some("复制到剪贴板，方便粘贴进笔记或提交信息。"),
            kind: Kind::Action(&[
                Button {
                    label: "复制 Markdown",
                    action: ActionId::ExportTodosMarkdown,
                    danger: false,
                },
                Button {
                    label: "复制 JSON",
                    action: ActionId::ExportTodosJson,
                    danger: false,
                },
            ]),
            when: None,
        }],
    },
];

const SYSTEM: &[Card] = &[
    Card {
        title: None,
        fields: &[
            switch_hint(
                "general.autostart",
                "开机自动启动",
                "写入当前用户的启动项，不需要管理员权限。",
            ),
            hotkey(
                "clipboard.hotkey",
                "剪贴板快捷键",
                "打开剪贴板历史。点一下再按组合键即可记录，按 Backspace 清空则不注册。",
            ),
            hotkey(
                "clipboard.pin_hotkey",
                "贴图快捷键",
                "把剪贴板里的图片贴到屏幕最上层（与 Snipaste 的 F3 一致）。再按一次收起。",
            ),
            hotkey("todo.hotkey", "任务清单快捷键", "打开任务清单。留空则不注册。"),
        ],
    },
    Card {
        title: Some("日志"),
        fields: &[
            switch("ui.file_logging", "写入日志文件"),
            when(
                hint(
                    select("ui.log_level", "日志级别", LOG_LEVELS),
                    "修改后需要重启 WinBeautify 才会生效。",
                ),
                file_logging,
            ),
            Field {
                path: "",
                label: "打开位置",
                hint: None,
                kind: Kind::Action(&[
                    Button {
                        label: "打开数据目录",
                        action: ActionId::OpenDataDir,
                        danger: false,
                    },
                    Button {
                        label: "打开日志目录",
                        action: ActionId::OpenLogsDir,
                        danger: false,
                    },
                ]),
                when: None,
            },
        ],
    },
    Card {
        title: Some("运行"),
        fields: &[Field {
            path: "",
            label: "WinBeautify 在后台托盘运行",
            hint: Some("小组件栏与全部模块都运行在这个进程里；关闭设置窗口不会退出程序。"),
            kind: Kind::Action(&[Button {
                label: "退出 WinBeautify",
                action: ActionId::Quit,
                danger: true,
            }]),
            when: None,
        }],
    },
];

const SNIP: &[Card] = &[
    Card {
        title: None,
        fields: &[
            switch("snip.enabled", "启用截图"),
            when(
                hotkey(
                    "snip.hotkey",
                    "截图快捷键",
                    "点一下再按组合键即可记录。按下后在整块桌面上拖出要截取的区域，Esc 或右键取消；                     按 Backspace 清空则不注册。",
                ),
                snip_on,
            ),
            when(
                Field {
                    path: "",
                    label: "立即截图",
                    hint: Some("等同于按下上面的快捷键。"),
                    kind: Kind::Action(&[Button {
                        label: "开始截图",
                        action: ActionId::StartSnip,
                        danger: false,
                    }]),
                    when: None,
                },
                snip_on,
            ),
        ],
    },
    Card {
        title: Some("结果"),
        fields: &[
            when(
                switch_hint(
                    "snip.copy_to_clipboard",
                    "复制到剪贴板",
                    "以标准 CF_DIB 写入，可直接粘贴到聊天窗口或画图。",
                ),
                snip_on,
            ),
            when(
                switch_hint(
                    "snip.auto_pin",
                    "同时贴到屏幕上",
                    "在截取的原位置生成一张贴图：拖动移动，滚轮缩放，方向键微调，Esc 或双击关闭。",
                ),
                snip_on,
            ),
            when(
                hint(
                    slider("snip.dim", "选区外遮罩", None, 0.0, 0.85, 0.05, Format::Percent),
                    "遮罩越深，选区越突出；过深会看不清背景。",
                ),
                snip_on,
            ),
        ],
    },
    Card {
        title: Some("贴图"),
        fields: &[Field {
            path: "",
            label: "屏幕上可能有之前留下的贴图",
            hint: Some("贴图是独立窗口，会一直留在桌面上直到手动关闭。"),
            kind: Kind::Action(&[Button {
                label: "关闭全部贴图",
                action: ActionId::CloseAllPins,
                danger: true,
            }]),
            when: None,
        }],
    },
];

const ABOUT: &[Card] = &[
    Card {
        title: Some("运行信息"),
        fields: &[
            info(InfoKey::Version, "版本"),
            info(InfoKey::Renderer, "小组件渲染"),
            info(InfoKey::Build, "系统版本"),
        ],
    },
    Card {
        title: Some("位置"),
        fields: &[
            hint(info(InfoKey::ConfigPath, "配置文件"), "可以直接用文本编辑器修改。"),
            info(InfoKey::DataDir, "数据目录"),
            info(InfoKey::LogsDir, "日志目录"),
            Field {
                path: "",
                label: "打开位置",
                hint: None,
                kind: Kind::Action(&[
                    Button {
                        label: "打开数据目录",
                        action: ActionId::OpenDataDir,
                        danger: false,
                    },
                    Button {
                        label: "打开日志目录",
                        action: ActionId::OpenLogsDir,
                        danger: false,
                    },
                ]),
                when: None,
            },
        ],
    },
    Card {
        title: Some("最近操作"),
        fields: &[hint(
            info(InfoKey::LastAction, "结果"),
            "任务栏效果通过 DWM 与合成 API 实现，不修改任何系统文件，退出时自动还原；全部模块按需加载，空闲时不轮询。",
        )],
    },
];

/// Every section, in sidebar order.
pub const SECTIONS: &[Section] = &[
    Section {
        id: "appearance",
        title: "外观",
        description: "主题、强调色，以及小组件外观。这些设置只影响 WinBeautify 自己的窗口。",
        cards: APPEARANCE,
        is_about: false,
    },
    Section {
        id: "taskbar",
        title: "任务栏",
        description: "通过 DWM 与合成 API 为任务栏叠加透明、模糊或材质背景，不修改任何系统文件。",
        cards: TASKBAR,
        is_about: false,
    },
    Section {
        id: "widget",
        title: "小组件栏",
        description: "嵌入任务栏的 Widget Bar：启动器、音频组件与 Flyout。",
        cards: WIDGET,
        is_about: false,
    },
    Section {
        id: "media",
        title: "媒体与歌词",
        description: "通过 Windows 媒体会话（GSMTC）读取正在播放的内容，并从系统音频回放中提取频谱。",
        cards: MEDIA,
        is_about: false,
    },
    Section {
        id: "clipboard",
        title: "剪贴板",
        description: "监听系统剪贴板变化并保存历史，支持文本、图片与文件列表。",
        cards: CLIPBOARD,
        is_about: false,
    },
    Section {
        id: "todo",
        title: "任务清单",
        description: "轻量的待办列表，可从小组件栏的 Flyout 直接查看与勾选。",
        cards: TODO,
        is_about: false,
    },
    Section {
        id: "snip",
        title: "截图",
        description: "拖动选择区域后把截图放进剪贴板，或贴回屏幕上继续对照。全程在本进程内完成，不写临时文件。",
        cards: SNIP,
        is_about: false,
    },
    Section {
        id: "system",
        title: "系统",
        description: "开机启动、快捷键与日志。",
        cards: SYSTEM,
        is_about: false,
    },
    Section {
        id: "about",
        title: "关于",
        description: "",
        cards: ABOUT,
        is_about: true,
    },
];

/// Look up a section by id.
pub fn section(id: &str) -> Option<&'static Section> {
    SECTIONS.iter().find(|s| s.id == id)
}

impl Section {
    /// Is this field visible for `config`?
    pub fn shows(&self, field: &Field, config: &Config) -> bool {
        field.when.map(|rule| rule(config)).unwrap_or(true)
    }

    /// The fields of this section that `config` currently shows.
    pub fn visible_fields<'a>(
        &'a self,
        config: &'a Config,
    ) -> impl Iterator<Item = (&'a Card, &'a Field)> + 'a {
        self.cards.iter().flat_map(move |card| {
            card.fields
                .iter()
                .filter(move |field| self.shows(field, config))
                .map(move |field| (card, field))
        })
    }
}

/// Parse a `TaskbarMode` from its stored id.
pub fn mode_from_id(id: &str) -> TaskbarMode {
    match id {
        "opaque" => TaskbarMode::Opaque,
        "clear" => TaskbarMode::Clear,
        "blur" => TaskbarMode::Blur,
        "acrylic" => TaskbarMode::Acrylic,
        "mica" => TaskbarMode::Mica,
        _ => TaskbarMode::Normal,
    }
}

/// Parse a `Theme` from its stored id.
pub fn theme_from_id(id: &str) -> Theme {
    match id {
        "light" => Theme::Light,
        "auto" => Theme::Auto,
        _ => Theme::Dark,
    }
}

/// Parse a `WidgetAnchor` from its stored id.
pub fn anchor_from_id(id: &str) -> WidgetAnchor {
    match id {
        "taskbar-center" => WidgetAnchor::TaskbarCenter,
        "taskbar-right" => WidgetAnchor::TaskbarRight,
        "bottom-left" => WidgetAnchor::BottomLeft,
        "bottom-center" => WidgetAnchor::BottomCenter,
        "bottom-right" => WidgetAnchor::BottomRight,
        _ => WidgetAnchor::TaskbarLeft,
    }
}

/// The label MODES shows for a stored mode id, for the status pill.
pub fn mode_label(id: &str) -> &'static str {
    MODES
        .iter()
        .find(|choice| choice.value == id)
        .map(|choice| choice.label)
        .unwrap_or("未知")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_editable_field_names_a_config_path() {
        for section in SECTIONS {
            for (_, field) in section.visible_fields(&Config::default()) {
                let needs_path = field.kind.holds_value();
                assert_eq!(
                    needs_path,
                    !field.path.is_empty(),
                    "section {} field {:?} has path {:?} but kind {:?}",
                    section.id,
                    field.label,
                    field.path,
                    std::mem::discriminant(&field.kind),
                );
            }
        }
    }

    #[test]
    fn paths_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for section in SECTIONS {
            for card in section.cards {
                for field in card.fields {
                    if field.path.is_empty() {
                        continue;
                    }
                    assert!(seen.insert(field.path), "duplicate path {}", field.path);
                }
            }
        }
        assert!(seen.len() > 45, "expected the full page, found {}", seen.len());
    }

    #[test]
    fn every_label_is_filled_in() {
        for section in SECTIONS {
            if section.is_about {
                continue;
            }
            assert!(!section.title.is_empty());
            for card in section.cards {
                assert!(!card.fields.is_empty(), "empty card in {}", section.id);
                for field in card.fields {
                    assert!(!field.label.is_empty(), "unlabelled field in {}", section.id);
                }
            }
        }
    }

    #[test]
    fn an_off_module_hides_its_dependent_rows() {
        let mut config = Config::default();
        config.taskbar.enabled = false;
        let section = section("taskbar").unwrap();
        let titles: Vec<&str> = section
            .visible_fields(&config)
            .map(|(_, field)| field.label)
            .collect();
        assert_eq!(titles, vec!["启用任务栏美化", "当前状态"]);

        config.taskbar.enabled = true;
        assert!(
            section.visible_fields(&config).count() > titles.len(),
            "switching the module on must reveal its settings"
        );
    }

    #[test]
    fn the_tint_rows_follow_the_mode() {
        let mut config = Config::default();
        config.taskbar.mode = TaskbarMode::Normal;
        let section = section("taskbar").unwrap();
        let shows_tint = |c: &Config| {
            section
                .visible_fields(c)
                .any(|(_, field)| field.path == "taskbar.color")
        };
        assert!(!shows_tint(&config), "no tint to configure in normal mode");
        config.taskbar.mode = TaskbarMode::Acrylic;
        assert!(shows_tint(&config));
    }

    #[test]
    fn the_dynamic_override_only_shows_for_the_dynamic_mode() {
        let mut config = Config::default();
        config.taskbar.enabled = true;
        config.taskbar.dynamic_mode = false;
        let section = section("taskbar").unwrap();
        let shows = |c: &Config| {
            section
                .visible_fields(c)
                .any(|(_, field)| field.path == "taskbar.dynamic_mode_override")
        };
        assert!(!shows(&config));
        config.taskbar.dynamic_mode = true;
        assert!(shows(&config));
    }

    #[test]
    fn the_snip_section_collapses_to_its_switch() {
        let mut config = Config::default();
        config.snip.enabled = true;
        let section = section("snip").unwrap();
        let expanded: Vec<&str> = section.visible_fields(&config).map(|(_, f)| f.label).collect();
        assert!(expanded.len() >= 5, "the feature's rows should be visible");

        config.snip.enabled = false;
        let collapsed: Vec<&str> = section.visible_fields(&config).map(|(_, f)| f.label).collect();
        assert!(
            collapsed.contains(&"启用截图"),
            "the switch that turns it back on must never hide itself"
        );
        assert!(
            !collapsed.contains(&"立即截图"),
            "a trigger for a disabled feature is a dead control"
        );
        // The one row that stays is the pin housekeeping: pins left over from a
        // previous session still have to be dismissible.
        assert!(collapsed.contains(&"屏幕上可能有之前留下的贴图"));
    }

    #[test]
    fn the_about_page_has_something_to_show() {
        let about = section("about").unwrap();
        assert!(about.is_about, "the sidebar pins this entry to the bottom");
        assert!(
            !about.cards.is_empty(),
            "an empty 关于 page is a blank screen, not a page"
        );
        // Its rows are read-only, so none of them may claim a config path.
        for card in about.cards {
            for field in card.fields {
                assert!(
                    !field.kind.holds_value() || !field.path.is_empty(),
                    "关于 field {:?} binds a value but names no path",
                    field.label
                );
            }
        }
        assert!(
            about
                .cards
                .iter()
                .flat_map(|card| card.fields.iter())
                .any(|field| matches!(field.kind, Kind::Info(_))),
            "the about page is where the live values go"
        );
    }

    #[test]
    fn formats_render_the_way_the_old_page_did() {
        assert_eq!(Format::Percent.render(0.78), "78%");
        assert_eq!(Format::Percent.render(0.0), "0%");
        assert_eq!(Format::Milliseconds.render(150.0), "150 ms");
        assert_eq!(Format::SignedMilliseconds.render(250.0), "+250 ms");
        assert_eq!(Format::SignedMilliseconds.render(-250.0), "-250 ms");
        assert_eq!(Format::SignedMilliseconds.render(0.0), "0 ms");
        assert_eq!(Format::Multiplier.render(2.5), "2.5×");
        assert_eq!(Format::Decimal.render(0.65), "0.65");
        assert_eq!(Format::PixelsOrSquare.render(8.0), "8 px");
        assert_eq!(Format::PixelsOrSquare.render(0.0), "直角");
        assert_eq!(Format::Pixels.render(120.0), "120 px");
    }

    #[test]
    fn mode_ids_round_trip_through_the_label_table() {
        for choice in MODES {
            assert_eq!(mode_from_id(choice.value).id(), choice.value);
            assert_ne!(mode_label(choice.value), "未知");
        }
    }
}
