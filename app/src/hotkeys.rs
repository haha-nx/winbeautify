//! Global hotkeys.
//!
//! `RegisterHotKey` with a null window handle posts `WM_HOTKEY` to the calling
//! *thread's* message queue, so a dedicated thread with a bare `GetMessage`
//! loop is enough — no hidden window, no subclassing. That also keeps hotkey
//! handling completely independent of whichever window happens to have focus.

use beautify_core::hotkey::{Binding, Modifier, Modifiers};
use tauri::{AppHandle, Manager};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    PeekMessageW, PostThreadMessageW, GetMessageW, TranslateMessage, DispatchMessageW, MSG,
    PM_NOREMOVE, WM_APP, WM_HOTKEY,
};

use crate::win;

/// Posted to the hotkey thread to make it quit.
const WM_APP_SHUTDOWN: u32 = WM_APP + 31;

/// What a hotkey does when it fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    OpenClipboard,
    OpenTodo,
    Snip,
    /// Pin whatever image the clipboard holds to the desktop — Snipaste's F3.
    PinClipboard,
}

impl HotkeyAction {
    const fn id(self) -> i32 {
        match self {
            HotkeyAction::OpenClipboard => 1,
            HotkeyAction::OpenTodo => 2,
            HotkeyAction::Snip => 3,
            HotkeyAction::PinClipboard => 4,
        }
    }

    fn from_id(id: i32) -> Option<Self> {
        match id {
            1 => Some(HotkeyAction::OpenClipboard),
            2 => Some(HotkeyAction::OpenTodo),
            3 => Some(HotkeyAction::Snip),
            4 => Some(HotkeyAction::PinClipboard),
            _ => None,
        }
    }
}

/// `RegisterHotKey`'s modifier flags for a binding.
///
/// The spec itself is parsed by [`beautify_core::hotkey`], which the settings
/// window also uses to *write* one; a second parser here is how a binding ends up
/// displayed as one thing and registered as another.
fn win_modifiers(modifiers: Modifiers) -> HOT_KEY_MODIFIERS {
    let mut flags = MOD_NOREPEAT.0;
    for (modifier, flag) in [
        (Modifier::Control, MOD_CONTROL.0),
        (Modifier::Alt, MOD_ALT.0),
        (Modifier::Shift, MOD_SHIFT.0),
        (Modifier::Win, MOD_WIN.0),
    ] {
        if modifiers.contains(modifier) {
            flags |= flag;
        }
    }
    HOT_KEY_MODIFIERS(flags)
}

/// Owns the hotkey thread. Dropping it unregisters everything.
pub struct HotkeyRegistry {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HotkeyRegistry {
    /// Register `bindings` and start dispatching.
    ///
    /// Returns `None` when every binding failed to parse or register — which is
    /// not an error worth surfacing: the user simply has no hotkey.
    pub fn start(bindings: Vec<(HotkeyAction, String)>, app: AppHandle) -> Option<Self> {
        let parsed: Vec<(HotkeyAction, Binding)> = bindings
            .into_iter()
            .filter_map(|(action, spec)| match beautify_core::hotkey::parse(&spec) {
                Some(binding) => Some((action, binding)),
                None => {
                    if !spec.trim().is_empty() {
                        tracing::warn!(hotkey = %spec, "could not parse hotkey; ignoring");
                    }
                    None
                }
            })
            .collect();
        if parsed.is_empty() {
            return None;
        }

        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Option<u32>>();
        let thread = std::thread::Builder::new()
            .name("wb-hotkeys".into())
            .spawn(move || {
                let thread_id = run(parsed, app, &ready_tx);
                if thread_id.is_none() {
                    tracing::warn!("hotkey thread could not start");
                }
            })
            .ok()?;

        let thread_id = ready_rx.recv().ok()??;
        Some(Self {
            thread_id,
            thread: Some(thread),
        })
    }

    pub fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            unsafe {
                let _ = PostThreadMessageW(self.thread_id, WM_APP_SHUTDOWN, WPARAM(0), LPARAM(0));
            }
            let _ = thread.join();
        }
    }
}

impl Drop for HotkeyRegistry {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    bindings: Vec<(HotkeyAction, Binding)>,
    app: AppHandle,
    ready: &std::sync::mpsc::Sender<Option<u32>>,
) -> Option<u32> {
    // Force the message queue into existence before registering: a hotkey with
    // a null window needs one to post to.
    let mut msg = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
    }
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };

    let mut registered = Vec::new();
    for (action, binding) in &bindings {
        let ok = unsafe {
            RegisterHotKey(
                None,
                action.id(),
                win_modifiers(binding.modifiers),
                binding.virtual_key,
            )
        };
        if ok.is_ok() {
            registered.push(action.id());
            tracing::info!(
                action = ?action,
                vk = binding.virtual_key,
                "global hotkey registered"
            );
        } else {
            tracing::warn!(
                action = ?action,
                "global hotkey is already taken by another application"
            );
        }
    }

    if registered.is_empty() {
        let _ = ready.send(None);
        return None;
    }
    let _ = ready.send(Some(thread_id));

    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        match msg.message {
            WM_HOTKEY => {
                if let Some(action) = HotkeyAction::from_id(msg.wParam.0 as i32) {
                    dispatch(&app, action);
                }
            }
            WM_APP_SHUTDOWN => break,
            _ => unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            },
        }
    }

    for id in registered {
        unsafe {
            let _ = UnregisterHotKey(None, id);
        }
    }
    Some(thread_id)
}

fn dispatch(app: &AppHandle, action: HotkeyAction) {
    // Neither of these is a flyout: one takes the screen over, the other puts a
    // window on the desktop.
    if action == HotkeyAction::Snip {
        if let Err(e) = crate::snip::start(app) {
            tracing::warn!("could not start a capture: {e}");
        }
        return;
    }
    if action == HotkeyAction::PinClipboard {
        match crate::snip::pin_clipboard(app) {
            Ok(size) => tracing::info!("贴图 {size}"),
            Err(e) => tracing::warn!("could not pin the clipboard image: {e}"),
        }
        return;
    }
    let tab = match action {
        HotkeyAction::OpenClipboard => beautify_core::model::FlyoutTab::Clipboard,
        HotkeyAction::OpenTodo => beautify_core::model::FlyoutTab::Todo,
        HotkeyAction::Snip | HotkeyAction::PinClipboard => unreachable!("handled above"),
    };
    // Toggle: pressing the hotkey while the panel is up should put it away. The
    // flag is the authoritative answer, because asking a window whether it is
    // visible round-trips to the main thread.
    let visible = app
        .state::<std::sync::Arc<crate::state::AppState>>()
        .flyout_visible
        .load(std::sync::atomic::Ordering::Acquire);
    if visible {
        win::hide_flyout(app);
    } else if let Err(e) = win::show_flyout(app, tab) {
        tracing::error!("could not show the flyout from a hotkey: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_ids_round_trip() {
        for action in [
            HotkeyAction::OpenClipboard,
            HotkeyAction::OpenTodo,
            HotkeyAction::Snip,
            HotkeyAction::PinClipboard,
        ] {
            assert_eq!(HotkeyAction::from_id(action.id()), Some(action));
        }
        assert_eq!(HotkeyAction::from_id(99), None);
    }

    #[test]
    fn the_modifier_flags_match_the_spec() {
        let binding = beautify_core::hotkey::parse("Ctrl+Alt+V").unwrap();
        let flags = win_modifiers(binding.modifiers);
        assert_eq!(flags.0 & MOD_CONTROL.0, MOD_CONTROL.0);
        assert_eq!(flags.0 & MOD_ALT.0, MOD_ALT.0);
        assert_eq!(flags.0 & MOD_SHIFT.0, 0);
        // Every registration carries MOD_NOREPEAT, or holding the key down
        // fires the action dozens of times.
        assert_eq!(flags.0 & MOD_NOREPEAT.0, MOD_NOREPEAT.0);
    }

    #[test]
    fn a_bare_function_key_registers_with_no_modifiers() {
        let binding = beautify_core::hotkey::parse("F1").unwrap();
        let flags = win_modifiers(binding.modifiers);
        assert_eq!(flags.0 & (MOD_CONTROL.0 | MOD_ALT.0 | MOD_SHIFT.0 | MOD_WIN.0), 0);
        assert_eq!(flags.0 & MOD_NOREPEAT.0, MOD_NOREPEAT.0);
    }
}
