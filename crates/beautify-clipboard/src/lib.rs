//! Clipboard history module.
//!
//! Listens for `WM_CLIPBOARDUPDATE` on a hidden window and persists what it
//! finds. Nothing polls: the shell notifies us exactly when the clipboard
//! changes, which is the whole reason this costs nothing while idle.

pub mod capture;
pub mod ocr;
pub mod dib;
pub mod store;
pub mod writeback;

use beautify_core::config::Config;
use beautify_core::event::Event;
use beautify_core::module::{Module, ModuleContext, ModuleResult};
use beautify_core::paths;
use parking_lot::{Mutex, RwLock};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Arc;
use store::{ClipStore, NewClip};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, RemoveClipboardFormatListener,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    PostQuitMessage, RegisterClassExW, TranslateMessage, HMENU, MSG, WINDOW_EX_STYLE, WM_APP,
    WM_CLOSE, WM_CLIPBOARDUPDATE, WNDCLASSEXW, WS_POPUP,
};

const WM_APP_SHUTDOWN: u32 = WM_APP + 11;
const WM_APP_RECONFIG: u32 = WM_APP + 12;
const WINDOW_CLASS: PCWSTR = w!("WinBeautify.ClipboardHost");

/// Live settings the listener consults on each update.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Options {
    enabled: bool,
    capture_sensitive: bool,
    capture_images: bool,
    max_image_bytes: u32,
    max_entries: u32,
}

impl Options {
    fn from_config(cfg: &Config) -> Self {
        Self {
            enabled: cfg.clipboard.enabled,
            capture_sensitive: cfg.clipboard.capture_sensitive,
            capture_images: cfg.clipboard.capture_images,
            max_image_bytes: cfg.clipboard.max_image_bytes,
            max_entries: cfg.clipboard.max_entries,
        }
    }
}

struct Shared {
    options: RwLock<Options>,
    bus: RwLock<Option<beautify_core::event::EventBus>>,
    store: RwLock<Option<Arc<ClipStore>>>,
    hwnd: AtomicIsize,
    /// Content hash of the last clip *we* put on the clipboard.
    ///
    /// Restoring an entry from history would otherwise be captured straight
    /// back as a fresh copy and bounce the row to the top of the list — which
    /// is only accidentally right, and would also file every Markdown export as
    /// a clipboard entry.
    self_written: Mutex<Option<String>>,
    /// Images waiting to be recognised. A queue rather than a call into the
    /// engine, because recognition takes long enough that doing it here would
    /// stall the listener and block whichever application is copying.
    ocr_queue: Mutex<Vec<(i64, std::path::PathBuf)>>,
    /// Poked whenever something is pushed onto `ocr_queue`.
    ocr_wake: parking_lot::Condvar,
    /// Set on stop, so the recognition thread can leave its wait.
    shutdown: std::sync::atomic::AtomicBool,
}

/// Clipboard history module.
pub struct ClipboardModule {
    shared: Arc<Shared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Default for ClipboardModule {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardModule {
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Shared {
                options: RwLock::new(Options {
                    enabled: false,
                    capture_sensitive: false,
                    capture_images: true,
                    max_image_bytes: 8 * 1024 * 1024,
                    max_entries: 500,
                }),
                bus: RwLock::new(None),
                store: RwLock::new(None),
                hwnd: AtomicIsize::new(0),
                ocr_queue: Mutex::new(Vec::new()),
                ocr_wake: parking_lot::Condvar::new(),
                shutdown: std::sync::atomic::AtomicBool::new(false),
                self_written: Mutex::new(None),
            }),
            thread: Mutex::new(None),
        }
    }

    /// Put something on the clipboard ourselves.
    ///
    /// The listener will still fire — Windows always notifies — but the
    /// matching update is recognised and dropped, so restoring a clip does not
    /// re-ingest it.
    pub fn write(
        &self,
        kind: store::ClipKind,
        text: &str,
        image_path: &str,
    ) -> Result<(), writeback::WriteError> {
        let payload: &[u8] = if kind.is_textual() {
            text.as_bytes()
        } else {
            b""
        };
        *self.shared.self_written.lock() = Some(store::content_hash(kind, payload));
        match writeback::put(HWND::default(), kind, text, image_path) {
            Ok(()) => Ok(()),
            Err(e) => {
                // Nothing was written, so clear the guard rather than swallow
                // the user's next genuine copy.
                *self.shared.self_written.lock() = None;
                Err(e)
            }
        }
    }

    /// Open the store eagerly so the flyout has data even before the first copy
    /// of the session.
    pub fn open_store(&self) -> Result<Arc<ClipStore>, store::StoreError> {
        if let Some(existing) = self.shared.store.read().clone() {
            return Ok(existing);
        }
        let store = Arc::new(ClipStore::open(&paths::database_path())?);
        *self.shared.store.write() = Some(Arc::clone(&store));
        Ok(store)
    }

    /// Store shared with the UI layer, opening it on first use.
    pub fn store(&self) -> Option<Arc<ClipStore>> {
        self.shared
            .store
            .read()
            .clone()
            .or_else(|| self.open_store().ok())
    }

    fn post(&self, message: u32) {
        let raw = self.shared.hwnd.load(Ordering::Acquire);
        if raw != 0 {
            let hwnd = HWND(raw as *mut core::ffi::c_void);
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(hwnd),
                    message,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        }
    }
}

impl Module for ClipboardModule {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn is_enabled(&self, config: &Config) -> bool {
        config.clipboard.enabled
    }

    fn start(&self, ctx: ModuleContext) -> ModuleResult {
        let mut guard = self.thread.lock();
        if guard.is_some() {
            return Ok(());
        }
        *self.shared.bus.write() = Some(ctx.bus.clone());
        *self.shared.options.write() = Options::from_config(&ctx.config.get());
        // A failure to open the database must not stop the listener: the user
        // can still paste, they just lose history.
        if let Err(e) = self.open_store() {
            tracing::error!("clipboard history database unavailable: {e}");
        }

        // Anything stored while the app was closed, or before this feature
        // existed, still deserves an index entry.
        if let Some(store) = self.shared.store.read().clone() {
            match store.images_without_ocr(200) {
                Ok(pending) if !pending.is_empty() => {
                    tracing::info!(count = pending.len(), "queueing images for recognition");
                    self.shared.ocr_queue.lock().extend(
                        pending
                            .into_iter()
                            .map(|(id, path)| (id, std::path::PathBuf::from(path))),
                    );
                }
                Ok(_) => {}
                Err(e) => tracing::error!("could not list images for OCR: {e}"),
            }
        }

        let shared = Arc::clone(&self.shared);
        let ocr_shared = Arc::clone(&self.shared);
        *guard = Some(
            std::thread::Builder::new()
                .name("wb-clipboard".into())
                .spawn(move || {
                    if let Err(e) = run_pump(shared) {
                        tracing::error!("clipboard pump exited: {e}");
                    }
                })?,
        );
        std::thread::Builder::new()
            .name("wb-clipboard-ocr".into())
            .spawn(move || run_ocr(ocr_shared))?;
        Ok(())
    }

    fn apply(&self, config: &Config) -> ModuleResult {
        let next = Options::from_config(config);
        let mut guard = self.shared.options.write();
        let changed = *guard != next;
        *guard = next;
        drop(guard);
        if changed {
            self.post(WM_APP_RECONFIG);
        }
        Ok(())
    }

    fn stop(&self) -> ModuleResult {
        let Some(handle) = self.thread.lock().take() else {
            return Ok(());
        };
        self.post(WM_APP_SHUTDOWN);
        self.shared
            .shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared.ocr_wake.notify_all();
        let _ = handle.join();
        Ok(())
    }
}

impl Drop for ClipboardModule {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

thread_local! {
    static PUMP: std::cell::RefCell<Option<Arc<Shared>>> =
        const { std::cell::RefCell::new(None) };
}

fn run_pump(shared: Arc<Shared>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let instance = unsafe { GetModuleHandleW(None)? };
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wndproc),
        hInstance: instance.into(),
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    unsafe { RegisterClassExW(&class) };

    PUMP.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&shared)));

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            w!("WinBeautify Clipboard"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            Option::<HMENU>::None,
            Some(instance.into()),
            None,
        )
    }?;
    shared.hwnd.store(hwnd.0 as isize, Ordering::Release);

    let listening = unsafe { AddClipboardFormatListener(hwnd) }.is_ok();
    if !listening {
        tracing::error!("AddClipboardFormatListener failed; clipboard history is inactive");
    } else {
        tracing::debug!("clipboard listener ready (hwnd={:?})", hwnd.0);
    }

    let mut msg = MSG::default();
    loop {
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    if listening {
        unsafe {
            let _ = RemoveClipboardFormatListener(hwnd);
        }
    }
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
    shared.hwnd.store(0, Ordering::Release);
    PUMP.with(|slot| *slot.borrow_mut() = None);
    Ok(())
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_CLIPBOARDUPDATE => {
            if let Some(shared) = PUMP.with(|s| s.borrow().clone()) {
                handle_update(&shared, hwnd);
            }
            LRESULT(0)
        }
        WM_APP_RECONFIG => LRESULT(0),
        WM_APP_SHUTDOWN | WM_CLOSE => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn handle_update(shared: &Arc<Shared>, hwnd: HWND) {
    let options = *shared.options.read();
    if !options.enabled {
        return;
    }
    let Some(store) = shared.store.read().clone() else {
        return;
    };

    let max_image_bytes = if options.capture_images {
        options.max_image_bytes
    } else {
        0 // `read_image` treats 0 as "no limit", so skip images explicitly below
    };
    let Some(clip) = capture_clip(hwnd, &options, max_image_bytes) else {
        return;
    };

    // Was this the clip we just wrote back onto the clipboard ourselves?
    {
        let mut guard = shared.self_written.lock();
        if guard.as_deref() == Some(clip.hash().as_str()) {
            *guard = None;
            return;
        }
    }

    match store.insert(&clip, options.max_entries) {
        Ok(id) => {
            // Recognition is queued rather than run here: this is the clipboard
            // listener's message loop, and the copying application is waiting on
            // it. Deduplicated re-copies return an existing id with no new blob,
            // so only rows that actually have a file are queued.
            if clip.kind == store::ClipKind::Image {
                if let Some(path) = store.image_path(id).ok().flatten() {
                    shared.ocr_queue.lock().push((id, path.into()));
                    shared.ocr_wake.notify_one();
                }
            }
            if let Some(bus) = shared.bus.read().clone() {
                bus.publish(&Event::ClipboardChanged);
            }
        }
        Err(e) => tracing::error!("failed to store clipboard entry: {e}"),
    }
}

/// Recognise text inside queued images, for the rest of the process's life.
///
/// # Why this thread exists at all
///
/// `RecognizeAsync` takes tens to hundreds of milliseconds. The clipboard
/// listener has to answer `WM_CLIPBOARDUPDATE` promptly or the application doing
/// the copying blocks, so the work is parked here instead. The thread is also
/// the only one that touches the WinRT OCR engine, which is not free-threaded.
fn run_ocr(shared: Arc<Shared>) {
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
    // WinRT wants an apartment initialised on the thread that uses it.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

    if ocr::available() {
        tracing::info!(languages = ?ocr::languages(), "clipboard OCR ready");
    } else {
        tracing::warn!("no Windows OCR language pack installed; images will not be searchable");
        return;
    }

    loop {
        let next = {
            let mut queue = shared.ocr_queue.lock();
            while queue.is_empty() {
                if shared.shutdown.load(Ordering::Acquire) {
                    return;
                }
                shared.ocr_wake.wait_for(&mut queue, std::time::Duration::from_millis(500));
            }
            queue.remove(0)
        };

        let (id, path) = next;
        let Some(store) = shared.store.read().clone() else {
            continue;
        };
        let Some(recognised) = ocr::recognise_file(&path) else {
            tracing::debug!(id, "image could not be decoded for OCR");
            continue;
        };
        if recognised.is_empty() {
            tracing::debug!(id, engine = recognised.engine_available, "no text found");
            continue;
        }
        match store.set_ocr_text(id, &recognised.text) {
            Ok(()) => {
                tracing::debug!(id, chars = recognised.text.chars().count(), "recognised image text");
                if let Some(bus) = shared.bus.read().clone() {
                    bus.publish(&Event::ClipboardChanged);
                }
            }
            Err(e) => tracing::error!("could not store recognised text: {e}"),
        }
    }
}

/// Split out so the image switch is handled in one obvious place.
fn capture_clip(hwnd: HWND, options: &Options, max_image_bytes: u32) -> Option<NewClip> {
    let clip = capture::capture(hwnd, options.capture_sensitive, max_image_bytes)?;
    if clip.kind == store::ClipKind::Image && !options.capture_images {
        return None;
    }
    Some(clip)
}
