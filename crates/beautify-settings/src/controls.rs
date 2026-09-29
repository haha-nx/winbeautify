//! The interactive rectangles inside a row's control area.
//!
//! One function decides where a switch, a slider's track, a colour swatch or a
//! set of buttons lives, and both the painter and the hit tester read its output.
//! Deriving those rectangles twice — once to draw and once to click — is how a
//! control ends up drawn in one place and clickable in another.

use crate::devices::DeviceRow;
use crate::geom::Rect;
use crate::layout::Metrics;
use crate::schema::{Field, Kind};

/// The parts of a row that respond to the mouse.
#[derive(Debug, Clone, Default)]
pub struct Parts {
    /// Boxes in reading order: the switch, the dropdown, the number box, the
    /// colour swatch, the hex box, the status pill — or, for a checkbox list,
    /// one box per device line.
    pub boxes: Vec<Rect>,
    /// The draggable track of a slider, when the row has one.
    pub slider: Option<Rect>,
    /// Action buttons, in the order the schema lists them — or, for a checkbox
    /// list, one drag handle per *ticked* device, in ticked order: button `k`
    /// belongs to the `k`-th ticked entry.
    pub buttons: Vec<Rect>,
}

/// Which part is under a point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// A box, by index into [`Parts::boxes`].
    ///
    /// For a checkbox list this is the device at the same index in
    /// `Row::devices`, so the caller reads the id from there rather than
    /// guessing from the rectangle.
    Box(usize),
    /// The slider's track; the x position sets the value.
    SliderTrack,
    /// An action button, by index.
    ///
    /// For a checkbox list the handles are laid out one per *ticked* device, in
    /// ticked order: the index *is* the ticked position the handle reorders.
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
pub fn parts(field: &Field, control: Rect, metrics: &Metrics, devices: &[DeviceRow]) -> Parts {
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
            // A full gap, not half: the knob is drawn *centred* on the track's
            // end, so it sticks out by its own radius, and at the maximum value
            // it would otherwise sit on top of the number it is showing.
            let track = Rect::new(
                control.left,
                control.top,
                (control.right - readout - metrics.control_gap()).max(control.left),
                control.bottom,
            );
            parts.slider = Some(track);
            parts
                .boxes
                .push(Rect::new(track.right, control.top, control.right, control.bottom));
        }
        Kind::Select(_) | Kind::Number { .. } | Kind::Text { .. } | Kind::Hotkey => {
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
        Kind::CheckboxList(_) => {
            // One line per device. The checkbox is a square at the leading
            // edge; the drag handle sits at the trailing edge of each *ticked*
            // line — a long press on it starts a drag that reorders the list.
            let line = metrics.device_row_height();
            let gap = metrics.device_row_gap();
            let box_size = metrics.checkbox_size();
            let handle = metrics.arrow_size();

            if devices.is_empty() {
                // The "nothing here" line: it is drawn, but there is nothing to
                // click on it.
                return parts;
            }

            for (index, device) in devices.iter().enumerate() {
                let top = control.top + index as f32 * (line + gap);
                if top >= control.bottom {
                    break;
                }
                // The checkbox is centred on the line.
                let box_top = top + (line - box_size) * 0.5;
                parts.boxes.push(Rect::new(
                    control.left,
                    box_top,
                    control.left + box_size,
                    box_top + box_size,
                ));

                if device.ticked_index.is_some() {
                    let handle_top = top + (line - handle) * 0.5;
                    let left = (control.right - handle).max(control.left);
                    if left > control.left {
                        // Button `k` is the arrow of the `k`-th ticked line,
                        // which is exactly what a reorder acts on.
                        parts.buttons.push(Rect::new(left, handle_top, control.right, handle_top + handle));
                    }
                }
            }
        }
        // A read-only value has nothing to click and nothing to type; the
        // painter reads its rectangle straight off the row.
        Kind::Info(_) => {}
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

    /// A control column tall enough for several device lines.
    ///
    /// The real layout sizes a checkbox list from the device count — see
    /// `layout::device_list_height` — so `control()` above, which is one row
    /// tall, would clip all but the first line. These tests are about where the
    /// lines land, so they hand over a rectangle the layout would actually have
    /// produced.
    fn tall_control() -> Rect {
        Rect::new(500.0, 100.0, 740.0, 320.0)
    }

    /// A resolved device line, for the checkbox-list kind.
    fn device(id: &str, ticked: bool, ticked_index: Option<usize>) -> DeviceRow {
        DeviceRow {
            id: id.to_string(),
            name: id.to_string(),
            kind: "音箱".to_string(),
            is_default: false,
            ticked,
            ticked_index,
        }
    }

    fn field_with_path(path: &str) -> &'static Field {
        all_fields()
            .find(|field| field.path == path)
            .unwrap_or_else(|| panic!("no field for {path}"))
    }

    /// Every field, paired with the device lines its row would carry.
    ///
    /// Checkbox lists are the only kind that needs them; everything else gets
    /// an empty slice, which is exactly what the layout passes.
    fn all_fields_with_devices() -> Vec<(&'static Field, Vec<DeviceRow>)> {
        all_fields()
            .map(|field| match field.kind {
                Kind::CheckboxList(_) => (
                    field,
                    vec![
                        device("a", true, Some(0)),
                        device("b", true, Some(1)),
                        device("c", false, None),
                    ],
                ),
                _ => (field, Vec::new()),
            })
            .collect()
    }

    #[test]
    fn every_row_produces_something_to_click() {
        let (m, control) = (metrics(), control());
        for (field, devices) in all_fields_with_devices() {
            let parts = parts(field, control, &m, &devices);
            // A checkbox list with no devices has nothing to click on purpose:
            // there is no device to tick. Every other interactive kind always
            // offers something.
            let is_list = matches!(field.kind, Kind::CheckboxList(_));
            let expected = field.kind.is_interactive() && (!is_list || !devices.is_empty());
            let produced =
                !parts.boxes.is_empty() || parts.slider.is_some() || !parts.buttons.is_empty();
            if expected {
                assert!(
                    produced,
                    "field {:?} produced no interactive area",
                    field.label
                );
            } else {
                assert!(
                    !produced,
                    "field {:?} is read-only or empty but offers something to click",
                    field.label
                );
            }
        }
    }

    #[test]
    fn no_part_escapes_the_control_column() {
        let (m, control) = (metrics(), control());
        for (field, devices) in all_fields_with_devices() {
            let parts = parts(field, control, &m, &devices);
            for rect in parts.boxes.iter().chain(parts.buttons.iter()) {
                assert!(rect.width() > 0.0 && rect.height() > 0.0, "{:?}", field.path);
                assert!(rect.right <= control.right + 0.01, "{:?} overflows", field.path);
                assert!(rect.left >= control.left - 0.01, "{:?} underflows", field.path);
            }
        }
    }

    /// Each device line gets a checkbox, and a ticked line gets a pair of
    /// arrows — which is what makes the order editable.
    #[test]
    fn a_checkbox_list_gives_every_device_a_box_and_ticked_ones_a_handle() {
        let (m, control) = (metrics(), tall_control());
        let field = field_with_path("audio_switch.speakers");
        let devices = vec![
            device("a", true, Some(0)),
            device("b", true, Some(1)),
            device("c", false, None),
        ];
        let parts = parts(field, control, &m, &devices);

        assert_eq!(parts.boxes.len(), 3, "one checkbox per device");
        assert_eq!(parts.buttons.len(), 2, "one drag handle per ticked line");

        // The boxes run down the list in order, without overlapping.
        for pair in parts.boxes.windows(2) {
            assert!(pair[1].top >= pair[0].bottom - 0.01, "lines must not overlap");
        }
        // Every box is square and inside the control.
        for rect in &parts.boxes {
            assert!((rect.width() - m.checkbox_size()).abs() < 0.01);
            assert!((rect.height() - m.checkbox_size()).abs() < 0.01);
        }
        // Each handle sits on its own ticked line, at the trailing edge. The
        // first two lines are the ticked ones, so their boxes pair with the
        // two handles by line order.
        for (box_rect, handle) in parts.boxes.iter().zip(parts.buttons.iter()) {
            assert!(
                handle.center_y() >= box_rect.top && handle.center_y() <= box_rect.bottom,
                "handle is not on its line"
            );
        }
    }

    #[test]
    fn an_unticked_device_has_no_handle() {
        let (m, control) = (metrics(), tall_control());
        let field = field_with_path("audio_switch.speakers");
        let devices = vec![device("a", true, Some(0)), device("b", false, None)];
        let parts = parts(field, control, &m, &devices);
        assert_eq!(parts.boxes.len(), 2);
        assert_eq!(parts.buttons.len(), 1, "only the ticked line can be moved");
    }

    /// The `k`-th button is the handle of the `k`-th ticked line, and every
    /// handle sits inside its row's control.
    #[test]
    fn handle_indices_follow_the_ticked_order() {
        let (m, control) = (metrics(), tall_control());
        let field = field_with_path("audio_switch.speakers");
        let devices = vec![
            device("a", true, Some(0)),
            device("b", true, Some(1)),
            device("c", true, Some(2)),
        ];
        let parts = parts(field, control, &m, &devices);
        assert_eq!(parts.buttons.len(), 3);

        for (index, rect) in parts.buttons.iter().enumerate() {
            assert!(rect.width() > 0.0 && rect.height() > 0.0, "handle {index}");
            assert!(rect.right <= control.right + 0.01);
        }
        // The handles run down the list in the same order as the ticked lines.
        for pair in parts.buttons.windows(2) {
            assert!(pair[1].top > pair[0].top, "handles must follow the lines");
        }
    }

    /// Clicking the box under a line must report the device at that index,
    /// which is how the window maps a click back to an endpoint id.
    #[test]
    fn clicking_a_checkbox_reports_its_device_index() {
        let (m, control) = (metrics(), tall_control());
        let field = field_with_path("audio_switch.speakers");
        let devices = vec![device("a", false, None), device("b", false, None)];
        let parts = parts(field, control, &m, &devices);

        for (index, rect) in parts.boxes.iter().enumerate() {
            assert_eq!(
                parts.part_at(rect.center_x(), rect.center_y()),
                Some(Part::Box(index))
            );
        }
    }

    /// And pressing a handle must report its button index — the ticked
    /// position a long-press drag reorders.
    #[test]
    fn clicking_a_handle_reports_its_button_index() {
        let (m, control) = (metrics(), tall_control());
        let field = field_with_path("audio_switch.speakers");
        let devices = vec![device("a", true, Some(0)), device("b", true, Some(1))];
        let parts = parts(field, control, &m, &devices);

        for (index, rect) in parts.buttons.iter().enumerate() {
            assert_eq!(
                parts.part_at(rect.center_x(), rect.center_y()),
                Some(Part::Button(index)),
                "button {index}"
            );
        }
    }

    /// An empty machine draws its "nothing here" line and offers nothing.
    #[test]
    fn an_empty_checkbox_list_has_nothing_to_click() {
        let (m, control) = (metrics(), control());
        let field = field_with_path("audio_switch.speakers");
        let parts = parts(field, control, &m, &[]);
        assert!(parts.boxes.is_empty());
        assert!(parts.buttons.is_empty());
    }

    #[test]
    fn a_switch_is_switch_sized_and_right_aligned() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("taskbar.enabled"), control, &m, &[]);
        assert_eq!(parts.boxes.len(), 1);
        assert!((parts.boxes[0].width() - m.switch_width()).abs() < 0.01);
        assert!((parts.boxes[0].height() - m.switch_height()).abs() < 0.01);
        assert!((parts.boxes[0].right - control.right).abs() < 0.01);
    }

    #[test]
    fn a_slider_track_and_readout_do_not_overlap() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("widget.opacity"), control, &m, &[]);
        let track = parts.slider.expect("a track");
        assert!(track.right <= parts.boxes[0].left + 0.01);
        assert!(track.width() > parts.boxes[0].width());
        // The hit area is the row height, not the drawn line.
        assert!((track.height() - control.height()).abs() < 0.01);
    }

    #[test]
    fn a_colour_row_has_a_swatch_and_a_hex_box() {
        let (m, control) = (metrics(), control());
        let parts = parts(field_with_path("ui.accent"), control, &m, &[]);
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
        let parts = parts(action, control, &m, &[]);
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
        let parts = parts(field_with_path("widget.opacity"), control, &m, &[]);
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
        let parts = parts(field_with_path("widget.opacity"), control, &m, &[]);
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
            let parts = parts(field, tiny, &m, &[]);
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
        let parts = parts(field_with_path("ui.accent"), Rect::EMPTY, &metrics(), &[]);
        assert!(parts.boxes.is_empty() && parts.slider.is_none() && parts.buttons.is_empty());
    }
}
