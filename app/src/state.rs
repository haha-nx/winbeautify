//! Process-wide application state.
//!
//! One [`AppState`] lives in Tauri's managed state for the lifetime of the
//! process. It owns the module handles — which are `Arc`s, shared with the
//! lifecycle registry — plus the bits of UI state that outlive any single
//! window (the remembered flyout tab, the widget bar's current width).

use beautify_clipboard::ClipboardModule;
use beautify_core::config::ConfigManager;
use beautify_core::event::EventBus;
use beautify_core::model::{FlyoutTab, TaskbarState};
use beautify_core::{ModuleContext, Registry};
use beautify_media::MediaModule;
use beautify_taskbar::TaskbarModule;
use beautify_todo::TodoModule;
use beautify_widget::WidgetModule;
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

use crate::hotkeys::HotkeyRegistry;

pub struct AppState {
    pub bus: EventBus,
    pub config: Arc<ConfigManager>,
    pub registry: Registry,

    pub taskbar: Arc<TaskbarModule>,
    pub media: Arc<MediaModule>,
    pub clipboard: Arc<ClipboardModule>,
    pub todo: Arc<TodoModule>,
    /// Native (Direct2D) widget bar. Inert when the webview renderer is selected.
    pub widget: Arc<WidgetModule>,

    /// Which flyout tab the launcher reopens.
    pub flyout_tab: RwLock<FlyoutTab>,
    /// Width the widget bar last asked for, so a config change can reposition
    /// without waiting for the webview to re-measure.
    pub widget_width: AtomicI32,
    /// Whether the flyout window is currently shown.
    ///
    /// Tracked here because asking Tauri (`window.is_visible()`) blocks on a
    /// reply from the main thread, and the widget bar's pump thread must never
    /// block — it would stop processing mouse input entirely.
    pub flyout_visible: AtomicBool,
    /// Latest taskbar state, cached for the settings status pill.
    pub taskbar_state: RwLock<TaskbarState>,
    /// Global hotkeys, rebuilt whenever the configured bindings change.
    pub hotkeys: Mutex<Option<HotkeyRegistry>>,
    /// The native flyout panel — task list and clipboard history — created on
    /// first use and then hidden and reused.
    pub flyout_panel: Mutex<Option<beautify_flyout::Panel>>,
    /// The native settings window, created on first use.
    ///
    /// Its `Host` implementation needs an `AppHandle`, which does not exist
    /// until after this struct does, so the window is built lazily rather than
    /// in [`AppState::new`].
    pub settings_window: Mutex<Option<beautify_settings::SettingsWindow>>,
    /// What the last settings action or capture did, for the 关于 page.
    ///
    /// The settings window asks the host for its values on every repaint, so
    /// this is the channel a result reaches it through — there is no return path
    /// from an action row to the page.
    pub last_action: RwLock<String>,
    /// Set while `run()` is unwinding so background callbacks stop touching
    /// windows that are going away.
    pub shutting_down: AtomicBool,
    /// True once the module registry has been started.
    pub started: AtomicBool,
    /// Bus subscriptions that must outlive any single window. Kept here purely
    /// so they are not dropped — dropping a [`beautify_core::Subscription`]
    /// unsubscribes.
    pub subscriptions: Mutex<Vec<beautify_core::Subscription>>,
}

impl AppState {
    pub fn new(config: Arc<ConfigManager>) -> Arc<Self> {
        let bus = EventBus::new();
        let taskbar = Arc::new(TaskbarModule::new());
        let media = Arc::new(MediaModule::new());
        let clipboard = Arc::new(ClipboardModule::new());
        let todo = Arc::new(TodoModule::new());
        let widget = Arc::new(WidgetModule::new());

        let mut registry = Registry::new();
        registry
            .register(Arc::clone(&taskbar) as Arc<dyn beautify_core::Module>)
            .register(Arc::clone(&clipboard) as Arc<dyn beautify_core::Module>)
            .register(Arc::clone(&todo) as Arc<dyn beautify_core::Module>)
            .register(Arc::clone(&media) as Arc<dyn beautify_core::Module>)
            .register(Arc::clone(&widget) as Arc<dyn beautify_core::Module>);

        Arc::new(Self {
            bus,
            config,
            registry,
            taskbar,
            media,
            clipboard,
            todo,
            widget,
            flyout_tab: RwLock::new(FlyoutTab::Todo),
            widget_width: AtomicI32::new(0),
            taskbar_state: RwLock::new(TaskbarState::default()),
            flyout_visible: AtomicBool::new(false),
            hotkeys: Mutex::new(None),
            flyout_panel: Mutex::new(None),
            settings_window: Mutex::new(None),
            last_action: RwLock::new(String::new()),
            shutting_down: AtomicBool::new(false),
            started: AtomicBool::new(false),
            subscriptions: Mutex::new(Vec::new()),
        })
    }

    /// Start every enabled module. Idempotent.
    pub fn start_modules(&self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let ctx = ModuleContext::new(self.bus.clone(), Arc::clone(&self.config));
        self.registry.start_all(ctx);
    }

    /// Push the current config into every module.
    pub fn apply_config(&self, config: &beautify_core::Config) {
        if !self.started.load(Ordering::Acquire) {
            return;
        }
        let ctx = ModuleContext::new(self.bus.clone(), Arc::clone(&self.config));
        self.registry.apply_all(&ctx, config);
    }

    pub fn stop_modules(&self) {
        self.shutting_down.store(true, Ordering::Release);
        if let Some(mut hotkeys) = self.hotkeys.lock().take() {
            hotkeys.stop();
        }
        // Pinned screenshots are topmost windows of ours; leaving them up after
        // the process is gone is impossible, but closing them first means they
        // disappear while the desktop is still ours to clean up.
        beautify_snip::close_all_pins();
        if let Some(window) = self.settings_window.lock().take() {
            window.close();
        }
        if let Some(panel) = self.flyout_panel.lock().take() {
            panel.hide();
        }
        self.registry.stop_all();
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }
}
