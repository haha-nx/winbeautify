//! WinBeautify shared foundation.
//!
//! This crate deliberately depends on nothing platform-specific: it defines the
//! configuration schema, the event bus, the module contract and the data model
//! that the native modules and the web UI agree on.

pub mod config;
pub mod event;
pub mod geometry;
pub mod logging;
pub mod model;
pub mod module;
pub mod paths;

pub use config::{Config, ConfigManager, TaskbarMode, WidgetAnchor};
pub use event::{Event, EventBus, Subscription};
pub use geometry::{Color, Rect};
pub use model::{
    FlyoutTab, LyricLine, Lyrics, MediaSnapshot, PlaybackStatus, SpectrumFrame, TaskbarState,
    TaskbarVisualState,
};
pub use module::{Module, ModuleContext, ModuleResult, Registry};

/// Product name, used for window titles, the registry Run key and logs.
pub const APP_NAME: &str = "WinBeautify";
