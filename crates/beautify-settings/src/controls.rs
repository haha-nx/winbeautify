//! The interactive rectangles inside a row's control area.
//!
//! One function decides where a switch, a slider's track, a colour swatch or a
//! set of buttons lives, and both the painter and the hit tester read its output.
//! Deriving those rectangles twice — once to draw and once to click — is how a
//! control ends up drawn in one place and clickable in another.

use crate::geom::Rect;
use crate::layout::Metrics;
use crate::schema::{Field, Kind};

/// The parts of a row that respond to the mouse.
#[derive(Debug, Clone, Default)]
pub struct Parts {
    /// Boxes in reading order: the switch, the dropdown, the number box, the
    /// colour swatch, the hex box, the status pill.
    pub boxes: Vec<Rect>,
    /// The draggable track of a slider, when the row has one.
    pub slider: Option<Rect>,
    /// Action buttons, in the order the schema lists them.
    pub buttons: Vec<Rect>,
}

/// Which part is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// A box, by index into [`Parts::boxes`].
    Box(usize),
    /// The slider's track; the x position sets the value.
    SliderTrack,
    /// An action button, by index.
    Button(usize),
}

impl Parts {
    /// The part under `(x, y)`, if any.
    ///
    /// The slider is tested first: its hit area is the full row height, so it
    /// would otherwise swallow a neighbouring box that shares those rows.
    pub fn part_at(&self, x: f32, y: f32) -> Option<Part> {
        if let Some(track) = self.slider {
            if track.contains(x, y) {
                return Some(Part::SliderTrack);
            }
        }
        for (index, rect) in self.buttons.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Part::Button(index));
            }
        }
        for (index, rect) in self.boxes.iter().enumerate() {
            if rect.contains(x, y) {
                return Some(Part::Box(index));
            }
        }
        None
    }

    /// The drag position of a slider, as a `0..1` fraction of its track.
    pub fn slider_fraction(&self, x: f32) -> Option<f32> {
        let track = self.slider?;
        if track.width() <= 0.0 {
            return None;
        }
        Some(((x - track.left) / track.width()).clamp(0.0, 1.0))
    }
}

/// Work out the interactive rectangles for one row.
pub fn parts(field: &Field, control: Rect, metrics: &Metrics) -> Parts {
    let mut parts = Parts::default();
    if control.is_empty() {
        return parts;
    }
    // A fixed-height box, centred in the row.
    let boxed = |height: f32| {
        let top = (control.center_y() - height * 0.5).max(control.top);
        Rect::new(control.left, top, control.right, top + height)
    };

    match field.kind {
        Kind::Switch => {
            // Right-aligned and only as wide as a switch, so a column of
            // switches lines up down the page.
            let width = metrics.switch_width();
            let height = metrics.switch_height();
            let top = (control.center_y() - height * 0.5).max(control.top);
            parts.boxes.push(Rect::new(
                (control.right - width).max(control.left),
                top,
                control.right,
                top + height,
            ));
        }
        Kind::Slider(_) => {
            // The read-out takes the right, the track the rest. The track's hit
            // area is the full row height even though the track is drawn thin:
            // a four-pixel target is not something anyone can hit.
            let readout = metrics.slider_readout_width();
            let track = Rect::new(
                control.left,
                control.top,
                (control.right - readout - metrics.control_gap() * 0.5).max(control.left),
                control.bottom,
            );
            parts.slider = Some(track);
            parts
                .boxes
                .push(Rect::new(track.right, control.top, control.right, control.bottom));
        }
        Kind::Select(_) | Kind::Number { .. } | Kind::Text { .. } => {
            parts.boxes.push(boxed(metrics.control_height()));
        }
        Kind::Color => {
            let height = metrics.control_height();
            let top = control.center_y() - height * 0.5;
            // The swatch is clamped to the room available, and the hex box
            // hangs off its *actual* right edge: measuring from the nominal
            // width instead inverts the second box on a narrow column.
            let swatch = metrics.color_swatch_width().min(control.width());
            let swatch_rect = Rect::new(control.left, top, control.left + swatch, top + height);
            parts.boxes.push(swatch_rect);
            let hex_left = (swatch_rect.right + metrics.control_gap() * 0.5).min(control.right);
            parts.boxes.push(Rect::new(hex_left, top, control.right, top + height));
        }
        Kind::Status(_) => {
            // The pill is sized to its own text when it is painted; this box is
            // the space it has to sit in, and is not interactive.
            let height = metrics.control_height() * 0.82;
            let top = (control.center_y() - height * 0.5).max(control.top);
            parts
                .boxes
                .push(Rect::new(control.left, top, control.right, top + height));
        }
        Kind::Action(buttons) => {
            // Right to left, so the first button in the schema sits at the
            // trailing edge and the row grows leftwards.
            let height = metrics.button_height();
            let top = (control.center_y() - height * 0.5).max(control.top);
            let width = metrics.button_width();
            let mut right = control.right;
            for _ in buttons {
                // Out of room: one button per gap is better than a row of
                // inverted rectangles.
                if right <= control.left {
                    break;
                }
                let left = (right - width).max(control.left);
                parts
                    .buttons
                    .push(Rect::new(left, top, right.max(left), top + height));
                right = left - metrics.button_gap();
            }
        }
    }
    parts
}

/// Every field with its kind, for tests that need to walk the whole page.
#[cfg(test)]
fn all_fields() -> impl Iterator<Item = &'static Field> {
    crate::schema::SECTIONS
        .iter()
        .flat_map(|section| section.cards.iter())
        .flat_map(|card| card.fields.iter())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> Metrics {
        Metrics::new(96)
    }

    fn control() -> Rect {
        Rect::new(500.0, 100.0, 740.0, 128.0)
    }

    fn field_with_path(path: &str) -> &'static Field {
        all_fields()
            .find(|field| field.path == path)
            .unwrap_or_else(|| panic!("no field for {path}"))
    }

    #[test]
    fn every_row_produces_something_to_click() {
        let (m, control) = (metrics(), control());
        for field in all_fields() {
            let parts = parts(field, control, &m);
            assert!(
                !parts.boxes.is_empty() || parts.slider.is_some() || !parts.buttons.is_empty(),
                "field {:?} produced no interactive area",
                field.label
            );
        }
    }

    #[test]
    fn no_part_escapes_the_control_column() {
        let (m, control) = (metrics(), control());
        for field in all_fields() {
            let parts = parts(field, control, &m);
            for rect in parts.boxes.iter().chain(parts.buttons.iter()) {
                assert!(rect.width() > 0.0 && rect.height() > 0.0, "{:?}", field.path);
                assert!(rect.right <= control.right + 0.01, "{:?} overflows", field.path);
                assert!(rect.left >= control.left - 0.01, "{:?} underflows", field.path);
            }
        }
    }

    #[test]
    fn a_switch_is_switch_sized_and_right_aligned() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("taskbar.enabled"), control, &m);
        assert_eq!(parts.boxes.len(), 1);
        assert!((parts.boxes[0].width() - m.switch_width()).abs() < 0.01);
        assert!((parts.boxes[0].height() - m.switch_height()).abs() < 0.01);
        assert!((parts.boxes[0].right - control.right).abs() < 0.01);
    }

    #[test]
    fn a_slider_track_and_readout_do_not_overlap() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("widget.opacity"), control, &m);
        let track = parts.slider.expect("a track");
        assert!(track.right <= parts.boxes[0].left + 0.01);
        assert!(track.width() > parts.boxes[0].width());
        // The hit area is the row height, not the drawn line.
        assert!((track.height() - control.height()).abs() < 0.01);
    }

    #[test]
    fn a_colour_row_has_a_swatch_and_a_hex_box() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("ui.accent"), control, &m);
        assert_eq!(parts.boxes.len(), 2);
        assert!((parts.boxes[0].width() - m.color_swatch_width()).abs() < 0.01);
        assert!(parts.boxes[1].left > parts.boxes[0].right);
    }

    #[test]
    fn action_buttons_do_not_overlap_and_can_be_hit() {
        let (m, control) = (metrics(), control());
        let action = all_fields()
            .find(|field| matches!(field.kind, Kind::Action(_)))
            .expect("an action row");
        let parts = parts(action, control, &m);
        let expected = match action.kind {
            Kind::Action(buttons) => buttons.len(),
            _ => 0,
        };
        assert_eq!(parts.buttons.len(), expected);
        assert!((parts.buttons[0].right - control.right).abs() < 0.01);
        for pair in parts.buttons.windows(2) {
            assert!(pair[1].right < pair[0].left, "buttons must not overlap");
        }

        let first = parts.buttons[0];
        let center = ((first.left + first.right) * 0.5, first.center_y());
        assert_eq!(parts.part_at(center.0, center.1), Some(Part::Button(0)));
    }

    #[test]
    fn the_slider_wins_over_boxes_that_share_its_rows() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("widget.opacity"), control, &m);
        let track = parts.slider.unwrap();
        assert_eq!(
            parts.part_at(track.left + 5.0, track.center_y()),
            Some(Part::SliderTrack)
        );
        assert_eq!(parts.part_at(control.left - 40.0, control.center_y()), None);
    }

    #[test]
    fn a_slider_fraction_maps_the_track_ends_to_zero_and_one() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("widget.opacity"), control, &m);
        let track = parts.slider.unwrap();
        assert_eq!(parts.slider_fraction(track.left), Some(0.0));
        assert_eq!(parts.slider_fraction(track.right), Some(1.0));
        assert!((parts.slider_fraction(track.center_y() * 0.0 + (track.left + track.right) * 0.5).unwrap() - 0.5).abs() < 0.01);
        // Dragging past the end clamps rather than overshooting the range.
        assert_eq!(parts.slider_fraction(track.left - 500.0), Some(0.0));
        assert_eq!(parts.slider_fraction(track.right + 500.0), Some(1.0));
    }

    #[test]
    fn a_squeezed_control_lays_out_without_inverting() {
        let m = metrics();
        let tiny = Rect::new(0.0, 0.0, 10.0, 20.0);
        for field in all_fields() {
            let parts = parts(field, tiny, &m);
            for rect in parts.boxes.iter().chain(parts.buttons.iter()) {
                assert!(rect.width() >= 0.0 && rect.height() >= 0.0, "{:?}", field.path);
            }
            if let Some(track) = parts.slider {
                assert!(track.width() >= 0.0);
            }
        }
    }

    #[test]
    fn an_empty_control_yields_nothing() {
        let parts = parts(field_with_path("ui.accent"), Rect::EMPTY, &metrics());
        assert!(parts.boxes.is_empty() && parts.slider.is_none() && parts.buttons.is_empty());
    }
}
