//! Choosing the next device and applying it.
//!
//! The cycle rule is the whole feature, and it is pure: given the ordered list
//! the user ticked and the device that is default right now, which one comes
//! next? Keeping that decision in a free function — separate from COM — is what
//! lets it be tested exhaustively, including the awkward cases (the current
//! default is not in the list at all, the list holds ids that no longer exist).

use beautify_core::config::MIN_AUDIO_DEVICES;

use crate::device::{self, Flow};
use crate::policy;

/// The modes this module drives, re-exported so callers need not reach into
/// `beautify-core`. The type itself lives there because the settings page and
/// the config file both name it.
pub use beautify_core::config::AudioSwitchMode as SwitchMode;

impl Flow {
    /// Does `mode` drive this side of the stack?
    pub const fn covered_by(self, mode: SwitchMode) -> bool {
        match self {
            Flow::Render => mode.covers_speakers(),
            Flow::Capture => mode.covers_microphones(),
        }
    }
}

/// The minimum number of ticked devices before a flow can be cycled.
///
/// One device is not a choice: switching to the device that is already default
/// would be a no-op that looked like a broken feature. The constant itself
/// lives in `beautify-core` so the settings page can show the same rule, and is
/// re-exported here for callers working in terms of flows.
pub const MIN_SELECTED: usize = MIN_AUDIO_DEVICES;

/// One side's cycle: the ordered ids the user ticked, and what is live now.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cycle {
    /// Ticked device ids, in the order the user arranged them.
    pub selected: Vec<String>,
    /// The id of the current default, when it could be read.
    pub current: Option<String>,
}

impl Cycle {
    /// Is there anything to switch *to*?
    pub fn is_usable(&self) -> bool {
        self.selected.len() >= MIN_SELECTED
    }

    /// The id that should become default next.
    ///
    /// When the current default is in the list, the next entry in order — so
    /// ticking A, B, C walks A → B → C → A. When it is *not* (the user is on a
    /// device they did not tick, or the list holds stale ids), the first entry
    /// is the sensible target: it is where the user's list begins, and it makes
    /// the first double-click deterministic instead of "somewhere".
    ///
    /// `None` when there is nothing to switch to.
    pub fn next(&self) -> Option<&str> {
        if !self.is_usable() {
            return None;
        }
        match self.current.as_deref() {
            Some(current) => {
                let position = self.selected.iter().position(|id| id == current);
                match position {
                    // Wrap around: the last ticked device leads back to the first,
                    // which is what makes repeated double-clicks cycle.
                    Some(index) => Some(&self.selected[(index + 1) % self.selected.len()]),
                    None => Some(&self.selected[0]),
                }
            }
            None => Some(&self.selected[0]),
        }
    }
}

/// What one double-click did, for the toast and the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchOutcome {
    /// The flow that was switched.
    pub flow: Flow,
    /// The device that is now default.
    pub name: String,
    /// The device that was default before, when it could be read.
    pub previous: Option<String>,
}

/// A failure worth showing the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchError {
    /// The feature is off, or nothing was ticked, or only one thing was.
    NotConfigured,
    /// The mode does not cover anything with enough devices ticked.
    NoUsableFlow,
    /// Every attempt failed; the strings are per-flow messages.
    Failed(Vec<String>),
}

impl SwitchError {
    /// One line for the toast.
    pub fn message(&self) -> String {
        match self {
            SwitchError::NotConfigured => "没有勾选足够的音频设备（至少两个）".to_string(),
            SwitchError::NoUsableFlow => "当前模式下没有勾选足够的设备".to_string(),
            SwitchError::Failed(messages) => {
                if messages.is_empty() {
                    "切换失败".to_string()
                } else {
                    messages.join("；")
                }
            }
        }
    }
}

/// Build this side's cycle from the config's ordered ids and the live default.
pub fn cycle_for(flow: Flow, selected: &[String]) -> Cycle {
    Cycle {
        selected: selected.to_vec(),
        current: device::default_endpoint_id(flow),
    }
}

/// Switch one flow, returning what it became.
///
/// The name is looked up *after* the switch so it describes what is actually
/// live, not what was intended.
pub fn switch_flow(cycle: &Cycle, flow: Flow) -> Result<SwitchOutcome, String> {
    let target = cycle
        .next()
        .ok_or_else(|| format!("{}没有勾选足够的设备", flow.label()))?;

    // A ticked device that has been unplugged since is skipped rather than
    // failing the click: the list is a preference, and hardware moves.
    let target = if device::find(flow, target).is_some() {
        target.to_string()
    } else {
        let live = device::list(flow).unwrap_or_default();
        cycle
            .selected
            .iter()
            .find(|id| live.iter().any(|device| &device.id == *id))
            .cloned()
            .ok_or_else(|| format!("勾选的{}都已不在系统中", flow.label()))?
    };

    policy::set_default_for(flow, &target)?;

    let name = device::find(flow, &target)
        .map(|device| device.name)
        .unwrap_or_else(|| target.clone());
    Ok(SwitchOutcome {
        flow,
        name,
        previous: cycle.current.clone(),
    })
}

/// Run one double-click for `mode`, given the ticked ids for each side.
///
/// Each flow is judged independently, which is the confirmed behaviour: a
/// speaker list with three devices still cycles even if only one microphone was
/// ticked, and vice versa. Only when *no* covered flow is usable is the click a
/// no-op.
pub fn switch(
    mode: SwitchMode,
    speakers: &[String],
    microphones: &[String],
) -> Result<Vec<SwitchOutcome>, SwitchError> {
    let mut outcomes = Vec::new();
    let mut failures = Vec::new();
    let mut any_usable = false;

    for flow in Flow::ALL {
        if !flow.covered_by(mode) {
            continue;
        }
        let selected = match flow {
            Flow::Render => speakers,
            Flow::Capture => microphones,
        };
        let cycle = cycle_for(flow, selected);
        if !cycle.is_usable() {
            continue;
        }
        any_usable = true;
        match switch_flow(&cycle, flow) {
            Ok(outcome) => outcomes.push(outcome),
            Err(message) => failures.push(message),
        }
    }

    if !outcomes.is_empty() {
        // Partial success is success: a failed microphone must not hide the
        // speaker that did switch. The failure is still reported by the caller
        // through the log.
        return Ok(outcomes);
    }
    if !any_usable {
        return Err(SwitchError::NoUsableFlow);
    }
    Err(SwitchError::Failed(failures))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn cycle(selected: &[&str], current: Option<&str>) -> Cycle {
        Cycle {
            selected: ids(selected),
            current: current.map(str::to_string),
        }
    }

    /// The feature is off below two devices — the rule the user stated.
    #[test]
    fn fewer_than_two_selected_devices_cannot_cycle() {
        assert!(!cycle(&[], Some("a")).is_usable());
        assert!(!cycle(&["a"], Some("a")).is_usable());
        assert_eq!(cycle(&["a"], Some("a")).next(), None);
        assert!(cycle(&["a", "b"], Some("a")).is_usable());
    }

    /// Two ticked devices toggle back and forth.
    #[test]
    fn a_pair_toggles() {
        let c = cycle(&["a", "b"], Some("a"));
        assert_eq!(c.next(), Some("b"));
        let c = cycle(&["a", "b"], Some("b"));
        assert_eq!(c.next(), Some("a"));
    }

    /// Three or more walk the list in order and wrap.
    #[test]
    fn a_list_walks_and_wraps() {
        let selected = &["a", "b", "c"];
        assert_eq!(cycle(selected, Some("a")).next(), Some("b"));
        assert_eq!(cycle(selected, Some("b")).next(), Some("c"));
        assert_eq!(cycle(selected, Some("c")).next(), Some("a"), "must wrap");
    }

    /// The order is the user's, not the enumeration's.
    #[test]
    fn the_cycle_follows_the_configured_order() {
        assert_eq!(cycle(&["c", "a", "b"], Some("c")).next(), Some("a"));
        assert_eq!(cycle(&["c", "a", "b"], Some("a")).next(), Some("b"));
        assert_eq!(cycle(&["c", "a", "b"], Some("b")).next(), Some("c"));
    }

    /// A default the user did not tick starts the cycle at the list's head,
    /// rather than at some arbitrary offset.
    #[test]
    fn an_unlisted_default_starts_at_the_first_entry() {
        assert_eq!(cycle(&["a", "b"], Some("z")).next(), Some("a"));
        assert_eq!(cycle(&["a", "b"], None).next(), Some("a"));
        // A stale current value is the same situation as an unlisted one.
        assert_eq!(cycle(&["b", "c"], Some("gone")).next(), Some("b"));
    }

    /// Every starting position must reach every other one, which is what makes
    /// repeated double-clicks a real cycle rather than getting stuck.
    #[test]
    fn repeated_clicks_visit_every_device() {
        let selected = &["a", "b", "c", "d"];
        let mut current = "a".to_string();
        let mut seen = vec![current.clone()];
        for _ in 0..selected.len() {
            let next = cycle(selected, Some(&current)).next().unwrap().to_string();
            current = next;
            seen.push(current.clone());
        }
        assert_eq!(current, "a", "a full lap must return to the start");
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), selected.len(), "every device must be visited");
    }

    #[test]
    fn each_mode_covers_the_flows_it_names() {
        assert!(Flow::Render.covered_by(SwitchMode::Speakers));
        assert!(!Flow::Capture.covered_by(SwitchMode::Speakers));
        assert!(Flow::Capture.covered_by(SwitchMode::Microphones));
        assert!(!Flow::Render.covered_by(SwitchMode::Microphones));
        assert!(Flow::Render.covered_by(SwitchMode::Both));
        assert!(Flow::Capture.covered_by(SwitchMode::Both));
    }

    /// The mode names come from the config layer, which is what the settings
    /// page and `config.toml` both speak.
    #[test]
    fn mode_ids_round_trip_through_the_config_enum() {
        for mode in SwitchMode::ALL {
            let text = format!("[audio_switch]\nmode = \"{}\"\n", mode.id());
            let parsed = beautify_core::config::Config::from_toml(&text).unwrap();
            assert_eq!(parsed.audio_switch.mode, mode, "{}", mode.id());
        }
    }

    /// A mode that covers a flow with too few devices must not be treated as
    /// usable, and a mode whose every covered flow is unusable must no-op.
    #[test]
    fn a_mode_with_no_usable_flow_reports_rather_than_switching() {
        // Speakers mode, one speaker ticked: nothing to do.
        let result = switch(SwitchMode::Speakers, &ids(&["only"]), &ids(&["m1", "m2"]));
        assert_eq!(result, Err(SwitchError::NoUsableFlow));

        // Microphone mode ignores a well-stocked speaker list.
        let result = switch(SwitchMode::Microphones, &ids(&["s1", "s2"]), &ids(&["only"]));
        assert_eq!(result, Err(SwitchError::NoUsableFlow));
    }

    #[test]
    fn the_error_message_is_something_a_person_can_read() {
        assert!(!SwitchError::NotConfigured.message().is_empty());
        assert!(!SwitchError::NoUsableFlow.message().is_empty());
        assert!(SwitchError::Failed(vec!["boom".into()])
            .message()
            .contains("boom"));
        assert_eq!(SwitchError::Failed(vec![]).message(), "切换失败");
    }
}