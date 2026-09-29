//! Re-export of the layered-window surface.
//!
//! [`beautify_widget::surface::LayeredSurface`] is the plumbing that turns a
//! premultiplied-BGRA buffer into a `WS_EX_LAYERED` window's pixels, and it is
//! exactly what a toast needs. Re-exporting it rather than reaching into
//! `beautify_widget::surface` from every call site keeps the dependency in one
//! place, so there is one line to change if the surface ever moves.

pub use beautify_widget::surface::LayeredSurface;
