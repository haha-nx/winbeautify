//! The wire protocol between the WinBeautify process and the TAP living inside
//! explorer.exe.
//!
//! Commands travel as one `WM_COPYDATA` payload to a message-only window the
//! TAP creates on the taskbar's XAML UI thread. `SendMessage` gives us both
//! synchronous execution on that thread — required to touch XAML objects — and
//! a return value saying whether the command was applied.

/// Bump when [`TapCommand`] changes layout or meaning. A TAP that sees a
/// version it does not know rejects the command instead of misreading it.
pub const PROTOCOL_VERSION: u32 = 1;

/// `WM_COPYDATA.dwData` marker; a TAP ignores copies without it.
pub const COPYDATA_MAGIC: u32 = 0x5F50_5442; // 'BTP_'

/// Message-only window class the TAP registers on the XAML UI thread.
pub const TAP_WINDOW_CLASS: &str = "WinBeautify.TapWindow";

/// Manual-reset event the host waits on after forcing the DLL load. The TAP
/// signals it when the XAML diagnostics connection is up — or failed, so the
/// host never waits longer than it has to.
pub const READY_EVENT_NAME: &str = "WinBeautify.Tap.Ready";

/// Name of the cdylib built from this crate, as the host looks it up next to
/// its own executable.
pub const DLL_FILE_NAME: &str = "beautify_taskbar_tap.dll";

/// Exported hook placeholder the host points `SetWindowsHookEx` at; its only
/// real job is getting this DLL mapped into explorer.exe.
pub const HOOK_PROC_NAME: &str = "tap_hook_proc";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum CommandKind {
    /// Re-apply the default XAML fills of one taskbar.
    Restore = 2,
    /// Re-apply the default XAML fills of every taskbar.
    RestoreAll = 3,
    /// Paint one taskbar with the requested brush.
    Set = 1,
    /// Show or hide the hairline along the taskbar's top edge.
    ///
    /// `argb` carries the flag rather than a colour: a non-zero value restores
    /// the shell's own brush and zero clears the fill. Added to an existing
    /// protocol without bumping the version on purpose — the struct layout is
    /// unchanged, and a TAP from a previous run answers an unknown command with
    /// "not applied" instead of misreading it, where a version bump would make
    /// it reject *every* command until explorer restarts.
    SetHairline = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum BrushKind {
    /// Opaque or translucent tint, no blur.
    Solid = 0,
    /// XAML `AcrylicBrush` with a backdrop source.
    Acrylic = 1,
    /// Backdrop blur + tint flood composited on the GPU.
    Blur = 2,
}

/// One command, copied verbatim across the process boundary.
///
/// `argb` is `0xAARRGGBB` (alpha in the high byte), unlike the Win32
/// `COLORREF` layout the legacy accent path uses. `taskbar` is the top-level
/// `Shell_TrayWnd`/`Shell_SecondaryTrayWnd` the command addresses; the TAP
/// resolves it to a XAML island by parent lookup.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct TapCommand {
    pub version: u32,
    pub command: u32,
    pub taskbar: usize,
    pub brush: u32,
    pub argb: u32,
    pub blur_amount: f32,
    pub worker_pid: u32,
}

impl TapCommand {
    pub fn new(command: CommandKind) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            command: command as u32,
            taskbar: 0,
            brush: 0,
            argb: 0,
            blur_amount: 0.0,
            worker_pid: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_a_fixed_layout_of_scalars() {
        // WM_COPYDATA marshals this struct byte for byte; padding surprises
        // would corrupt the command on the other side. On x64 the HWND-sized
        // `taskbar` field makes the struct 32 bytes with 8-byte alignment.
        assert_eq!(std::mem::size_of::<TapCommand>(), 32);
        assert_eq!(std::mem::align_of::<TapCommand>(), 8);
        assert_eq!(std::mem::offset_of!(TapCommand, taskbar), 8);
        assert_eq!(std::mem::offset_of!(TapCommand, worker_pid), 28);
    }

    #[test]
    fn set_command_fills_every_field() {
        let mut cmd = TapCommand::new(CommandKind::Set);
        cmd.taskbar = 0x1234;
        cmd.brush = BrushKind::Acrylic as u32;
        cmd.argb = 0x8010_2030;
        cmd.blur_amount = 3.0;
        cmd.worker_pid = 42;

        assert_eq!(cmd.version, PROTOCOL_VERSION);
        assert_eq!(cmd.command, CommandKind::Set as u32);
        assert_eq!(cmd.brush, BrushKind::Acrylic as u32);
    }

    /// The hairline rides in `argb` of an existing command rather than in a new
    /// field, which is what lets it reach a TAP that was injected before this
    /// version of the host existed.
    #[test]
    fn the_hairline_command_needs_no_extra_field() {
        let mut cmd = TapCommand::new(CommandKind::SetHairline);
        cmd.taskbar = 0x1234;
        cmd.argb = 1; // show; 0 would hide
        assert_eq!(cmd.command, 4);
        assert_eq!(std::mem::size_of::<TapCommand>(), 32);
    }
}
