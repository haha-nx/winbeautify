//! The WinBeautify configuration schema and its on-disk lifecycle.
//!
//! Everything is `serde`-defaultable so a hand-written partial `config.toml`
//! still loads: unknown keys are ignored and missing keys fall back to the
//! defaults below. The UI treats this file as the single source of truth.

use crate::geometry::Color;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bumped whenever the schema changes in a way that needs migration.
pub const CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub general: GeneralConfig,
    pub taskbar: TaskbarConfig,
    pub media: MediaConfig,
    pub clipboard: ClipboardConfig,
    pub todo: TodoConfig,
    pub widget: WidgetConfig,
    pub snip: SnipConfig,
    pub ui: UiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            general: GeneralConfig::default(),
            taskbar: TaskbarConfig::default(),
            media: MediaConfig::default(),
            clipboard: ClipboardConfig::default(),
            todo: TodoConfig::default(),
            widget: WidgetConfig::default(),
            snip: SnipConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralConfig {
    /// Start WinBeautify when the user logs in.
    pub autostart: bool,
    /// Create the registry Run entry immediately instead of waiting for the
    /// user to flip the switch in Settings.
    pub start_minimized: bool,
    /// Ask the media module to keep the spectrum analyser running while the
    /// audio session is paused. Off saves a little CPU.
    pub keep_alive_on_pause: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            autostart: false,
            start_minimized: true,
            keep_alive_on_pause: false,
        }
    }
}

/// How the taskbar backdrop is composited.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskbarMode {
    /// Hand the taskbar back to Windows (no accent applied at all).
    Normal,
    /// Fully opaque fill of `color`.
    Opaque,
    /// Fully transparent, no blur — the desktop shows through untouched.
    Clear,
    /// Gaussian blur behind the taskbar, tinted by `color` at `opacity`.
    Blur,
    /// Win10-1803+/Win11 acrylic: blur plus a noise layer.
    Acrylic,
    /// Win11 22H2+ Mica / Mica Alt, falling back to acrylic when unsupported.
    ///
    /// The default: the whole point of the app is to change how the taskbar
    /// looks rather than leaving it alone.
    #[default]
    Mica,
}

impl TaskbarMode {
    pub const ALL: [TaskbarMode; 6] = [
        TaskbarMode::Normal,
        TaskbarMode::Opaque,
        TaskbarMode::Clear,
        TaskbarMode::Blur,
        TaskbarMode::Acrylic,
        TaskbarMode::Mica,
    ];

    /// Stable identifier used over the Tauri command boundary.
    pub const fn id(self) -> &'static str {
        match self {
            TaskbarMode::Normal => "normal",
            TaskbarMode::Opaque => "opaque",
            TaskbarMode::Clear => "clear",
            TaskbarMode::Blur => "blur",
            TaskbarMode::Acrylic => "acrylic",
            TaskbarMode::Mica => "mica",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskbarConfig {
    pub enabled: bool,
    pub mode: TaskbarMode,
    /// Tint colour behind the taskbar.
    pub color: Color,
    /// Tint alpha, 0.0 (invisible) .. 1.0 (opaque). Ignored by `Opaque`.
    pub opacity: f32,
    /// Second taskbars on non-primary monitors get the same treatment.
    pub apply_to_secondary: bool,
    /// TranslucentTB-style "dynamic windows": go clear while any window on the
    /// taskbar's monitor is maximised, restore otherwise.
    pub dynamic_mode: bool,
    /// Mode used while `dynamic_mode` is active.
    pub dynamic_mode_override: TaskbarMode,
    /// Hide the taskbar accent entirely when a fullscreen app is foreground.
    pub hide_on_fullscreen: bool,
    /// Re-apply the last accent when the process exits.
    pub restore_on_exit: bool,
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: TaskbarMode::Acrylic,
            color: Color::rgb(0x14, 0x16, 0x1c),
            opacity: 0.35,
            apply_to_secondary: true,
            dynamic_mode: false,
            dynamic_mode_override: TaskbarMode::Clear,
            hide_on_fullscreen: true,
            restore_on_exit: true,
        }
    }
}

/// Where lyrics may be fetched from.
///
/// Every online option sends the track name and artist to a third party, which
/// is why [`LyricProvider::Off`] exists and why the choice is explicit rather
/// than hidden behind an "auto" mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LyricProvider {
    /// Lyrics cache directory only; never touches the network.
    Off,
    #[default]
    Netease,
    Qq,
    Kugou,
    Lrclib,
    /// Use `online_api` as a URL template.
    Custom,
}

impl LyricProvider {
    pub const ALL: [LyricProvider; 6] = [
        LyricProvider::Off,
        LyricProvider::Netease,
        LyricProvider::Qq,
        LyricProvider::Kugou,
        LyricProvider::Lrclib,
        LyricProvider::Custom,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            LyricProvider::Off => "off",
            LyricProvider::Netease => "netease",
            LyricProvider::Qq => "qq",
            LyricProvider::Kugou => "kugou",
            LyricProvider::Lrclib => "lrclib",
            LyricProvider::Custom => "custom",
        }
    }

    /// Does this provider need the network?
    pub const fn is_online(self) -> bool {
        !matches!(self, LyricProvider::Off)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaConfig {
    pub enabled: bool,
    pub show_lyrics: bool,
    pub show_spectrum: bool,
    /// Linear gain applied before the log-frequency mapping.
    pub spectrum_sensitivity: f32,
    /// Per-frame smoothing, 0.0 (no smoothing) .. 0.95 (very slow).
    pub spectrum_smoothing: f32,
    pub lyric_provider: LyricProvider,
    /// URL template used when `lyric_provider` is `Custom`.
    /// `{title}` / `{artist}` / `{album}` are substituted.
    pub online_api: String,
    /// Positive shifts lyrics later, in milliseconds.
    pub lyric_offset_ms: i32,
    /// Safety-net refresh cadence for the GSMTC snapshot. Property changes are
    /// pushed through WinRT events; this timer only catches missed ones.
    pub poll_interval_ms: u64,
    /// Show a fabricated track, lyrics and spectrum so the widget's appearance
    /// can be previewed without anything playing. Purely cosmetic — the
    /// transport buttons do nothing and real sessions are ignored while it is
    /// on.
    pub demo_mode: bool,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            show_lyrics: true,
            show_spectrum: true,
            spectrum_sensitivity: 1.0,
            spectrum_smoothing: 0.6,
            lyric_provider: LyricProvider::default(),
            online_api: String::new(),
            lyric_offset_ms: 0,
            poll_interval_ms: 2000,
            demo_mode: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardConfig {
    pub enabled: bool,
    /// Oldest non-favourite entries are evicted past this count.
    pub max_entries: u32,
    pub capture_images: bool,
    /// Images above this size are not persisted (bytes of the DIB payload).
    pub max_image_bytes: u32,
    /// Global hotkey that opens the flyout on the Clipboard tab, e.g.
    /// `"Ctrl+Alt+V"`. Empty disables it.
    pub hotkey: String,
    /// Global hotkey that pins the clipboard's image to the desktop. Matches
    /// Snipaste's F3. Empty disables it.
    pub pin_hotkey: String,
    /// Include text copied by password managers etc. Off by default because
    /// Windows marks such clips with a "do not record" format.
    pub capture_sensitive: bool,
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 500,
            capture_images: true,
            max_image_bytes: 8 * 1024 * 1024,
            hotkey: "Ctrl+Alt+V".to_string(),
            pin_hotkey: "F3".to_string(),
            capture_sensitive: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TodoConfig {
    pub enabled: bool,
    /// Global hotkey that opens the flyout on the Todo tab. Empty disables it.
    pub hotkey: String,
    /// Roll unfinished tasks over to the next day at midnight.
    pub carry_over: bool,
    /// Show a badge with the open task count on the launcher.
    pub show_badge: bool,
}

impl Default for TodoConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            hotkey: String::new(),
            carry_over: true,
            show_badge: true,
        }
    }
}

/// One axis of an anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnchorAlign {
    Start,
    Center,
    End,
}

/// Which screen corner/anchor the widget bar grows from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WidgetAnchor {
    /// Left-aligned inside the taskbar, just right of the Start button.
    TaskbarLeft,
    /// Centred inside the taskbar.
    TaskbarCenter,
    /// Right-aligned inside the taskbar, just left of the tray.
    #[default]
    TaskbarRight,
    /// Bottom-left of the monitor's work area.
    BottomLeft,
    BottomCenter,
    BottomRight,
}

impl WidgetAnchor {
    pub const ALL: [WidgetAnchor; 6] = [
        WidgetAnchor::TaskbarLeft,
        WidgetAnchor::TaskbarCenter,
        WidgetAnchor::TaskbarRight,
        WidgetAnchor::BottomLeft,
        WidgetAnchor::BottomCenter,
        WidgetAnchor::BottomRight,
    ];

    pub const fn id(self) -> &'static str {
        match self {
            WidgetAnchor::TaskbarLeft => "taskbar-left",
            WidgetAnchor::TaskbarCenter => "taskbar-center",
            WidgetAnchor::TaskbarRight => "taskbar-right",
            WidgetAnchor::BottomLeft => "bottom-left",
            WidgetAnchor::BottomCenter => "bottom-center",
            WidgetAnchor::BottomRight => "bottom-right",
        }
    }

    /// Which edge of the bar stays put while its width animates.
    ///
    /// Returns `(horizontal, vertical)` alignment. A right-hand anchor pins the
    /// bar's right edge, which is the "grows leftwards" behaviour in the spec;
    /// a left-hand anchor pins the left edge, and a centre anchor grows both
    /// ways from the middle.
    pub const fn alignment(self) -> (AnchorAlign, AnchorAlign) {
        match self {
            WidgetAnchor::TaskbarLeft | WidgetAnchor::BottomLeft => {
                (AnchorAlign::Start, AnchorAlign::End)
            }
            WidgetAnchor::TaskbarCenter | WidgetAnchor::BottomCenter => {
                (AnchorAlign::Center, AnchorAlign::Center)
            }
            WidgetAnchor::TaskbarRight | WidgetAnchor::BottomRight => {
                (AnchorAlign::End, AnchorAlign::End)
            }
        }
    }

    /// Anchors that sit inside the taskbar rect rather than the work area.
    pub const fn is_taskbar(self) -> bool {
        matches!(
            self,
            WidgetAnchor::TaskbarLeft | WidgetAnchor::TaskbarCenter | WidgetAnchor::TaskbarRight
        )
    }
}

/// How the widget bar is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WidgetRenderer {
    /// Direct2D in the host process: no webview, so the bar costs a few
    /// megabytes instead of a few hundred. The default.
    #[default]
    Native,
    /// The original WebView2 bar. Kept as a fallback for machines where the
    /// native renderer misbehaves, and for previewing both side by side.
    Webview,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WidgetConfig {
    /// Master switch for the embedded widget bar window.
    pub enabled: bool,
    /// Which renderer draws the bar.
    pub renderer: WidgetRenderer,
    pub anchor: WidgetAnchor,
    /// Horizontal nudge from the resolved anchor edge, in physical pixels.
    pub offset_x: i32,
    /// Vertical nudge; 0 centres the bar inside the taskbar.
    pub offset_y: i32,
    /// Gap between the bar and the taskbar/tray neighbours.
    pub margin: i32,
    pub background: Color,
    pub opacity: f32,
    pub corner_radius: f32,
    /// Width transition duration for the adaptive audio component.
    pub animation_ms: u32,
    /// Hide the bar when the taskbar is auto-hidden and currently retracted.
    pub hide_with_autohide: bool,

    // --- adaptive audio component geometry (physical px) ---
    pub audio_min_width: i32,
    pub audio_max_width: i32,
    pub lyric_min_width: i32,
    pub lyric_max_width: i32,

    // --- flyout ---
    pub flyout_width: i32,
    pub flyout_height: i32,
    /// Flip the flyout above the bar when there is no room below.
    pub flyout_flip: bool,
    /// Remember the last visited flyout tab.
    pub remember_tab: bool,
}

impl Default for WidgetConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            renderer: WidgetRenderer::Native,
            anchor: WidgetAnchor::TaskbarRight,
            offset_x: -10,
            offset_y: 0,
            margin: 8,
            background: Color::rgb(0x14, 0x16, 0x1c),
            opacity: 0.78,
            corner_radius: 8.0,
            animation_ms: 150,
            hide_with_autohide: true,

            audio_min_width: 168,
            audio_max_width: 420,
            lyric_min_width: 96,
            lyric_max_width: 280,

            flyout_width: 380,
            flyout_height: 480,
            flyout_flip: true,
            remember_tab: true,
        }
    }
}

/// Screen capture and pin-to-desktop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnipConfig {
    pub enabled: bool,
    /// Global hotkey that starts a region capture, e.g. `"Ctrl+Alt+A"`.
    /// Empty disables the hotkey (the tray entry still works).
    pub hotkey: String,
    /// Put the finished capture on the clipboard as `CF_DIB`.
    pub copy_to_clipboard: bool,
    /// Pin the capture to the desktop as well, at the position it was taken.
    pub auto_pin: bool,
    /// How far the unselected area is darkened, 0.0 (untouched) .. 0.85.
    pub dim: f32,
}

impl Default for SnipConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            // Snipaste's default, and what the owner asked for.
            hotkey: "F1".to_string(),
            copy_to_clipboard: true,
            auto_pin: false,
            dim: 0.45,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    #[default]
    Dark,
    Light,
    /// Follow the Windows apps-light-theme setting.
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: Theme,
    /// Accent colour used for selections, toggles and focus rings.
    pub accent: Color,
    /// Write a rotating log file under `%LOCALAPPDATA%\WinBeautify\logs`.
    pub file_logging: bool,
    pub log_level: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: Theme::Dark,
            accent: Color::rgb(0x6C, 0x8C, 0xFF),
            file_logging: false,
            log_level: "info".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Loading / saving
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Serialize(toml::ser::Error),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config io error: {e}"),
            ConfigError::Parse(e) => write!(f, "config parse error: {e}"),
            ConfigError::Serialize(e) => write!(f, "config serialize error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(e: std::io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl Config {
    /// Read `path`, falling back to defaults when it does not exist.
    ///
    /// A corrupt file is *not* silently discarded: the caller gets the parse
    /// error so it can back the file up and start over, which beats overwriting
    /// a config the user spent time on.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(ConfigError::Io(e)),
        }
    }

    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let mut cfg: Config = toml::from_str(text).map_err(ConfigError::Parse)?;
        cfg.version = CONFIG_VERSION;
        cfg.clamp();
        Ok(cfg)
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(ConfigError::Serialize)
    }

    /// Write atomically: the temp file is renamed over the target so a crash
    /// mid-write can never leave a truncated config behind.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = self.to_toml()?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Force every value into the range the native layers can actually honour.
    /// Called after every load and every mutation so the rest of the program
    /// never has to re-validate.
    pub fn clamp(&mut self) {
        self.taskbar.opacity = self.taskbar.opacity.clamp(0.0, 1.0);
        self.widget.opacity = self.widget.opacity.clamp(0.0, 1.0);
        // Windows rounds window corners at a fixed 8 px (DWMWA_WINDOW_CORNER_PREFERENCE
        // takes no custom radius), so anything larger would leave a gap between the CSS
        // panel and the frame it is clipped to.
        self.widget.corner_radius = self.widget.corner_radius.clamp(0.0, 8.0);
        self.widget.audio_min_width = self.widget.audio_min_width.clamp(96, 900);
        self.widget.audio_max_width = self
            .widget
            .audio_max_width
            .clamp(self.widget.audio_min_width, 1600);
        self.widget.lyric_min_width = self.widget.lyric_min_width.clamp(0, 1200);
        self.widget.lyric_max_width = self
            .widget
            .lyric_max_width
            .clamp(self.widget.lyric_min_width, 1600);
        self.widget.flyout_width = self.widget.flyout_width.clamp(260, 900);
        self.widget.flyout_height = self.widget.flyout_height.clamp(240, 1200);
        self.widget.animation_ms = self.widget.animation_ms.min(1000);

        self.media.spectrum_sensitivity = self.media.spectrum_sensitivity.clamp(0.1, 8.0);
        self.media.spectrum_smoothing = self.media.spectrum_smoothing.clamp(0.0, 0.95);
        self.media.lyric_offset_ms = self.media.lyric_offset_ms.clamp(-10_000, 10_000);
        self.media.poll_interval_ms = self.media.poll_interval_ms.clamp(250, 30_000);

        self.clipboard.max_entries = self.clipboard.max_entries.clamp(10, 10_000);
        self.clipboard.max_image_bytes = self.clipboard.max_image_bytes.clamp(0, 64 * 1024 * 1024);

        // Above about 0.85 the selection is hard to see against the dimmed
        // background, which makes the tool feel broken rather than dark.
        self.snip.dim = self.snip.dim.clamp(0.0, 0.85);
    }

    /// True when at least one module that owns a widget-bar surface is on.
    pub fn any_widget_source(&self) -> bool {
        self.todo.enabled || self.clipboard.enabled || self.media.enabled
    }
}

/// Owns the current config plus the path it lives at.
///
/// Mutations go through [`ConfigManager::update`] so the in-memory copy and the
/// file on disk can never drift.
pub struct ConfigManager {
    path: PathBuf,
    inner: parking_lot::RwLock<Config>,
}

impl ConfigManager {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            inner: parking_lot::RwLock::new(Config::default()),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> Config {
        self.inner.read().clone()
    }

    /// Read a single field without cloning the whole config.
    pub fn read<R>(&self, f: impl FnOnce(&Config) -> R) -> R {
        f(&self.inner.read())
    }

    pub fn set(&self, cfg: Config) {
        let mut guard = self.inner.write();
        let mut cfg = cfg;
        cfg.clamp();
        *guard = cfg;
    }

    /// Mutate in place and persist. Returns the new snapshot.
    pub fn update(&self, f: impl FnOnce(&mut Config)) -> Result<Config, ConfigError> {
        let snapshot = {
            let mut guard = self.inner.write();
            let mut cfg = guard.clone();
            f(&mut cfg);
            cfg.clamp();
            *guard = cfg.clone();
            cfg
        };
        snapshot.save(&self.path)?;
        Ok(snapshot)
    }

    /// Load from disk, quarantining a corrupt file instead of losing it.
    pub fn load(&self) -> Result<Config, ConfigError> {
        match Config::load(&self.path) {
            Ok(cfg) => {
                self.set(cfg.clone());
                Ok(cfg)
            }
            Err(ConfigError::Parse(e)) => {
                let backup = self.path.with_extension("toml.broken");
                let _ = std::fs::rename(&self.path, &backup);
                tracing::warn!(
                    "config at {} was unreadable ({e}); moved to {} and restored defaults",
                    self.path.display(),
                    backup.display()
                );
                let cfg = Config::default();
                let _ = cfg.save(&self.path);
                self.set(cfg.clone());
                Ok(cfg)
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let cfg = Config::default();
        let text = cfg.to_toml().unwrap();
        let back = Config::from_toml(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn partial_toml_keeps_defaults_for_missing_keys() {
        let cfg = Config::from_toml("[taskbar]\nmode = \"clear\"\nopacity = 0.1\n").unwrap();
        assert_eq!(cfg.taskbar.mode, TaskbarMode::Clear);
        assert_eq!(cfg.taskbar.opacity, 0.1);
        // untouched sections fall back
        assert_eq!(cfg.widget.anchor, WidgetAnchor::TaskbarRight);
        assert!(cfg.clipboard.enabled);
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        let cfg = Config::from_toml("[taskbar]\nopacity = 4.5\n\n[widget]\naudio_min_width = 9000\n")
            .unwrap();
        assert_eq!(cfg.taskbar.opacity, 1.0);
        assert_eq!(cfg.widget.audio_min_width, 900);
        // max is pulled up to stay at least min
        assert!(cfg.widget.audio_max_width >= cfg.widget.audio_min_width);
    }

    #[test]
    fn the_snip_defaults_are_usable_and_clamped() {
        let cfg = Config::default();
        assert!(cfg.snip.enabled);
        assert!(cfg.snip.copy_to_clipboard, "a capture nobody can paste is a bug");
        assert!(!cfg.snip.hotkey.is_empty(), "the feature would be unreachable");
        assert!((0.0..0.9).contains(&cfg.snip.dim));

        let cfg = Config::from_toml("[snip]\ndim = 3.0\n").unwrap();
        assert_eq!(cfg.snip.dim, 0.85, "an extreme mask makes the selection invisible");
    }

    #[test]
    fn corrupt_file_is_quarantined() {
        let dir = std::env::temp_dir().join(format!("wb-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not = = toml").unwrap();

        let mgr = ConfigManager::new(path.clone());
        let cfg = mgr.load().unwrap();
        assert_eq!(cfg.taskbar.mode, TaskbarMode::Acrylic);
        assert!(path.with_extension("toml.broken").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
