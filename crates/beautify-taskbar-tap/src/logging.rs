//! Diagnostics that are safe to call from a COM entry point.
//!
//! The TAP's entry points run on threads it does not own: `DllGetClassObject`,
//! `CreateInstance` and `SetSite` are called by the XAML diagnostics framework
//! (on the taskbar's UI thread, which is inside a CoreMessaging/win32k callback,
//! under `KiUserCallbackDispatcher`), and the tree callback runs there too. In
//! that context neither the heap lock nor the loader lock can be assumed free,
//! so a log line that allocates, calls `GetModuleHandleExW` or opens a file can
//! wedge explorer.
//!
//! That is what an earlier version of this crate did, and it is what the
//! "`AdviseVisualTreeChange` never returns" investigation turned out to be: the
//! stall was a `format!` inside a `debug_log` called from `SetSite`, stuck in
//! `realloc` while holding the UI thread the framework was waiting on.
//!
//! The caller path therefore does three things and nothing else: format into a
//! fixed stack buffer, copy the bytes into a lock-free ring of fixed-size slots,
//! and return. A logger thread — started from the install thread, which the TAP
//! does own, before it goes anywhere near the framework — drains the ring,
//! stamps the lines, appends them to `wb_tap_debug.log` and mirrors them to
//! `OutputDebugStringW`. The latter takes a global mutex and can raise a debug
//! event against the calling thread, so it belongs there and not in a callback.
//!
//! Logging is off unless a `wb_tap_debug.flag` file sits next to the DLL, and
//! the decision is made once, when the logger starts: with logging off, a call
//! is one relaxed atomic load and a branch.

use std::cell::UnsafeCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Slots in the ring. A full ring drops lines; the caller never waits.
const SLOTS: usize = 256;

/// Bytes kept per line. Longer messages are truncated rather than allocating.
const TEXT_MAX: usize = 256;

/// Flag file, next to the DLL, that turns logging on.
pub const FLAG_FILE: &str = "wb_tap_debug.flag";

/// Log file written next to the DLL.
pub const LOG_FILE: &str = "wb_tap_debug.log";

/// One ring slot.
///
/// Producers number the lines they log with tickets. A ticket maps to the slot
/// `ticket % SLOTS`, and the slot's earlier occupants are the tickets one and
/// more laps back, so a slot only ever sees one ticket per lap. Two atomics
/// publish what happened to a slot's ticket:
///
/// * `published` — `ticket + 1` once the ticket's text is in the slot. The
///   logger thread reads the text only after loading this value with `Acquire`.
/// * `dropped` — `ticket + 1` when the ticket's line was dropped because the
///   logger thread had fallen a full lap behind. Without it the logger thread
///   would sit waiting for a line that is never coming, and logging would stop
///   for good; with it, the thread skips the ticket and says so in the log.
///
/// `dropped` never touches the text, which is what makes a producer that is
/// running laps ahead of the reader safe: overwriting the text of a line the
/// reader is copying is not possible, because a producer only writes text when
/// the reader is less than a lap behind.
struct Slot {
    published: AtomicU64,
    dropped: AtomicU64,
    len: AtomicUsize,
    text: UnsafeCell<[u8; TEXT_MAX]>,
}

// SAFETY: `text` is written only by the producer that owns the slot's current
// lap and read only by the logger thread, and only after the releasing
// `published` store made it visible. `push` refuses to write when the reader is
// a full lap behind, which is exactly the case where the reader might still be
// inside this slot.
unsafe impl Sync for Slot {}

impl Slot {
    const fn new() -> Self {
        Self {
            published: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            len: AtomicUsize::new(0),
            text: UnsafeCell::new([0; TEXT_MAX]),
        }
    }
}

static RING: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
/// Ticket the next producer takes.
static WRITE_TICKET: AtomicU64 = AtomicU64::new(0);
/// Ticket the logger thread will read next. The only writer is the logger
/// thread; producers read it to decide whether they still have room.
static READ_TICKET: AtomicU64 = AtomicU64::new(0);
/// Set once, by [`start`], when the flag file is present.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Log a fixed message. Allocation-free and lock-free.
pub fn debug_log(message: &str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    push(message.as_bytes());
}

/// Log a message with data in it. The formatting goes into a stack buffer, so
/// unlike `format!` this does not touch the heap.
pub fn debug_log_fmt(args: core::fmt::Arguments) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    let mut buffer = StackBuffer::new();
    // Running out of room in a fixed buffer is not worth reporting: saying so
    // would need a second buffer.
    let _ = core::fmt::Write::write_fmt(&mut buffer, args);
    push(buffer.filled());
}

/// Resolve where the log goes and, if the flag file is present, start the logger
/// thread. Call this from a thread the TAP owns (the install thread) before it
/// connects to the framework: from then on every COM entry point is allocation-
/// and IO-free, and anything logged before this call is dropped.
pub fn start() {
    let Some(dir) = module_dir() else {
        return;
    };
    // The only filesystem call on the setup path, made exactly once.
    if !dir.join(FLAG_FILE).exists() {
        return;
    }
    let path = dir.join(LOG_FILE);
    ENABLED.store(true, Ordering::Release);
    let _ = std::thread::Builder::new()
        .name("wb-tap-log".into())
        .spawn(move || logger_loop(path));
}

// ---------------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------------

/// Publish `text` for the next ticket, or drop it if the logger thread has
/// fallen a full lap behind.
fn push(text: &[u8]) {
    let ticket = WRITE_TICKET.fetch_add(1, Ordering::Relaxed);
    let slot = &RING[(ticket % SLOTS as u64) as usize];
    // A lap behind means the reader may still be inside this slot: it gets no
    // text from us, only the news that its own ticket is never arriving.
    if ticket.wrapping_sub(READ_TICKET.load(Ordering::Acquire)) >= SLOTS as u64 {
        slot.dropped.fetch_max(ticket.wrapping_add(1), Ordering::Release);
        return;
    }
    let len = text.len().min(TEXT_MAX);
    // SAFETY: the check above establishes that the reader is not inside this
    // slot, and nothing else reads or writes it before the releasing store
    // below publishes it.
    unsafe {
        core::ptr::copy_nonoverlapping(text.as_ptr(), slot.text.get().cast::<u8>(), len);
    }
    slot.len.store(len, Ordering::Relaxed);
    slot.published.store(ticket.wrapping_add(1), Ordering::Release);
}

// ---------------------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------------------

/// What the logger thread found for its current ticket.
enum Taken {
    /// `usize` bytes of the line were copied into the caller's buffer.
    Line(usize),
    /// The producer dropped this ticket; nothing was copied.
    Dropped,
    /// Nothing to read yet.
    Empty,
}

/// Read the line published for `*ticket` and advance the ticket.
fn take(ticket: &mut u64, out: &mut [u8; TEXT_MAX]) -> Taken {
    let slot = &RING[(*ticket % SLOTS as u64) as usize];
    let published = slot.published.load(Ordering::Acquire);
    let taken = if published == ticket.wrapping_add(1) {
        // SAFETY: the producer's releasing store published this text for our
        // ticket, so it is fully written and no producer will touch the slot
        // again until we advance `READ_TICKET` below.
        let len = slot.len.load(Ordering::Relaxed).min(TEXT_MAX);
        unsafe {
            core::ptr::copy_nonoverlapping(slot.text.get().cast::<u8>(), out.as_mut_ptr(), len);
        }
        Taken::Line(len)
    } else if slot.dropped.load(Ordering::Acquire) > *ticket {
        // A later ticket in this slot was dropped, and the producer only ever
        // writes that after giving up on a ticket this reader is still waiting
        // for. Our line is not coming.
        Taken::Dropped
    } else {
        Taken::Empty
    };
    if !matches!(taken, Taken::Empty) {
        *ticket = ticket.wrapping_add(1);
        READ_TICKET.store(*ticket, Ordering::Release);
    }
    taken
}

fn logger_loop(path: PathBuf) {
    let mut ticket = 0u64;
    let mut line = [0u8; TEXT_MAX];
    let mut dropped = 0u64;
    loop {
        match take(&mut ticket, &mut line) {
            Taken::Line(len) => {
                report_drops(&mut dropped, &path);
                emit(&line[..len], &path);
            }
            Taken::Dropped => dropped += 1,
            Taken::Empty => {
                report_drops(&mut dropped, &path);
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}

/// Flush the count of lines that the ring dropped, so a gap in the log is
/// visible rather than silent.
fn report_drops(dropped: &mut u64, path: &Path) {
    if *dropped == 0 {
        return;
    }
    let mut buffer = StackBuffer::new();
    let _ = core::fmt::Write::write_fmt(
        &mut buffer,
        format_args!("<{} log lines dropped: logger fell behind>", *dropped),
    );
    emit(buffer.filled(), path);
    *dropped = 0;
}

/// Write one line to the debugger and to the log file. Only the logger thread
/// runs this, so the allocations and the file IO here are off the caller path by
/// construction.
fn emit(text: &[u8], path: &Path) {
    unsafe {
        use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
        let mut wide = [0u16; TEXT_MAX + 16];
        let mut len = 0;
        for unit in "wb-tap: ".encode_utf16() {
            wide[len] = unit;
            len += 1;
        }
        for unit in String::from_utf8_lossy(text).encode_utf16() {
            if len + 1 >= wide.len() {
                break;
            }
            wide[len] = unit;
            len += 1;
        }
        wide[len] = 0;
        OutputDebugStringW(windows::core::PCWSTR(wide.as_ptr()));
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        use std::io::Write;
        let _ = writeln!(file, "{stamp} {}", String::from_utf8_lossy(text));
    }
}

/// Directory of this DLL, resolved from our own code address. This takes the
/// loader lock, which is why it only ever runs from [`start`].
fn module_dir() -> Option<PathBuf> {
    const FROM_ADDRESS: u32 = windows::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
        | windows::Win32::System::LibraryLoader::GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    let mut handle = windows::Win32::Foundation::HMODULE::default();
    unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleExW(
            FROM_ADDRESS,
            windows::core::PCWSTR(module_dir as *const u16),
            &mut handle,
        )
        .ok()?;
        let mut buffer = [0u16; 512];
        let len =
            windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(handle), &mut buffer);
        if len == 0 || (len as usize) >= buffer.len() {
            return None;
        }
        PathBuf::from(String::from_utf16_lossy(&buffer[..len as usize]))
            .parent()
            .map(Path::to_path_buf)
    }
}

/// A `core::fmt::Write` sink over a fixed stack array.
struct StackBuffer {
    bytes: [u8; TEXT_MAX],
    len: usize,
}

impl StackBuffer {
    fn new() -> Self {
        Self {
            bytes: [0; TEXT_MAX],
            len: 0,
        }
    }

    fn filled(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl core::fmt::Write for StackBuffer {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let take = s.len().min(TEXT_MAX - self.len);
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ring is a process-wide static and a test binary runs tests in
    /// parallel, so everything that touches it lives in this one test.
    #[test]
    fn ring_publishes_in_order_then_reports_the_drops_it_has_to_make() {
        let mut ticket = 0;
        let mut out = [0u8; TEXT_MAX];

        push(b"first");
        push(b"second");
        assert!(matches!(take(&mut ticket, &mut out), Taken::Line(5)));
        assert_eq!(&out[..5], b"first");
        assert!(matches!(take(&mut ticket, &mut out), Taken::Line(6)));
        assert_eq!(&out[..6], b"second");
        assert!(matches!(take(&mut ticket, &mut out), Taken::Empty));

        // Push a lap and a bit without draining: the lines that fit are kept, the
        // rest are dropped — and the drops are visible, so the reader does not
        // wait forever for a line that is never coming.
        for _ in 0..SLOTS + 3 {
            push(b"x");
        }
        let mut lines = 0;
        let mut drops = 0;
        loop {
            match take(&mut ticket, &mut out) {
                Taken::Line(_) => lines += 1,
                Taken::Dropped => drops += 1,
                Taken::Empty => break,
            }
        }
        assert_eq!(lines, SLOTS);
        assert_eq!(drops, 3);

        // And the reader has resynchronised: the next line is read normally.
        push(b"third");
        assert!(matches!(take(&mut ticket, &mut out), Taken::Line(5)));
        assert_eq!(&out[..5], b"third");
    }

    #[test]
    fn formatting_stops_at_the_end_of_the_stack_buffer() {
        let mut buffer = StackBuffer::new();
        let long = "x".repeat(TEXT_MAX * 2);
        let _ = core::fmt::Write::write_fmt(&mut buffer, format_args!("{long}"));
        assert_eq!(buffer.filled().len(), TEXT_MAX);
    }
}
