//! The module contract every feature crate implements.
//!
//! Modules are shared as `Arc<dyn Module>` because two things need them at
//! once: the host's lifecycle registry (start / apply / stop) and the UI layer,
//! which calls query and control methods on the concrete type. Every method is
//! therefore `&self`, and a module keeps its mutable state behind its own locks
//! — which is what it has to do anyway, since its worker threads outlive the
//! call that started them.

use crate::config::Config;
use crate::event::EventBus;
use std::sync::Arc;

/// Everything a module needs from the host at startup.
#[derive(Clone)]
pub struct ModuleContext {
    /// Shared publish/subscribe bus.
    pub bus: EventBus,
    /// The live configuration.
    pub config: Arc<crate::config::ConfigManager>,
}

impl ModuleContext {
    pub fn new(bus: EventBus, config: Arc<crate::config::ConfigManager>) -> Self {
        Self { bus, config }
    }

    /// Take an owned snapshot of the current config.
    pub fn snapshot(&self) -> Config {
        self.config.get()
    }
}

pub type ModuleResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// A feature module (taskbar, media, clipboard, todo).
pub trait Module: Send + Sync {
    /// Stable identifier, also used as the config section name.
    fn name(&self) -> &'static str;

    /// True when the user has the module switched on in its config section.
    fn is_enabled(&self, config: &Config) -> bool;

    /// Spin up threads, hooks and native resources. Idempotent: calling it on a
    /// running module is a no-op.
    fn start(&self, ctx: ModuleContext) -> ModuleResult;

    /// Re-read the config. Must be cheap and must not restart threads.
    fn apply(&self, config: &Config) -> ModuleResult;

    /// Release everything acquired in [`Module::start`]. Idempotent.
    fn stop(&self) -> ModuleResult;
}

/// Ordered collection of modules with panic isolation.
///
/// A module that fails to start is logged and skipped rather than aborting the
/// process: losing the taskbar accent is annoying, losing the clipboard history
/// because of it would be worse.
#[derive(Clone, Default)]
pub struct Registry {
    modules: Vec<Arc<dyn Module>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, module: Arc<dyn Module>) -> &mut Self {
        self.modules.push(module);
        self
    }

    pub fn names(&self) -> Vec<&'static str> {
        self.modules.iter().map(|m| m.name()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Start every enabled module.
    pub fn start_all(&self, ctx: ModuleContext) {
        let config = ctx.config.get();
        for module in &self.modules {
            if !module.is_enabled(&config) {
                tracing::debug!(module = module.name(), "disabled by config; not starting");
                continue;
            }
            let started = std::time::Instant::now();
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| module.start(ctx.clone())))
            {
                Ok(Ok(())) => tracing::info!(
                    module = module.name(),
                    ms = started.elapsed().as_millis() as u64,
                    "module started"
                ),
                Ok(Err(e)) => tracing::error!(module = module.name(), "start failed: {e}"),
                Err(_) => tracing::error!(module = module.name(), "panicked during start"),
            }
        }
    }

    /// Push new settings into every module, honouring enable/disable flips.
    pub fn apply_all(&self, config: &Config) {
        for module in &self.modules {
            let enabled = module.is_enabled(config);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if enabled {
                    module.apply(config)
                } else {
                    module.stop()
                }
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::error!(module = module.name(), "apply failed: {e}"),
                Err(_) => tracing::error!(module = module.name(), "panicked during apply"),
            }
        }
    }

    /// Best-effort shutdown; used on exit and on session logout.
    pub fn stop_all(&self) {
        // Reverse order so modules that depend on earlier ones tear down first.
        for module in self.modules.iter().rev() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| module.stop()));
        }
    }
}
