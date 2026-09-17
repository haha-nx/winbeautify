//! `tracing` setup shared by the tray host and any standalone debug binary.

use crate::paths;

/// Every crate in the workspace, so the default filter can name them.
///
/// A directive matches a target as a module path, so `beautify` does **not**
/// cover `beautify_media` — the crates have to be listed. (This used to be a
/// single `beautify_{level}` directive, which built the target name
/// `beautify_info` and matched nothing: every module crate was silent unless
/// `RUST_LOG` was set, including the warnings meant for the user.)
const CRATES: [&str; 6] = [
    "beautify_core",
    "beautify_taskbar",
    "beautify_media",
    "beautify_clipboard",
    "beautify_todo",
    "beautify_widget",
];

/// Install the global subscriber.
///
/// Console output is always on (the app is normally launched from Explorer, so
/// this is harmless); the rotating file sink is opt-in because writing a log
/// every few seconds would burn through SSD writes for no benefit on a healthy
/// install.
pub fn init(level: &str, to_file: bool) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    // `RUST_LOG` wins so a user can debug without editing config.toml.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        let mut directives = format!("winbeautify={level}");
        for krate in CRATES {
            directives.push_str(&format!(",{krate}={level}"));
        }
        EnvFilter::new(directives)
    });

    let registry = tracing_subscriber::registry().with(filter);

    if to_file {
        let dir = paths::logs_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            match std::fs::File::create(dir.join("winbeautify.log")) {
                Ok(file) => {
                    let file_layer = tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(std::sync::Mutex::new(file));
                    let _ = registry.with(file_layer).try_init();
                    return;
                }
                Err(e) => eprintln!("WinBeautify: cannot open log file: {e}"),
            }
        }
    }

    let _ = registry
        .with(tracing_subscriber::fmt::layer().with_ansi(false))
        .try_init();
}
