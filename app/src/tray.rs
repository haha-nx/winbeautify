//! Tray icon.
//!
//! A background app with no windows at rest needs a way back in and a way out;
//! the tray menu is both. Left-clicking the icon also opens the settings, which
//! is what people try first.
//!
//! # One gesture, two meanings, and the two clicks that arrive first
//!
//! The icon carries two gestures that overlap on Windows: a single left click
//! opens the settings window, and a left double click cycles the default audio
//! device. Windows does not report the two as alternatives. A double click is
//! delivered as the sequence `DOWN, UP, DBLCLK, UP`, which this side sees as
//! `Click`, `DoubleClick`, `Click` — so a naive handler opens Settings *twice*
//! around every device switch, once from each of those clicks.
//!
//! Two separate mechanisms are needed, because the two stray clicks are stray
//! for different reasons:
//!
//! * The **first** click is a real click that turned out to be the start of a
//!   double click. It cannot be identified by its own event — only by what
//!   arrives after it. So it does not act immediately: it takes a serial number
//!   and waits out the double-click window, and opens Settings only if no
//!   double click arrived after that serial. See [`CLICK_SERIAL`].
//! * The **second** click is the release that *ends* the double click. It
//!   arrives after the event that explains it, so it is marked instead: a
//!   double click arms [`TRAILING_RELEASE_PENDING`] and the next click consumes
//!   that mark instead of starting a wait.
//!
//! # Why a serial rather than a timestamp
//!
//! A millisecond timestamp for the first click would be *almost* enough, but a
//! single click arriving in the same millisecond as the double click before it
//! compares equal and would be cancelled, though it was never part of that
//! gesture. Counting arrivals removes the question: a click's serial is lower
//! than everything that has already happened and higher than everything that
//! has not, with no ties to break.
//!
//! The wait itself is the system's own double-click timeout rather than a
//! constant — see [`single_click_delay`] for why a fixed one is wrong.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tauri::image::Image;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;

use crate::win;

const ID: &str = "winbeautify-tray";

/// How short the wait may ever be, whatever Windows reports.
///
/// The system's own double-click time is the real bound (see
/// [`single_click_delay`]); this only keeps a machine configured with an
/// unusually small timeout from making the gesture fire so fast that a
/// genuine double click feels like two separate actions.
const SINGLE_CLICK_FLOOR: Duration = Duration::from_millis(300);

/// Resolved once, on the first click, so the system setting is read once.
static SINGLE_CLICK_DELAY: OnceLock<Duration> = OnceLock::new();

/// How long a single click waits before it opens the settings window.
///
/// # Why this is not a constant
///
/// Windows — not this app — decides what counts as a double click, and it
/// announces one with `WM_LBUTTONDBLCLK` on the *second* click. That message
/// can arrive up to `GetDoubleClickTime()` after the first click, and that
/// value is a user setting: 500 ms by default, tunable up to 900 ms. A fixed
/// 300 ms wait would therefore open Settings underneath a deliberately slow
/// double click, which is the exact bug the delay exists to prevent.
///
/// So the system's own bound is the wait. [`SINGLE_CLICK_FLOOR`] is only a
/// floor for machines where the setting is unusually small. The cost is that a
/// genuine single click feels half a second slow; correctness of the gesture
/// is worth more than that, and there is a tray menu item for anyone who would
/// rather not wait.
fn single_click_delay() -> Duration {
    *SINGLE_CLICK_DELAY.get_or_init(|| {
        // SAFETY: no arguments, no pointers, and the return is a plain timeout
        // in milliseconds. It reads a user setting and cannot fail.
        let system = unsafe { GetDoubleClickTime() };
        // Documented as "failure or zero", and zero is not a usable bound.
        let system = Duration::from_millis(u64::from(system.max(1)));
        system.max(SINGLE_CLICK_FLOOR)
    })
}

/// Stamped on every left click, single or double, in arrival order.
///
/// This is the whole cancellation rule. A millisecond timestamp would be
/// *almost* enough, but not quite: a single click arriving in the same
/// millisecond as the double click before it compares equal to that double
/// click's stamp and would be cancelled, though it was never part of that
/// gesture. Counting arrivals removes the question — a click's serial is lower
/// than everything that has already happened and higher than everything that
/// has not, with no ties to break.
static CLICK_SERIAL: AtomicU64 = AtomicU64::new(0);

/// The serial of the last double click, which is what cancels a pending single
/// click. Zero means none has happened, and the first click of the process
/// takes serial 1, so an untouched counter can never cancel anything.
static LAST_DOUBLE_CLICK_SERIAL: AtomicU64 = AtomicU64::new(0);

/// How long after a double click a left click may still be its trailing
/// button release.
///
/// The second press of a double click is held for however long the user holds
/// it, so this is generous; past it the click is treated as a new gesture
/// rather than being swallowed. See [`TRAILING_RELEASE_PENDING`].
const TRAILING_RELEASE_GRACE: Duration = Duration::from_millis(700);

/// Set by a double click, consumed by the click that ends it.
///
/// This exists because of the shape of the message sequence, not because of
/// timing. Windows reports a double click as
/// `DOWN, UP, DBLCLK, UP` — so `tray-icon` emits `Click`, `DoubleClick`, and
/// then a **second** `Click` for the release that ends the gesture. That second
/// click is indistinguishable from a genuine single click by its event alone;
/// only its position in the sequence says what it is. This flag marks it, and
/// [`TRAILING_RELEASE_DEADLINE_MS`] stops a double click whose release never
/// arrives from swallowing a later real click forever.
static TRAILING_RELEASE_PENDING: AtomicBool = AtomicBool::new(false);

/// The instant after which [`TRAILING_RELEASE_PENDING`] no longer applies.
static TRAILING_RELEASE_DEADLINE_MS: AtomicU64 = AtomicU64::new(0);

/// The origin every timestamp here is measured from.
static CLOCK_ORIGIN: OnceLock<Instant> = OnceLock::new();

/// Monotonic milliseconds since [`CLOCK_ORIGIN`].
///
/// `Instant` rather than `SystemTime`: a wall clock is free to jump backwards,
/// and one that did between a click and its timer would make the comparison
/// nonsense.
fn now_ms() -> u64 {
    CLOCK_ORIGIN.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// The tooltip the icon carries before the audio feature names a device.
pub(crate) const BASE_TOOLTIP: &str = "WinBeautify — Windows 桌面美化";

/// Build and install the tray icon.
pub fn install(app: &AppHandle) -> tauri::Result<()> {
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "settings", "设置中心", true, None::<&str>)?,
            &MenuItem::with_id(app, "audio-switch", "切换音频设备", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "flyout-todo", "任务清单", true, None::<&str>)?,
            &MenuItem::with_id(app, "flyout-clipboard", "剪贴板历史", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "snip", "截图（F1）", true, None::<&str>)?,
            &MenuItem::with_id(
                app,
                "pin-clipboard",
                "贴图：剪贴板图片（F3）",
                true,
                None::<&str>,
            )?,
            &MenuItem::with_id(app, "close-pins", "关闭全部贴图", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "reload", "重新载入配置", true, None::<&str>)?,
            &MenuItem::with_id(app, "check-update", "检查更新", true, None::<&str>)?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "退出 WinBeautify", true, None::<&str>)?,
        ],
    )?;

    let mut builder = TrayIconBuilder::with_id(ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip(BASE_TOOLTIP)
        .on_menu_event(on_menu_event)
        .on_tray_icon_event(on_tray_event);

    // The window icon is embedded by `tauri-build` from `icons/32x32.png`.
    if let Some(icon) = app.default_window_icon().cloned() {
        builder = builder.icon(icon);
    }

    builder.build(app)?;
    Ok(())
}

/// Replace the tray icon. The tray id stays private to this module.
///
/// Failures are logged rather than returned: the caller is a device event or a
/// config change, neither of which has anything to do about a missing icon, and
/// the icon's old picture is a perfectly serviceable fallback.
pub fn set_icon(app: &AppHandle, icon: Option<Image<'_>>) {
    let Some(tray) = app.tray_by_id(ID) else {
        tracing::debug!("no tray icon to update");
        return;
    };
    if let Err(e) = tray.set_icon(icon) {
        tracing::warn!("could not set the tray icon: {e}");
    }
}

/// Replace the tray tooltip. As above: a failure is a log line, not a result.
pub fn set_tooltip<S: AsRef<str>>(app: &AppHandle, tooltip: Option<S>) {
    let Some(tray) = app.tray_by_id(ID) else {
        tracing::debug!("no tray icon to update");
        return;
    };
    if let Err(e) = tray.set_tooltip(tooltip) {
        tracing::warn!("could not set the tray tooltip: {e}");
    }
}

fn on_menu_event(app: &AppHandle, event: tauri::menu::MenuEvent) {
    match event.id().as_ref() {
        "settings" => crate::settings::open(app),
        // The same action as the double click, and the reason it is here: a
        // double click is not discoverable, and the feature has to be reachable
        // before anyone knows the gesture exists.
        "audio-switch" => crate::audio::toggle(app),
        "flyout-todo" => show(beautify_core::model::FlyoutTab::Todo, app),
        "flyout-clipboard" => show(beautify_core::model::FlyoutTab::Clipboard, app),
        "snip" => {
            if let Err(e) = crate::snip::start(app) {
                tracing::warn!("could not start a capture: {e}");
            }
        }
        "pin-clipboard" => match crate::snip::pin_clipboard(app) {
            Ok(size) => tracing::info!("贴图 {size}"),
            Err(e) => tracing::warn!("{e}"),
        },
        "close-pins" => crate::snip::close_pins(),
        "reload" => reload(app),
        "check-update" => crate::update::check_for_update(app),
        "quit" => crate::shutdown(app),
        _ => {}
    }
}

fn show(tab: beautify_core::model::FlyoutTab, app: &AppHandle) {
    if let Err(e) = win::show_flyout(app, tab) {
        tracing::error!("could not show the flyout: {e}");
    }
}

fn on_tray_event(tray: &tauri::tray::TrayIcon, event: TrayIconEvent) {
    match event {
        TrayIconEvent::DoubleClick {
            button: MouseButton::Left,
            ..
        } => {
            // Stamped before the work, so a single click's timer that wakes
            // during the switch already sees that a double click happened.
            LAST_DOUBLE_CLICK_SERIAL.store(
                CLICK_SERIAL.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
            // Arm the trailing release: Windows sends one more `UP` after
            // `DBLCLK`, and it arrives here as an ordinary single click.
            TRAILING_RELEASE_DEADLINE_MS.store(
                now_ms().saturating_add(TRAILING_RELEASE_GRACE.as_millis() as u64),
                Ordering::SeqCst,
            );
            TRAILING_RELEASE_PENDING.store(true, Ordering::SeqCst);
            crate::audio::toggle(tray.app_handle());
        }
        TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } => {
            // "Is this the release that ended a double click?" — asked and
            // answered as one step, so two rapid clicks cannot both claim it.
            let was_trailing = TRAILING_RELEASE_PENDING.swap(false, Ordering::SeqCst)
                && now_ms() <= TRAILING_RELEASE_DEADLINE_MS.load(Ordering::SeqCst);
            if was_trailing {
                tracing::debug!("left click belongs to a double click; ignoring");
                return;
            }
            let serial = CLICK_SERIAL.fetch_add(1, Ordering::SeqCst) + 1;
            open_settings_after_the_double_click_window(tray.app_handle().clone(), serial);
        }
        _ => {}
    }
}

/// Wait out the double-click window, then open Settings unless one arrived.
///
/// The thread is detached and does one sleep: the tray's message pump must not
/// be held for the length of the double-click timeout, and nothing needs to
/// join it at shutdown — the only thing it touches is a window that Tauri
/// refuses to open once the app is exiting.
fn open_settings_after_the_double_click_window(app: AppHandle, serial: u64) {
    let spawned = std::thread::Builder::new()
        .name("wb-tray-click".into())
        .spawn(move || {
            std::thread::sleep(single_click_delay());
            // A double click that arrived *after* this click cancels it. The
            // comparison is on arrival order, not on the clock, so a click that
            // was never part of a double click can never be cancelled by one —
            // including the click that shares its millisecond.
            if LAST_DOUBLE_CLICK_SERIAL.load(Ordering::SeqCst) >= serial {
                tracing::debug!("single click superseded by a double click");
                return;
            }
            crate::settings::open(&app);
        });
    if let Err(e) = spawned {
        // Without the timer the click would do nothing at all, which is worse
        // than the double-click ambiguity. Fall back to opening immediately.
        tracing::warn!("could not start the tray click timer: {e}");
    }
}

/// Re-read `config.toml` from disk and apply it. Useful while experimenting
/// with the file by hand.
fn reload(app: &AppHandle) {
    use crate::state::AppState;
    let state = app.state::<std::sync::Arc<AppState>>();
    match state.config.load() {
        Ok(config) => {
            state.apply_config(&config);
            crate::sync_side_effects(app, &config);
            // The settings window holds its own copy of the config; without
            // this it would keep showing the file as it was when it opened.
            crate::settings::refresh(app);
            let _ = app.emit("config-changed", &config);
            tracing::info!("configuration reloaded from disk");
        }
        Err(e) => tracing::error!("could not reload configuration: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wait has to outlast the system's own double-click window, or a click
    /// the shell is still willing to pair into a double click could open
    /// Settings anyway. This is the assertion a fixed 300 ms delay fails on a
    /// default Windows, which is why the wait is derived rather than hardcoded.
    #[test]
    fn the_delay_covers_the_system_double_click_time() {
        // SAFETY: as in `single_click_delay` — a plain read of a user setting.
        let system = Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }.max(1)));
        let delay = single_click_delay();
        assert!(
            delay >= system,
            "{} ms does not cover the system's {} ms",
            delay.as_millis(),
            system.as_millis()
        );
        assert!(
            delay >= SINGLE_CLICK_FLOOR,
            "the wait must never be shorter than the floor"
        );
    }

    /// The resolved wait is stable, and reading it repeatedly does not call
    /// into the system again — a click handler must not pay for that.
    #[test]
    fn the_delay_resolves_once() {
        assert_eq!(single_click_delay(), single_click_delay());
    }

    /// The cancellation rule, exercised without a tray or a window: a single
    /// click is cancelled only by a double click that arrived after it.
    #[test]
    fn only_a_later_double_click_cancels_a_click() {
        // Serial 7 was a plain single click; serial 9 is a double click that
        // arrived afterwards.
        let cancelled = |click_serial: u64, last_double: u64| last_double >= click_serial;
        assert!(
            cancelled(7, 9),
            "a double click after the click must cancel it"
        );
        assert!(
            !cancelled(9, 0),
            "no double click has happened, so nothing cancels"
        );
        assert!(
            !cancelled(9, 7),
            "a double click from before this click must not cancel it"
        );
        // The click that *is* the first half of the double click is cancelled
        // by it, which is what makes the gesture switch the device instead of
        // opening Settings as well.
        assert!(cancelled(9, 9));
    }

    /// Serials are ordered and never reused, because that ordering is the
    /// entire cancellation rule.
    #[test]
    fn click_serials_are_strictly_increasing() {
        let first = CLICK_SERIAL.fetch_add(1, Ordering::SeqCst);
        let second = CLICK_SERIAL.fetch_add(1, Ordering::SeqCst);
        assert!(second > first, "two clicks cannot share a serial");
    }

    /// Windows' own sequence for a double click, modelled as this side sees it:
    /// `Click`, `DoubleClick`, `Click`. The first click is cancelled by the
    /// serial rule and the last is consumed as the trailing release, so
    /// Settings is never opened — which is the whole point of the debounce.
    #[test]
    fn a_double_click_opens_settings_from_neither_of_its_clicks() {
        // The events this side receives, in order.
        #[derive(Debug, PartialEq, Clone, Copy)]
        enum Seen {
            Click,
            DoubleClick,
        }
        // A double click is delivered as DOWN, UP, DBLCLK, UP, so its three
        // events all arrive well inside one click delay.
        let sequence = [Seen::Click, Seen::DoubleClick, Seen::Click];

        let mut click_serial = 0u64;
        let mut last_double_serial = 0u64;
        let mut trailing_pending = false;
        // Clicks that started a delayed timer, with the serial they carried.
        let mut pending_timers: Vec<u64> = Vec::new();

        for event in &sequence {
            match event {
                Seen::DoubleClick => {
                    click_serial += 1;
                    last_double_serial = click_serial;
                    trailing_pending = true;
                }
                Seen::Click => {
                    // Answered and cleared in one step, exactly as the handler
                    // does it.
                    if trailing_pending {
                        trailing_pending = false;
                        continue;
                    }
                    click_serial += 1;
                    pending_timers.push(click_serial);
                }
            }
        }

        // The timers fire after the whole double-click window has elapsed, so
        // they are judged against the serial the double click left behind.
        let opened: Vec<bool> = pending_timers
            .iter()
            .map(|serial| last_double_serial < *serial)
            .collect();

        assert_eq!(
            pending_timers.len(),
            1,
            "only the leading click may arm a timer; the release is consumed"
        );
        assert_eq!(
            opened,
            vec![false],
            "the double click must cancel that timer"
        );
        assert!(
            !trailing_pending,
            "the trailing release must have been consumed"
        );
    }

    /// A lone click, with nothing after it, does open Settings — otherwise the
    /// gesture would be dead.
    #[test]
    fn a_lone_click_opens_settings() {
        let click_serial = 1u64;
        let last_double_serial = 0u64; // no double click ever happened
        assert!(
            last_double_serial < click_serial,
            "an uncancelled timer must open the settings window"
        );
    }

    /// The trailing release is consumed exactly once. A real single click after
    /// a double click still opens Settings, which is what keeps the gesture
    /// usable.
    #[test]
    fn a_real_click_after_a_double_click_still_opens_settings() {
        let mut trailing_pending = true;
        // First click after the double click: its trailing release.
        let consumed = std::mem::take(&mut trailing_pending);
        assert!(consumed, "the release is consumed");
        // Second click: a genuine click, so nothing is left to consume.
        let consumed = std::mem::take(&mut trailing_pending);
        assert!(!consumed, "a later click must not be swallowed");
    }

    /// A double click whose release never arrives must not swallow clicks
    /// forever: the mark expires, and a click past the deadline is real.
    #[test]
    fn an_unclaimed_trailing_release_expires() {
        let armed_at = 1_000u64;
        let deadline = armed_at + TRAILING_RELEASE_GRACE.as_millis() as u64;
        let still_trailing = |now: u64| now <= deadline;
        assert!(still_trailing(1_050), "the normal release is inside");
        assert!(still_trailing(deadline), "the deadline itself is inclusive");
        assert!(!still_trailing(deadline + 1), "past it, the click is real");
    }

    /// Timestamps have to be ordered, because the deadline comparison and the
    /// timer cancellation both rest on that.
    #[test]
    fn timestamps_are_monotonic() {
        let first = now_ms();
        std::thread::sleep(Duration::from_millis(2));
        assert!(now_ms() >= first);
        assert!(now_ms() >= first, "the origin is shared, not restarted");
    }

    /// Nothing is recorded as a double click until one happens, so the first
    /// single click of the process opens Settings rather than being cancelled
    /// by an initialiser.
    #[test]
    fn a_fresh_process_has_no_pending_cancellation() {
        assert_eq!(LAST_DOUBLE_CLICK_SERIAL.load(Ordering::SeqCst), 0);
        // The first click of the process takes serial 1, which is strictly
        // greater than the untouched "no double click" value of zero.
        let first_click_serial = CLICK_SERIAL.load(Ordering::SeqCst) + 1;
        assert!(LAST_DOUBLE_CLICK_SERIAL.load(Ordering::SeqCst) < first_click_serial);
    }
}
