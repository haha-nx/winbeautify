//! Screen capture for the screenshot feature.
//!
//! # Status
//!
//! Only the pixel layer is here so far: grabbing the screen, cropping, dimming,
//! scaling and encoding, all free of windows and message loops and covered by
//! tests. The interactive part — the full-screen region selector and the pinned
//! image windows — is **not implemented yet**; it needs a window procedure per
//! overlay and a lifetime story for the selection session, and it is the part
//! that has to be verified on a real desktop.
//!
//! Keeping the pure half landed means the next step builds on code that already
//! passes, rather than starting from a blank file.

pub mod capture;

pub use capture::{grab, virtual_screen, virtual_screen_rect, Shot};
