//! The audio devices the settings page offers to tick.
//!
//! Two things live here, and both are deliberately free of any dependency on
//! `beautify-audio`: the crate that talks to WASAPI is a *host* concern, and
//! this crate only draws a page. The app maps its enumerated endpoints into
//! [`DeviceInfo`] and hands them over through [`Host::audio_devices`].
//!
//! [`Host::audio_devices`]: crate::window::Host::audio_devices
//!
//! The interesting part is [`resolve`], which turns "what is on the machine"
//! plus "what the user ticked" into the exact list of lines to draw. It is pure,
//! so the awkward cases — a ticked device that has been unplugged, a ticked id
//! that no longer exists at all — are covered by tests instead of by hoping.

/// One endpoint, as a row in the settings list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The WASAPI endpoint id. Stable; this is what the config stores.
    pub id: String,
    /// The name Windows shows in the Sound control panel.
    pub name: String,
    /// A short tag for the row — "耳机", "音箱", "其它".
    pub kind: String,
    /// Is this the endpoint audio currently goes to?
    pub is_default: bool,
}

/// Which of the two device lists a checkbox row edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceListKind {
    /// Playback devices.
    Speakers,
    /// Recording devices.
    Microphones,
}

impl DeviceListKind {
    pub const ALL: [DeviceListKind; 2] =
        [DeviceListKind::Speakers, DeviceListKind::Microphones];

    /// The config path holding this list, in switch order.
    pub const fn path(self) -> &'static str {
        match self {
            DeviceListKind::Speakers => "audio_switch.speakers",
            DeviceListKind::Microphones => "audio_switch.microphones",
        }
    }

    /// The word the page uses for this side.
    pub const fn label(self) -> &'static str {
        match self {
            DeviceListKind::Speakers => "播放设备",
            DeviceListKind::Microphones => "录音设备",
        }
    }

    /// The device name shown when nothing of this kind is present.
    pub const fn empty_message(self) -> &'static str {
        match self {
            DeviceListKind::Speakers => "没有找到可用的播放设备。",
            DeviceListKind::Microphones => "没有找到可用的录音设备，或本机没有麦克风。",
        }
    }

    /// The noun used in the hint under the list.
    pub const fn noun(self) -> &'static str {
        match self {
            DeviceListKind::Speakers => "扬声器",
            DeviceListKind::Microphones => "麦克风",
        }
    }
}

/// Everything the machine currently offers, in enumeration order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Devices {
    pub speakers: Vec<DeviceInfo>,
    pub microphones: Vec<DeviceInfo>,
}

impl Devices {
    /// The list for one side.
    pub fn of(&self, kind: DeviceListKind) -> &[DeviceInfo] {
        match kind {
            DeviceListKind::Speakers => &self.speakers,
            DeviceListKind::Microphones => &self.microphones,
        }
    }

    /// True when neither side has anything — used to say so once rather than
    /// drawing two empty lists.
    pub fn is_empty(&self) -> bool {
        self.speakers.is_empty() && self.microphones.is_empty()
    }
}

/// One drawn line of a checkbox list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub is_default: bool,
    /// Is this device in the config's list?
    pub ticked: bool,
    /// Position within the ticked list, for the drag handle that reorders it.
    ///
    /// `None` for an unticked device: it has no place in the switch order yet.
    pub ticked_index: Option<usize>,
}

/// Build the lines to draw for one list: ticked devices first, in the order the
/// user arranged them, then everything else in enumeration order.
///
/// Ticked-but-absent devices are **kept**, shown as 未连接. Dropping them would
/// make a device that is merely unplugged impossible to untick, and would
/// silently rewrite a preference the user is about to plug back in. The id is
/// still shown (shortened) so a stale entry can be told apart from a real one.
pub fn resolve(devices: &[DeviceInfo], ticked: &[String]) -> Vec<DeviceRow> {
    let mut rows: Vec<DeviceRow> = Vec::with_capacity(devices.len().max(ticked.len()));

    // The ticked ids, in the user's order, whether or not they are present.
    for (index, id) in ticked.iter().enumerate() {
        let found = devices.iter().find(|device| &device.id == id);
        rows.push(match found {
            Some(device) => DeviceRow {
                id: device.id.clone(),
                name: device.name.clone(),
                kind: device.kind.clone(),
                is_default: device.is_default,
                ticked: true,
                ticked_index: Some(index),
            },
            None => DeviceRow {
                id: id.clone(),
                name: "未连接".to_string(),
                // The tail of the id, which is the part that differs between
                // two entries of the same model.
                kind: short_id(id),
                is_default: false,
                ticked: true,
                ticked_index: Some(index),
            },
        });
    }

    // Everything not already listed.
    for device in devices {
        if ticked.iter().any(|id| id == &device.id) {
            continue;
        }
        rows.push(DeviceRow {
            id: device.id.clone(),
            name: device.name.clone(),
            kind: device.kind.clone(),
            is_default: device.is_default,
            ticked: false,
            ticked_index: None,
        });
    }

    rows
}

/// The last segment of an endpoint id, so a stale entry is identifiable.
fn short_id(id: &str) -> String {
    let trimmed = id.trim();
    // Endpoint ids look like `{0.0.0.00000000}.{guid}`; the guid is the part
    // worth showing.
    match trimmed.rsplit("}.").next() {
        Some(tail) if tail.len() < trimmed.len() => tail.trim_matches(['{', '}']).to_string(),
        _ => trimmed.to_string(),
    }
}

/// Move the entry at `index` one place towards the front of `list`.
///
/// Returns the new list, or `None` when the move is not possible — which is the
/// case the drag has to clamp at the ends, rather than wrap around and silently
/// reshuffle the whole order.
pub fn move_up(list: &[String], index: usize) -> Option<Vec<String>> {
    if index == 0 || index >= list.len() {
        return None;
    }
    let mut next = list.to_vec();
    next.swap(index - 1, index);
    Some(next)
}

/// Move the entry at `index` one place towards the back of `list`.
pub fn move_down(list: &[String], index: usize) -> Option<Vec<String>> {
    if index + 1 >= list.len() {
        return None;
    }
    let mut next = list.to_vec();
    next.swap(index, index + 1);
    Some(next)
}

/// Add `id` to the end of `list`, or remove it if already present.
///
/// Ticking appends rather than inserting, so the order stays "the order the
/// user ticked things" until they deliberately rearrange it — which is the same
/// rule the switch cycle walks.
pub fn toggle_selection(list: &[String], id: &str) -> Vec<String> {
    let mut next = list.to_vec();
    match next.iter().position(|existing| existing == id) {
        Some(position) => {
            next.remove(position);
        }
        None => next.push(id.to_string()),
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            kind: "音箱".to_string(),
            is_default: false,
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_list_with_nothing_ticked_shows_everything_unticked() {
        let devices = vec![device("a", "Speakers"), device("b", "Headset")];
        let rows = resolve(&devices, &[]);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| !row.ticked));
        assert!(rows.iter().all(|row| row.ticked_index.is_none()));
        // Enumeration order is preserved.
        assert_eq!(rows[0].id, "a");
        assert_eq!(rows[1].id, "b");
    }

    /// The order the user arranged must be the order drawn, and the ticked
    /// index must match it — that index is what the drag handle acts on.
    #[test]
    fn ticked_devices_come_first_in_the_configured_order() {
        let devices = vec![device("a", "A"), device("b", "B"), device("c", "C")];
        let rows = resolve(&devices, &ids(&["c", "a"]));

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "c", "the ticked order leads");
        assert_eq!(rows[0].ticked_index, Some(0));
        assert_eq!(rows[1].id, "a");
        assert_eq!(rows[1].ticked_index, Some(1));
        // `b` was never ticked, so it trails and has no place in the order.
        assert_eq!(rows[2].id, "b");
        assert!(!rows[2].ticked);
        assert_eq!(rows[2].ticked_index, None);
    }

    /// A device that has been unplugged stays on the list so it can be unticked
    /// — and it keeps its place in the switch order.
    #[test]
    fn a_ticked_device_that_is_gone_is_still_shown() {
        let devices = vec![device("a", "A")];
        let rows = resolve(&devices, &ids(&["gone", "a"]));

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "gone");
        assert!(rows[0].ticked);
        assert_eq!(rows[0].name, "未连接");
        assert_eq!(rows[0].ticked_index, Some(0), "its order is preserved");
        assert_eq!(rows[1].id, "a");
    }

    #[test]
    fn an_absent_device_still_offers_something_identifiable() {
        // Freshly generated endpoint guids, so nothing depends on the machine.
        let rows = resolve(
            &[],
            &ids(&["{0.0.0.00000000}.{aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee}"]),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert!(
            rows[0].kind.len() > 8,
            "enough of the id to tell two entries apart"
        );
    }

    #[test]
    fn an_empty_machine_yields_no_rows() {
        assert!(resolve(&[], &[]).is_empty());
    }

    /// Ticking appends; unticking removes wherever the entry was.
    #[test]
    fn toggling_appends_and_removes_in_place() {
        let list = ids(&["a", "b"]);
        assert_eq!(toggle_selection(&list, "c"), ids(&["a", "b", "c"]));
        assert_eq!(toggle_selection(&list, "a"), ids(&["b"]));
        assert_eq!(toggle_selection(&[], "z"), ids(&["z"]));
        // Toggling the same id twice returns to the start.
        let once = toggle_selection(&list, "c");
        assert_eq!(toggle_selection(&once, "c"), list);
    }

    #[test]
    fn moving_up_and_down_swaps_neighbours() {
        let list = ids(&["a", "b", "c"]);
        assert_eq!(move_up(&list, 2).unwrap(), ids(&["a", "c", "b"]));
        assert_eq!(move_down(&list, 0).unwrap(), ids(&["b", "a", "c"]));
    }

    /// The ends must refuse rather than wrap: a drag that silently reorders
    /// the whole cycle is worse than no move.
    #[test]
    fn the_ends_of_the_list_cannot_move() {
        let list = ids(&["a", "b"]);
        assert_eq!(move_up(&list, 0), None);
        assert_eq!(move_down(&list, 1), None);
        assert_eq!(move_up(&list, 9), None, "out of range is refused too");
        assert_eq!(move_down(&list, 9), None);
    }

    #[test]
    fn a_single_entry_cannot_move_at_all() {
        let list = ids(&["only"]);
        assert_eq!(move_up(&list, 0), None);
        assert_eq!(move_down(&list, 0), None);
    }

    #[test]
    fn each_side_reads_its_own_list_and_has_its_own_words() {
        let devices = Devices {
            speakers: vec![device("s", "S")],
            microphones: vec![device("m", "M")],
        };
        assert_eq!(devices.of(DeviceListKind::Speakers)[0].id, "s");
        assert_eq!(devices.of(DeviceListKind::Microphones)[0].id, "m");
        assert!(!devices.is_empty());
        assert!(Devices::default().is_empty());

        assert_ne!(
            DeviceListKind::Speakers.path(),
            DeviceListKind::Microphones.path()
        );
        let labels = [
            DeviceListKind::Speakers.label(),
            DeviceListKind::Microphones.label(),
        ];
        assert_ne!(labels[0], labels[1]);
        assert!(DeviceListKind::ALL
            .iter()
            .all(|kind| !kind.empty_message().is_empty()));
    }
}
