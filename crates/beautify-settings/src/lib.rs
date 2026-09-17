//! The native settings window.
//!
//! Replaces the WebView2 settings page: the same fields, drawn with Direct2D in
//! this process, so opening the settings no longer costs a Chromium process tree.
//!
//! # Layout of the crate
//!
//! * [`schema`] — the page as data: sections, cards and about sixty fields, each
//!   with its label, hint, control kind and visibility rule. Nothing here knows
//!   how it will be drawn.
//! * [`access`] — reading and writing a field by its config path.
//! * [`geom`] — the float rectangle the layout works in.
//! * [`layout`] — turning the schema into rectangles. Pure arithmetic, tested on
//!   its own; the painter and the hit tester both read its output so they cannot
//!   disagree.
//! * [`controls`] — the interactive rectangles inside a row, again shared by the
//!   painter and the hit tester.
//! * [`paint`] — what a control looks like given its rectangle and state.
//! * [`window`] — the window, its message loop and its input handling. It talks
//!   to the application only through [`window::Host`], which is what keeps this
//!   crate free of any dependency on the binary that hosts it.

pub mod access;
pub mod controls;
pub mod geom;
pub mod layout;
pub mod paint;
pub mod palette;
pub mod schema;
pub mod window;

pub use access::{read, toggle, write, Value};
pub use controls::{Part, Parts};
pub use paint::{Interaction, StatusText, Tone};
pub use schema::{
    ActionId, Button, Choice, Field, Format, InfoKey, Kind, Section, StatusKind, SECTIONS,
};
pub use window::{Host, SettingsWindow, CLASS_NAME};
