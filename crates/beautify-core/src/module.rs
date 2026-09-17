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
    ///
    /// A module an earlier apply disabled has had its threads joined, so an
    /// enabled module is started before the config is applied: [`Module::start`]
    /// is contractually idempotent, which makes that a no-op for one already
    /// running and a revival for one that is not.
    pub fn apply_all(&self, ctx: &ModuleContext, config: &Config) {
        for module in &self.modules {
            let enabled = module.is_enabled(config);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if enabled {
                    module.start(ctx.clone())?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A module whose enable flag and lifecycle counters the test drives
    /// directly, bypassing any real config file. `start`/`stop` honour the
    /// trait's idempotency contract the way every real module must: a start
    /// while running is a no-op, so the counters only move on real flips.
    struct CountingModule {
        enabled: AtomicBool,
        running: AtomicBool,
        starts: AtomicUsize,
        applies: AtomicUsize,
        stops: AtomicUsize,
    }

    impl CountingModule {
        fn new() -> Self {
            Self {
                enabled: AtomicBool::new(false),
                running: AtomicBool::new(false),
                starts: AtomicUsize::new(0),
                applies: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
            }
        }

        fn set_enabled(&self, enabled: bool) {
            self.enabled.store(enabled, Ordering::Release);
        }

        fn starts(&self) -> usize {
            self.starts.load(Ordering::Acquire)
        }

        fn stops(&self) -> usize {
            self.stops.load(Ordering::Acquire)
        }
    }

    impl Module for CountingModule {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn is_enabled(&self, _config: &Config) -> bool {
            self.enabled.load(Ordering::Acquire)
        }

        fn start(&self, _ctx: ModuleContext) -> ModuleResult {
            if self.running.swap(true, Ordering::AcqRel) {
                return Ok(());
            }
            self.starts.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn apply(&self, _config: &Config) -> ModuleResult {
            self.applies.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn stop(&self) -> ModuleResult {
            if !self.running.swap(false, Ordering::AcqRel) {
                return Ok(());
            }
            self.stops.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    }

    fn ctx() -> ModuleContext {
        ModuleContext::new(
            EventBus::new(),
            Arc::new(crate::config::ConfigManager::new(std::path::PathBuf::from(
                "unused.toml",
            ))),
        )
    }

    /// The regression behind "turn the widget off and on in settings and it
    /// never comes back": disabling joins the module's threads, so re-enabling
    /// has to start it again — `apply` alone cannot, by contract.
    #[test]
    fn re_enabling_a_stopped_module_starts_it_again() {
        let module = Arc::new(CountingModule::new());
        let mut registry = Registry::new();
        registry.register(Arc::clone(&module) as Arc<dyn Module>);
        let context = ctx();

        module.set_enabled(true);
        registry.apply_all(&context, &Config::default());
        assert_eq!(module.starts(), 1, "first enable must start the module");

        module.set_enabled(false);
        registry.apply_all(&context, &Config::default());
        assert_eq!(module.stops(), 1, "disabling must stop the module");

        module.set_enabled(true);
        registry.apply_all(&context, &Config::default());
        assert_eq!(
            module.starts(),
            2,
            "re-enabling must start the module again, not only apply the config"
        );
    }

    /// The flip side: a module that stayed enabled must not be restarted by an
    /// unrelated config change — a restart would drop live state (and, for the
    /// todo store, re-run carry-over).
    #[test]
    fn an_enabled_module_is_not_restarted_by_a_config_change() {
        let module = Arc::new(CountingModule::new());
        let mut registry = Registry::new();
        registry.register(Arc::clone(&module) as Arc<dyn Module>);
        let context = ctx();

        module.set_enabled(true);
        registry.apply_all(&context, &Config::default());
        registry.apply_all(&context, &Config::default());
        registry.apply_all(&context, &Config::default());
        assert_eq!(module.starts(), 1);
        assert_eq!(module.stops(), 0);
    }
}
