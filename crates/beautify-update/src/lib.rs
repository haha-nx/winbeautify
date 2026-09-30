//! Self-update plumbing for WinBeautify.
//!
//! # The shape of an update
//!
//! WinBeautify's TAP DLL is mapped inside explorer.exe and stays there for the
//! shell's lifetime, and the running app holds both its own exe and that DLL
//! open. Nothing on the install side can be overwritten while either process
//! lives, so the update runs in two processes:
//!
//! 1. **The app** (this crate's [`check`] and [`stage`]) resolves the latest
//!    release, downloads the zip plus its `.sha256` sidecar, verifies the
//!    hash, unpacks the payload into a staging directory — and then spawns a
//!    fresh copy of the app exe in *helper mode* and shuts itself down.
//! 2. **The helper** (orchestrated by the app crate around
//!    [`apply_payload`], not in this crate — it needs the shell restart from
//!    `beautify-taskbar`) waits for the old process to exit, restarts the
//!    shell, and copies the staged payload over the install directory.
//!
//! # Files that refuse to die
//!
//! Windows locks a running exe and a mapped DLL against writes and deletes,
//! but — usefully — not against a rename on the same volume. [`apply_payload`]
//! leans on that: a file that cannot be overwritten is renamed to
//! `<name>.old` (making the target path free), the new file is copied into
//! place, and the `.old` copy is swept by the next startup
//! ([`cleanup_old_files`]) once whatever mapped it has exited. Worst case a
//! stale `.old` lingers until the shell has restarted at some point; it never
//! blocks an update.
//!
//! # What this crate deliberately does not know
//!
//! No UI, no single-instance rules, no shell: errors are user-presentable
//! strings, the caller decides how to show them, and the orchestration that
//! needs Win32 (process waits, explorer restart) lives one layer up.

pub mod apply;
pub mod release;
pub mod stage;
pub mod version;

pub use apply::{apply_payload, cleanup_old_files, payload_is_valid};
pub use release::{check, Release, RELEASE_REPO};
pub use stage::stage;
pub use version::{parse_tag, parse_version, is_newer};
