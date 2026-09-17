//! Geometry for the settings UI.
//!
//! The rectangle itself lives in `beautify_widget::layout`, because the widget
//! bar's drawing primitives take it and having two float rectangles in one
//! process means converting at every call site — and getting that wrong in one
//! of them. What is specific to this crate is [`clamp`].

pub use beautify_widget::layout::Rect;

/// Clip `value` into `min..=max`.
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
        let squashed = rect.inset_by(10.0, 10.0);
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
