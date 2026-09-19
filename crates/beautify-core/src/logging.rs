//! `tracing` setup shared by the tray host and any standalone debug binary.

use crate::paths;

/// Third-party crates that are chatty even at `debug`.
///
/// The filter is *the configured level for everything*, with these quietened.
/// The previous shape — a level per workspace crate, listed by name — was a trap
/// that has now caught three crates in a row: a directive matches a target as a
/// module path, so a crate that is not named is silent, and `beautify_snip`,
/// `beautify_settings` and `beautify_flyout` each lost their warnings and errors
/// (including the one that would have explained why a window never appeared).
/// Listing what to *silence* means a new crate is covered the day it is added.
const QUIET: [&str; 8] = [
    "winit",
    "tao",
    "wry",
    "tauri",
    "muda",
    "tray_icon",
    "hyper",
    "mio",
];

/// The default filter directives for `level`.
pub fn directives(level: &str) -> String {
    let mut directives = level.to_string();
    for krate in QUIET {
        directives.push_str(&format!(",{krate}=warn"));
    }
    directives
}

/// Install the global subscriber.
///
/// The console layer is unconditional but only reaches a screen in debug builds,
/// where the host keeps the console subsystem; the release binary is a GUI image
/// with no console to write to, so `file_logging` is how a release build gets a
/// log at all. The file sink stays opt-in because writing a log every few
/// seconds would burn through SSD writes for no benefit on a healthy install.
pub fn init(level: &str, to_file: bool) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::EnvFilter;

    // `RUST_LOG` wins so a user can debug without editing config.toml.
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directives(level)));

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_filter_covers_every_crate_by_default() {
        let text = directives("debug");
        // No workspace crate is named, so none can be forgotten: a crate added
        // tomorrow inherits the configured level.
        assert!(
            !text.contains("beautify_"),
            "our own crates must not need an entry: {text}"
        );
        assert!(text.starts_with("debug,"), "the level leads: {text}");
        // …and the noisy ones are quietened.
        for krate in ["winit", "tauri", "wry"] {
            assert!(
                text.contains(&format!("{krate}=warn")),
                "{krate} should be quietened: {text}"
            );
        }
    }

    #[test]
    fn the_configured_level_is_the_one_used() {
        assert!(directives("info").starts_with("info,"));
        assert!(directives("warn").starts_with("warn,"));
    }
}
