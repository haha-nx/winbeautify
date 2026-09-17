//! Float rectangles for UI layout.
//!
//! [`beautify_core::geometry::Rect`] is integer and screen-oriented — it models
//! monitor and taskbar bounds, where a half pixel is meaningless. A UI layout
//! scaled by DPI does have half pixels, and rounding at every step accumulates
//! into visibly misaligned text, so this is a separate type on purpose.

/// A rectangle in logical device-independent units, already scaled to pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    /// An empty rectangle at the origin, used for "no such element".
    pub const EMPTY: Rect = Rect {
        left: 0.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    };

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }

    pub fn is_empty(&self) -> bool {
        self.width() <= 0.0 || self.height() <= 0.0
    }

    pub fn center_y(&self) -> f32 {
        (self.top + self.bottom) * 0.5
    }

    /// Is `(x, y)` inside? Half-open on the right and bottom edges, so two
    /// adjacent rectangles never both claim the pixel between them.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        !self.is_empty() && x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }

    /// Shrink on every side.
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(
            self.left + dx,
            self.top + dy,
            (self.right - dx).max(self.left + dx),
            (self.bottom - dy).max(self.top + dy),
        )
    }

    /// Move down by `dy` (negative moves up).
    pub fn shifted(&self, dy: f32) -> Rect {
        Rect::new(self.left, self.top + dy, self.right, self.bottom + dy)
    }

    /// Take `width` off the left edge, returning that strip.
    pub fn take_left(&self, width: f32) -> Rect {
        Rect::new(self.left, self.top, (self.left + width).min(self.right), self.bottom)
    }

    /// The right-hand `width` of this rectangle.
    pub fn right_part(&self, width: f32) -> Rect {
        Rect::new(
            (self.right - width).max(self.left),
            self.top,
            self.right,
            self.bottom,
        )
    }

    /// A horizontal slice of height `height` from the top.
    pub fn take_top(&self, height: f32) -> Rect {
        Rect::new(self.left, self.top, self.right, (self.top + height).min(self.bottom))
    }

    /// Clamp to `other`, for deciding what is on screen.
    pub fn clipped_to(&self, other: Rect) -> Rect {
        Rect::new(
            self.left.max(other.left),
            self.top.max(other.top),
            self.right.min(other.right),
            self.bottom.min(other.bottom),
        )
    }
}

/// Clip a value into a range.
pub fn clamp(value: f32, min: f32, max: f32) -> f32 {
    if max < min {
        return min;
    }
    value.max(min).min(max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_is_half_open() {
        let rect = Rect::new(10.0, 10.0, 20.0, 20.0);
        assert!(rect.contains(10.0, 10.0));
        assert!(rect.contains(19.9, 19.9));
        assert!(!rect.contains(20.0, 15.0), "the right edge belongs to the next cell");
        assert!(!rect.contains(15.0, 20.0));
        assert!(!rect.contains(9.9, 15.0));
    }

    #[test]
    fn an_empty_rectangle_contains_nothing() {
        assert!(!Rect::EMPTY.contains(0.0, 0.0));
        assert!(Rect::EMPTY.is_empty());
        assert!(Rect::new(0.0, 0.0, 0.0, 10.0).is_empty());
    }

    #[test]
    fn insetting_cannot_turn_a_rectangle_inside_out() {
        let rect = Rect::new(0.0, 0.0, 4.0, 4.0);
        let squashed = rect.inset(10.0, 10.0);
        assert_eq!(squashed.width(), 0.0);
        assert!(squashed.height() >= 0.0);
    }

    #[test]
    fn taking_a_strip_leaves_the_rest_alone() {
        let rect = Rect::new(0.0, 0.0, 100.0, 50.0);
        let left = rect.take_left(30.0);
        assert_eq!(left.right, 30.0);
        assert_eq!(left.height(), 50.0);
        let right = rect.right_part(40.0);
        assert_eq!(right.left, 60.0);
        assert_eq!(right.right, 100.0);
        // Asking for more than there is clamps rather than inverting.
        assert_eq!(rect.take_left(500.0).right, 100.0);
        assert_eq!(rect.right_part(500.0).left, 0.0);
    }

    #[test]
    fn clipping_bounds_a_row_to_the_viewport() {
        let row = Rect::new(0.0, -20.0, 100.0, 30.0);
        let viewport = Rect::new(0.0, 0.0, 100.0, 200.0);
        let visible = row.clipped_to(viewport);
        assert_eq!(visible.top, 0.0);
        assert_eq!(visible.bottom, 30.0);
        // A row entirely above the viewport has no visible area.
        assert!(Rect::new(0.0, -50.0, 100.0, -10.0).clipped_to(viewport).is_empty());
    }

    #[test]
    fn clamping_handles_an_inverted_range() {
        assert_eq!(clamp(5.0, 10.0, 0.0), 10.0);
        assert_eq!(clamp(-5.0, 0.0, 10.0), 0.0);
        assert_eq!(clamp(15.0, 0.0, 10.0), 10.0);
        assert_eq!(clamp(5.0, 0.0, 10.0), 5.0);
    }
}
