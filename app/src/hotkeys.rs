//! Global hotkeys.
//!
//! `RegisterHotKey` with a null window handle posts `WM_HOTKEY` to the calling
//! *thread's* message queue, so a dedicated thread with a bare `GetMessage`
//! loop is enough — no hidden window, no subclassing. That also keeps hotkey
//! handling completely independent of whichever window happens to have focus.

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
}

impl HotkeyAction {
    const fn id(self) -> i32 {
        match self {
            HotkeyAction::OpenClipboard => 1,
            HotkeyAction::OpenTodo => 2,
        }
    }

    fn from_id(id: i32) -> Option<Self> {
        match id {
            1 => Some(HotkeyAction::OpenClipboard),
            2 => Some(HotkeyAction::OpenTodo),
            _ => None,
        }
    }
}

/// A parsed `"Ctrl+Alt+V"` style binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Binding {
    pub modifiers: u32,
    pub virtual_key: u32,
}

impl Binding {
    pub const fn new(modifiers: u32, virtual_key: u32) -> Self {
        Self {
            modifiers,
            virtual_key,
        }
    }
}

/// Parse a human-written accelerator.
///
/// Accepts `Ctrl`/`Control`, `Alt`, `Shift`, `Win`/`Super`/`Meta` in any order
/// and case, then exactly one key. Returns `None` for anything else, including
/// bindings with no modifier — a bare letter would shadow typing everywhere.
pub fn parse(spec: &str) -> Option<Binding> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }

    let mut modifiers = 0u32;
    let mut key: Option<u32> = None;

    for part in spec.split('+').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL.0,
            "alt" => modifiers |= MOD_ALT.0,
            "shift" => modifiers |= MOD_SHIFT.0,
            "win" | "super" | "meta" | "cmd" => modifiers |= MOD_WIN.0,
            other => {
                if key.is_some() {
                    return None; // more than one non-modifier
                }
                key = Some(parse_key(other)?);
            }
        }
    }

    let virtual_key = key?;
    if modifiers == 0 {
        return None;
    }
    Some(Binding::new(modifiers, virtual_key))
}

fn parse_key(token: &str) -> Option<u32> {
    // Single character: letters and digits map straight onto their VK code.
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase() as u32);
        }
        if c.is_ascii_digit() {
            return Some(c as u32);
        }
    }
    if let Some(rest) = token.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                return Some(0x70 + n - 1); // VK_F1
            }
        }
    }
    match token {
        "space" => Some(0x20),
        "tab" => Some(0x09),
        "enter" | "return" => Some(0x0D),
        "backspace" => Some(0x08),
        "delete" | "del" => Some(0x2E),
        "insert" | "ins" => Some(0x2D),
        "home" => Some(0x24),
        "end" => Some(0x23),
        "pageup" => Some(0x21),
        "pagedown" => Some(0x22),
        "up" => Some(0x26),
        "down" => Some(0x28),
        "left" => Some(0x25),
        "right" => Some(0x27),
        "`" | "backquote" => Some(0xC0),
        "-" => Some(0xBD),
        "=" => Some(0xBB),
        "[" => Some(0xDB),
        "]" => Some(0xDD),
        "\\" => Some(0xDC),
        ";" => Some(0xBA),
        "'" => Some(0xDE),
        "," => Some(0xBC),
        "." => Some(0xBE),
        "/" => Some(0xBF),
        _ => None,
    }
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
            .filter_map(|(action, spec)| match parse(&spec) {
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
        let modifiers = HOT_KEY_MODIFIERS(binding.modifiers | MOD_NOREPEAT.0);
        let ok = unsafe { RegisterHotKey(None, action.id(), modifiers, binding.virtual_key) };
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
    let tab = match action {
        HotkeyAction::OpenClipboard => beautify_core::model::FlyoutTab::Clipboard,
        HotkeyAction::OpenTodo => beautify_core::model::FlyoutTab::Todo,
    };
    // Toggle: pressing the hotkey while the flyout is up should put it away.
    let visible = app
        .get_webview_window(win::FLYOUT)
        .and_then(|w| w.is_visible().ok())
        .unwrap_or(false);
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
    fn parses_the_documented_defaults() {
        let binding = parse("Ctrl+Alt+V").unwrap();
        assert_eq!(binding.modifiers, MOD_CONTROL.0 | MOD_ALT.0);
        assert_eq!(binding.virtual_key, 'V' as u32);

        let binding = parse("Ctrl+Alt+T").unwrap();
        assert_eq!(binding.virtual_key, 'T' as u32);
    }

    #[test]
    fn modifier_order_and_case_do_not_matter() {
        let a = parse("alt+ctrl+v").unwrap();
        let b = parse("CTRL + ALT + V").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn win_key_spellings_are_accepted() {
        for spec in ["Win+1", "Super+1", "Meta+1", "cmd+1"] {
            assert_eq!(parse(spec).unwrap().modifiers, MOD_WIN.0, "{spec}");
        }
    }

    #[test]
    fn function_and_named_keys_resolve() {
        assert_eq!(parse("Ctrl+F5").unwrap().virtual_key, 0x74);
        assert_eq!(parse("Ctrl+Shift+Space").unwrap().virtual_key, 0x20);
        assert_eq!(parse("Ctrl+PageUp").unwrap().virtual_key, 0x21);
        assert_eq!(parse("Ctrl+1").unwrap().virtual_key, '1' as u32);
    }

    #[test]
    fn a_bare_key_is_rejected_because_it_would_shadow_typing() {
        assert!(parse("V").is_none());
        assert!(parse("F5").is_none());
    }

    #[test]
    fn nonsense_is_rejected() {
        assert!(parse("").is_none());
        assert!(parse("   ").is_none());
        assert!(parse("Ctrl+").is_none());
        assert!(parse("Ctrl+Alt").is_none(), "no key");
        assert!(parse("Ctrl+V+B").is_none(), "two keys");
        assert!(parse("Ctrl+Banana").is_none());
        assert!(parse("Ctrl+F99").is_none());
    }

    #[test]
    fn action_ids_round_trip() {
        for action in [HotkeyAction::OpenClipboard, HotkeyAction::OpenTodo] {
            assert_eq!(HotkeyAction::from_id(action.id()), Some(action));
        }
        assert_eq!(HotkeyAction::from_id(99), None);
    }
}
