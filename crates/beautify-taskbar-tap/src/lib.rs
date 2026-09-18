//! `beautify_taskbar_tap.dll` — the module WinBeautify loads into
//! explorer.exe to repaint the Windows 11 22H2+ taskbar.
//!
//! The Windows 11 22H2+ taskbar is a XAML island inside explorer, so its
//! background cannot be changed from outside the process: every composition
//! call on the taskbar window succeeds and does nothing. This module is the
//! same shape as TranslucentTB's `ExplorerTAP` (the mechanism is public
//! knowledge): the host forces this DLL into explorer through a
//! `WH_CALLWNDPROC` hook, the DLL connects to the XAML diagnostics framework
//! (`InitializeXamlDiagnosticsEx`), watches the taskbar's visual tree, and
//! repaints the `BackgroundFill`/`BackgroundStroke` rectangles on demand.
//!
//! Control travels over a message-only window the TAP creates on the XAML UI
//! thread; commands are single `WM_COPYDATA` structs (see [`protocol`]).

// The whole crate is hand-rolled COM glue where every entry point is an
// `unsafe extern "system"` fn that the framework or the OS calls; writing
// `# Safety` prose for each would repeat "this is a raw COM entry point"
// thirty times without adding information. The safety invariants are
// documented where they are non-obvious.
#![allow(clippy::missing_safety_doc)]

pub mod com;
pub mod effects;
pub mod protocol;
pub mod service;
pub mod site;
pub mod watcher;
pub mod xaml;
