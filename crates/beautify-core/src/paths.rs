//! Filesystem locations WinBeautify reads and writes.

use std::path::PathBuf;

/// Root of the per-user writable state, `%LOCALAPPDATA%\WinBeautify`.
///
/// `LocalAppData` rather than `Roaming`: the SQLite databases are machine
/// specific, so roaming them across a domain profile would be actively harmful.
pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("WinBeautify")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn database_path() -> PathBuf {
    data_dir().join("winbeautify.db")
}

/// Where hand-written `.lrc` files are read from.
///
/// Read-only as far as WinBeautify is concerned: lyrics fetched from a provider
/// stay in memory for the current track and are never written here, so anything
/// in this directory was put there by the user.
pub fn lyrics_dir() -> PathBuf {
    data_dir().join("lyrics")
}

pub fn logs_dir() -> PathBuf {
    data_dir().join("logs")
}

/// Create every directory the app expects to write into.
pub fn ensure_dirs() -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir())?;
    std::fs::create_dir_all(lyrics_dir())?;
    Ok(())
}
