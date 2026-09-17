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
//! * [`layout`] — turning the schema into rectangles. Pure arithmetic, tested
//!   on its own; the painter and the hit tester both read its output so they
//!   cannot disagree.
//!
//! The window itself (painting, input) is built on top of those, and is the
//! remaining work: see the module list above for what is done.

pub mod access;
pub mod geom;
pub mod layout;
pub mod schema;

pub use access::{read, toggle, write, Value};
pub use schema::{ActionId, Button, Choice, Field, Format, Kind, Section, StatusKind, SECTIONS};
