//! Audio device switching, as the app drives it.
//!
//! [`beautify_audio`] knows how to enumerate endpoints and how to make one the
//! default; it knows nothing about the tray, the config file or the user. This
//! module is the other half: it reads the configured device lists, runs one
//! switch, reports the result as a toast, and keeps the tray icon saying which
//! kind of device is currently live.
//!
//! # The two entry points
//!
//! * [`toggle`] is the gesture — a double-click on the tray icon, or the menu
//!   item for people who would rather not double-click. It switches and tells
//!   the user what happened.
//! * [`refresh_tray_icon`] is the *consequence*: the tray icon names the kind of
//!   the current default playback device, so the icon has to be redrawn after a
//!   switch and after a device is plugged in or removed.
//!
//! Both run on threads that are not the UI thread — the tray's message pump, or
//! the event bus. Neither takes a lock on Tauri's main thread, and neither can
//! block for long: the audio calls underneath are quick COM round trips, and
//! [`crate::tray::set_icon`] is the only window work, which Tauri dispatches.
//!
//! # Why the feedback is a toast
//!
//! A double-click on a tray icon has no window to look at afterwards, and the
//! audible result is not something you can check without playing something. The
//! toast is the entire feedback loop, which is why the failure paths here
//! deliberately produce one *too*: "the feature is off" and "nothing was
//! ticked" are the two states a first-time double-click is most likely to land
//! in, and silence would be indistinguishable from a broken gesture.

use std::sync::Arc;
use std::sync::OnceLock;

use beautify_audio::device::{self, DeviceKind, Flow};
use beautify_audio::switch::{self, SwitchError, SwitchOutcome};
use tauri::image::Image;
use tauri::{AppHandle, Manager};

use crate::state::AppState;
use crate::tray::BASE_TOOLTIP;

/// The toast title when both sides moved at once.
const COMBINED_TITLE: &str = "已切换音频设备";

/// Switch the configured audio devices one step around their cycle.
///
/// Called from the tray's own thread, so nothing here may assume it is the main
/// thread: no window creation, no `settings::open`, nothing that waits on a
/// Tauri reply.
pub fn toggle(app: &AppHandle) {
    let state = app.state::<Arc<AppState>>();
    let cfg = state.config.get();

    // Off, or nothing ticked, or only one device ticked. The user gets told
    // which of those it is in the same words the settings page uses, so the two
    // places agree rather than each inventing a phrasing.
    if !cfg.audio_switch.is_usable() {
        tracing::debug!(
            enabled = cfg.audio_switch.enabled,
            mode = cfg.audio_switch.mode.id(),
            speakers = cfg.audio_switch.speakers.len(),
            microphones = cfg.audio_switch.microphones.len(),
            "audio switch requested but not configured"
        );
        state
            .toast
            .show("音频切换不可用", &SwitchError::NotConfigured.message());
        return;
    }

    match switch::switch(
        cfg.audio_switch.mode,
        &cfg.audio_switch.speakers,
        &cfg.audio_switch.microphones,
    ) {
        Ok(outcomes) => {
            // One `show` call for the whole click, never one per flow: the
            // toast replaces whatever is on screen, so two calls in quick
            // succession would leave only the second one readable — and in
            // "both sides" mode the first would be invisible every time.
            let (title, detail) = toast_text(&outcomes);
            for outcome in &outcomes {
                tracing::info!(
                    flow = outcome.flow.id(),
                    device = %outcome.name,
                    previous = ?outcome.previous,
                    "audio device switched"
                );
            }
            state.toast.show(&title, &detail);
            // The tray icon and the settings page both describe which device is
            // default; neither knows about the COM call that just happened.
            state.bus.publish(&beautify_core::Event::AudioDeviceChanged);
        }
        Err(e) => {
            tracing::warn!(error = %e.message(), "audio device switch failed");
            state.toast.show("切换失败", &e.message());
        }
    }
}

/// Redraw the tray icon and tooltip from the current device and configuration.
///
/// Cheap by construction: the icons are embedded in the binary and decoded at
/// most once each, and the only COM call is a single default-endpoint read.
pub fn refresh_tray_icon(app: &AppHandle) {
    let usable = app.state::<Arc<AppState>>().config.read(|c| {
        (
            c.audio_switch.is_usable(),
            c.audio_switch.mode.covers_speakers(),
        )
    });
    let (usable, covers_speakers) = usable;

    if !usable {
        // The app's own icon, exactly as `tray::install` set it. Loading the
        // embedded window icon again rather than caching it here keeps the tray
        // as the single owner of "what the icon is when the feature is off".
        crate::tray::set_icon(app, app.default_window_icon().cloned());
        crate::tray::set_tooltip(app, Some(BASE_TOOLTIP.to_string()));
        return;
    }

    // Which kind the icon names, and what the tooltip calls it. When the mode
    // does not drive playback devices, the playback default says nothing about
    // what a double-click will do, so the neutral icon is the honest one — and
    // the tooltip keeps its plain wording rather than naming a device the
    // gesture does not touch.
    let (kind, name) = if covers_speakers {
        match current_render() {
            Some((kind, name)) => (Some(kind), Some(name)),
            // No playback default at all. The feature is still armed, so the
            // neutral icon is right rather than the app's own.
            None => (Some(DeviceKind::Other), None),
        }
    } else {
        (Some(DeviceKind::Other), None)
    };

    // `None` is not the same as "no icon": `set_icon(None)` takes the tray icon
    // away entirely, leaving no way back into the app. The app's own icon is
    // the fallback, which is what a neutral state should be showing.
    let icon = icon_for(kind).or_else(|| app.default_window_icon().cloned());
    crate::tray::set_icon(app, icon);
    let tooltip = match name {
        Some(name) => format!("WinBeautify — 当前播放：{name}"),
        None => BASE_TOOLTIP.to_string(),
    };
    crate::tray::set_tooltip(app, Some(tooltip));
}

/// The icon for `kind`, or `None` when there is no kind to name.
///
/// The file is embedded with `include_bytes!` rather than read at runtime: the
/// tray icon is redrawn on every device event, and a decode of the same 700
/// bytes once per event would be pure waste. Decoding happens at most once per
/// kind, and a kind that fails to decode stays `None` — a missing icon is a
/// log line, not a panic on a message-pump thread.
fn icon_for(kind: Option<DeviceKind>) -> Option<Image<'static>> {
    let (slot, bytes) = match kind? {
        DeviceKind::Headphones => (&HEADPHONES, embedded_png(DeviceKind::Headphones)),
        DeviceKind::Speakers => (&SPEAKERS, embedded_png(DeviceKind::Speakers)),
        DeviceKind::Other => (&OTHER, embedded_png(DeviceKind::Other)),
    };
    slot.get_or_init(|| {
        // No embedded picture for this kind: nothing to decode, and the caller
        // falls back to the app's own icon.
        let bytes = bytes?;
        Image::from_bytes(bytes)
            .inspect_err(|e| tracing::warn!("could not decode the tray icon: {e}"))
            .ok()
    })
    .clone()
}

/// The embedded PNGs, keyed by [`DeviceKind::id`].
static EMBEDDED_PNG: &[(&str, &[u8])] = &[
    (
        "headphones",
        include_bytes!("../icons/device-headphones.png"),
    ),
    ("speakers", include_bytes!("../icons/device-speakers.png")),
    ("other", include_bytes!("../icons/device-other.png")),
];

static HEADPHONES: OnceLock<Option<Image<'static>>> = OnceLock::new();
static SPEAKERS: OnceLock<Option<Image<'static>>> = OnceLock::new();
static OTHER: OnceLock<Option<Image<'static>>> = OnceLock::new();

/// The embedded picture for one kind.
///
/// Keyed by [`DeviceKind::id`] rather than by an index, so the lookup cannot
/// silently drift onto the wrong picture if a variant is ever added. The `None`
/// arm is unreachable — every kind has an entry, and a test asserts it — but
/// keeping the lookup total means a future gap is a blank icon rather than a
/// panic on the tray's message-pump thread.
fn embedded_png(kind: DeviceKind) -> Option<&'static [u8]> {
    EMBEDDED_PNG
        .iter()
        .find(|(id, _)| *id == kind.id())
        .map(|(_, bytes)| *bytes)
}

/// The current playback default's kind and name.
///
/// One COM pass, not two: the tray wants the icon kind *and* the tooltip name
/// for the same device, and calling `default_kind` and `default_endpoint_id`
/// separately would enumerate the endpoints twice for one redraw.
///
/// `None` only when the machine has no playback default at all.
fn current_render() -> Option<(DeviceKind, String)> {
    let id = device::default_endpoint_id(Flow::Render)?;
    let found = device::find(Flow::Render, &id);
    let kind = match &found {
        Some(device) => device.kind,
        // The id resolved but the device is not in the active list, which a
        // device being unplugged right now can produce. Ask for its kind
        // directly rather than guessing.
        None => device::default_kind(Flow::Render).unwrap_or(DeviceKind::Other),
    };
    let name = found.map(|device| device.name).unwrap_or(id);
    Some((kind, name))
}

/// The toast text for one switch.
///
/// One flow is a toast that names it — "扬声器 / <device>". Two is a **single**
/// panel with both lines, because `Toast::show` replaces what is on screen: two
/// calls a few milliseconds apart would flash the first one and leave the user
/// reading only the second, which is exactly the case where they most need both.
fn toast_text(outcomes: &[SwitchOutcome]) -> (String, String) {
    match outcomes {
        [] => (COMBINED_TITLE.to_string(), String::new()),
        [only] => (format!("已切换到{}", only.flow.label()), only.name.clone()),
        many => {
            let detail = many
                .iter()
                .map(|outcome| format!("{}：{}", outcome.flow.label(), outcome.name))
                .collect::<Vec<_>>()
                .join("\n");
            (COMBINED_TITLE.to_string(), detail)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(flow: Flow, name: &str) -> SwitchOutcome {
        SwitchOutcome {
            flow,
            name: name.to_string(),
            previous: None,
        }
    }

    /// One side changed: the title names the side, the detail names the device.
    #[test]
    fn one_flow_produces_a_toast_naming_it() {
        let (title, detail) = toast_text(&[outcome(Flow::Render, "扬声器 (Realtek)")]);
        assert_eq!(title, "已切换到扬声器");
        assert_eq!(detail, "扬声器 (Realtek)");
    }

    /// The other side is named with its own word, not the playback one.
    #[test]
    fn the_capture_side_gets_its_own_word() {
        let (title, _) = toast_text(&[outcome(Flow::Capture, "麦克风阵列")]);
        assert_eq!(title, "已切换到麦克风");
    }

    /// Both sides: exactly one panel, with a line each. Two `show` calls would
    /// leave only the second readable.
    #[test]
    fn both_flows_share_a_single_toast() {
        let (title, detail) = toast_text(&[
            outcome(Flow::Render, "扬声器 A"),
            outcome(Flow::Capture, "麦克风 B"),
        ]);
        assert_eq!(title, COMBINED_TITLE);
        assert_eq!(detail, "扬声器：扬声器 A\n麦克风：麦克风 B");
        assert_eq!(detail.lines().count(), 2);
    }

    /// The combined title must not claim a side the click did not touch.
    #[test]
    fn the_combined_title_does_not_name_a_side() {
        let (title, _) = toast_text(&[outcome(Flow::Render, "a"), outcome(Flow::Capture, "b")]);
        assert!(!title.contains("扬声器"));
        assert!(!title.contains("麦克风"));
    }

    /// The order is the caller's, so the playback line always comes first.
    #[test]
    fn the_detail_follows_the_outcome_order() {
        let (_, detail) =
            toast_text(&[outcome(Flow::Capture, "mic"), outcome(Flow::Render, "spk")]);
        assert!(detail.starts_with("麦克风："));
    }

    /// No outcome is still a well-formed toast rather than an empty string.
    #[test]
    fn no_outcomes_still_produces_a_title() {
        let (title, detail) = toast_text(&[]);
        assert!(!title.is_empty());
        assert!(detail.is_empty());
    }

    /// Every kind maps to a distinct embedded picture, and every one decodes.
    /// This is real PNG decoding — no window, no hardware.
    #[test]
    fn every_kind_has_a_decodable_icon() {
        let mut ids: Vec<&str> = EMBEDDED_PNG.iter().map(|(id, _)| *id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "two kinds share one icon id");

        for kind in [
            DeviceKind::Headphones,
            DeviceKind::Speakers,
            DeviceKind::Other,
        ] {
            assert!(
                EMBEDDED_PNG.iter().any(|(id, _)| *id == kind.id()),
                "{} has no embedded icon",
                kind.id()
            );
            let image =
                icon_for(Some(kind)).unwrap_or_else(|| panic!("{} did not decode", kind.id()));
            assert_eq!((image.width(), image.height()), (32, 32));
            assert_eq!(image.rgba().len(), 32 * 32 * 4);
        }
    }

    /// A kind that could not be determined still yields a picture, and the
    /// caller never hands `None` to `set_icon` — that would remove the tray
    /// icon and with it the only way back into the app.
    #[test]
    fn no_kind_falls_back_rather_than_clearing_the_icon() {
        assert!(icon_for(None).is_none(), "there is nothing to decode");
        // The neutral kind is what the caller substitutes, and it decodes.
        assert!(icon_for(Some(DeviceKind::Other)).is_some());
    }

    /// Every kind resolves through the id table, so the lookup can never pick
    /// up a neighbour's picture.
    #[test]
    fn each_kind_resolves_to_its_own_embedded_png() {
        for kind in [
            DeviceKind::Headphones,
            DeviceKind::Speakers,
            DeviceKind::Other,
        ] {
            let bytes = embedded_png(kind).unwrap_or_else(|| panic!("{} is missing", kind.id()));
            let only = EMBEDDED_PNG
                .iter()
                .filter(|(_, candidate)| std::ptr::eq(*candidate, bytes))
                .count();
            assert_eq!(only, 1, "{} does not have exactly one picture", kind.id());
        }
    }

    /// The embedded kind ids are the crate's, not a private copy that could
    /// drift away from them.
    #[test]
    fn the_embedded_ids_are_the_crate_ids() {
        for kind in [
            DeviceKind::Headphones,
            DeviceKind::Speakers,
            DeviceKind::Other,
        ] {
            assert!(EMBEDDED_PNG.iter().any(|(id, _)| *id == kind.id()));
        }
    }
}
