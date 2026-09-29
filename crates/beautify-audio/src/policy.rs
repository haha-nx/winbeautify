//! Bindings for the undocumented `IPolicyConfig` COM interface.
//!
//! WASAPI can *enumerate* endpoints all day, but it has no public way to
//! **change which one is the default**. The only route is
//! `IPolicyConfig::SetDefaultEndpoint`, the interface the Sound control panel
//! itself calls. It is declared in no public header and no Windows SDK ships an
//! import library for it, so the vtable is stated here.
//!
//! This is the same class of private API as
//! `beautify_taskbar::ffi::SetWindowCompositionAttribute`, and it is handled the
//! same way: the whole surface lives in this one module, every entry point
//! returns a `Result`/`bool` rather than panicking, and nothing outside the
//! module knows the interface exists. If a future Windows build stops honouring
//! it, the failure is a log line and the feature reports itself unavailable —
//! not a crash.
//!
//! # Two things that are easy to get wrong here
//!
//! **The IID matters, not just the CLSID.** `CPolicyConfigClient` does not
//! implement `IPolicyConfig` as its *primary* interface. Activating the coclass
//! and asking for `IUnknown` yields a vtable whose slot 13 is
//! `SetEndpointVisibility` — which takes a `BOOL`, accepts what we pass, and
//! returns `S_OK` while changing nothing. The failure is silent: the call
//! succeeds and the device never moves. `IID_IPolicyConfig` must be requested
//! explicitly, and it resolves to a genuinely different vtable.
//!
//! **Never probe for the right slot.** Calling a slot whose real signature
//! differs corrupts the stack (`STATUS_STACK_BUFFER_OVERRUN`) rather than
//! returning an error. The slot index is asserted by a test against the
//! documented layout instead of being discovered at runtime.

use windows::core::{GUID, HRESULT, Interface, PCWSTR};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, ERole,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use std::ffi::c_void;

/// `CLSID_CPolicyConfigClient` — the coclass that implements `IPolicyConfig`.
#[allow(clippy::unusual_byte_groupings)]
const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// `IID_IPolicyConfig` — the Win7+ revision.
///
/// Requested explicitly; see the module docs for why `IUnknown` is not enough.
#[allow(clippy::unusual_byte_groupings)]
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

/// The raw COM vtable, in the order `IPolicyConfig` declares it.
///
/// Only the call this crate makes is named with a real signature; the slots
/// that must be present for the *offset* to line up are `usize`-sized. That is
/// deliberate: it keeps the stride honest without pretending to know the shape
/// of calls we never issue.
#[repr(C)]
struct PolicyConfigVtbl {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    // Slots 3..=12, in declaration order:
    //   GetMixFormat, GetDeviceFormat, ResetDeviceFormat, SetDeviceFormat,
    //   GetProcessingPeriod, SetProcessingPeriod, GetShareMode, SetShareMode,
    //   GetPropertyValue, SetPropertyValue.
    // Their signatures do not matter here, only that each occupies one pointer.
    _reserved: [usize; 10],
    /// Slot 13.
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
}

/// `IUnknown::QueryInterface`, for asking for an interface the `windows` crate
/// has no binding for.
type QueryInterfaceFn =
    unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
/// `IUnknown::Release`, used to hand back the `IPolicyConfig` view.
type ReleaseFn = unsafe extern "system" fn(*mut c_void) -> u32;

/// The vtable pointer behind a COM interface pointer.
///
/// One dereference: an interface pointer points at a cell holding the vtable's
/// address, so `vtable[0]` is another step in. Getting that wrong calls into a
/// data section, which faults.
///
/// # Safety
///
/// `iface` must be a live COM interface pointer.
unsafe fn vtable_of(iface: *mut c_void) -> *const usize {
    *(iface as *const *const usize)
}

/// A reference-counted `IPolicyConfig`.
///
/// Holds the activating `IUnknown` for the object's lifetime and a separate,
/// queried `IPolicyConfig` view. Both are released, in that order, by `Drop`.
struct PolicyConfig {
    /// The object, from `CoCreateInstance`. Its `Drop` releases that reference.
    ///
    /// Never read — it is held purely so the object outlives `iface` and so its
    /// reference is released. Dropping it early would invalidate `iface`.
    #[allow(dead_code)]
    owner: windows::core::IUnknown,
    /// The `IPolicyConfig` view, released explicitly in `Drop`.
    iface: *mut c_void,
}

impl PolicyConfig {
    /// Activate the coclass and query it for `IPolicyConfig`.
    fn new() -> windows::core::Result<Self> {
        // SAFETY: `CPolicyConfigClient` is an in-proc COM class and the
        // apartment is initialised by the caller. The returned reference is
        // released by `IUnknown::drop`.
        let owner: windows::core::IUnknown =
            unsafe { CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)? };

        let raw_owner = Interface::as_raw(&owner);
        // SAFETY: `raw_owner` is a live interface pointer, so its first word is
        // the vtable and slot 0 is `QueryInterface`. `iface` is written only on
        // success, and the add-ref it takes is returned by `Drop`.
        let iface = unsafe {
            let query: QueryInterfaceFn = std::mem::transmute(*vtable_of(raw_owner));
            let mut out: *mut c_void = std::ptr::null_mut();
            query(raw_owner, &IID_POLICY_CONFIG, &mut out).ok()?;
            out
        };
        if iface.is_null() {
            return Err(windows::core::Error::from(HRESULT(0x8000_4006u32 as i32))); // E_POINTER
        }
        Ok(Self { owner, iface })
    }

    fn vtbl(&self) -> &PolicyConfigVtbl {
        // SAFETY: `self.iface` is the live interface obtained above, so its
        // first word is the vtable. `PolicyConfigVtbl` mirrors that table for
        // the slots declared above, and the table belongs to the coclass and
        // outlives this reference.
        unsafe {
            let raw = self.iface as *const *const PolicyConfigVtbl;
            &**raw
        }
    }

    /// Point `role` at the endpoint with `endpoint_id`.
    fn set_default_endpoint(&self, endpoint_id: &str, role: ERole) -> windows::core::Result<()> {
        let wide: Vec<u16> = endpoint_id.encode_utf16().chain(std::iter::once(0)).collect();
        let vtbl = self.vtbl();
        // SAFETY: `self.iface` is a live `IPolicyConfig`, `wide` is
        // NUL-terminated and outlives the call, and slot 13 is
        // `SetDefaultEndpoint` — asserted against the documented layout by a
        // test below.
        let hr = unsafe { (vtbl.set_default_endpoint)(self.iface, PCWSTR(wide.as_ptr()), role) };
        hr.ok()
    }
}

impl Drop for PolicyConfig {
    fn drop(&mut self) {
        // SAFETY: `self.iface` came from a successful `QueryInterface`, so this
        // balances that add-ref. It runs before `owner`'s `Drop`, and — because
        // the caller always declares its `ComGuard` first — before the
        // apartment is torn down. Releasing an interface after
        // `CoUninitialize` faults, which is why that ordering is documented on
        // `ComGuard`.
        unsafe {
            let release: ReleaseFn = std::mem::transmute(*vtable_of(self.iface).add(2));
            release(self.iface);
        }
    }
}

/// Every role Windows keeps a separate default for.
///
/// All three have to be set. `eConsole` alone is what most tools do, and it
/// leaves applications that asked for the *multimedia* or *communications*
/// role — which includes a lot of VoIP and media software — still pointing at
/// the old device. SoundSwitch and AudioSwitcher set all three for this reason.
const ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

/// Make `endpoint_id` the default.
///
/// The roles are set in order and the first failure is returned. A failure on
/// one role does not undo the others: having the console role switched is still
/// better than nothing, and the caller reports what happened.
pub fn set_default(endpoint_id: &str) -> Result<(), String> {
    // Self-contained: this may be called from a thread that has never touched
    // COM — the tray's message loop, say — so it takes its own apartment.
    //
    // `_com` is declared *before* `config` so `config` is released first:
    // dropping an interface after `CoUninitialize` faults. This is not a style
    // choice, and it is why neither value may be passed straight into a tail
    // expression — a temporary in tail position is dropped *after* the body's
    // locals, which is exactly the wrong order here.
    let _com = crate::com::ComGuard::new();
    let config = PolicyConfig::new().map_err(|e| format!("无法连接到音频策略服务：{e}"))?;
    for role in ROLES {
        config
            .set_default_endpoint(endpoint_id, role)
            .map_err(|e| format!("设置默认设备失败：{e}"))?;
    }
    Ok(())
}

/// Set the default for a [`crate::device::Flow`].
///
/// Both data flows share the same three roles; `flow` is taken so call sites
/// read explicitly and so a future Windows revision that splits the roles has
/// one place to change.
pub fn set_default_for(flow: crate::device::Flow, endpoint_id: &str) -> Result<(), String> {
    let _ = match flow {
        crate::device::Flow::Render => windows::Win32::Media::Audio::eRender,
        crate::device::Flow::Capture => eCapture,
    };
    set_default(endpoint_id)
}

/// Is the private interface usable on this machine?
///
/// Called so a failure can be reported as "this Windows build does not expose
/// the interface" rather than as a mysterious per-click error.
pub fn available() -> bool {
    // Bound to locals, not written as a tail expression: see `set_default`.
    let _com = crate::com::ComGuard::new();
    let config = PolicyConfig::new();
    config.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The vtable stride is the thing that silently breaks this module: one
    /// missing slot and `SetDefaultEndpoint` calls something else. Three
    /// `IUnknown` entries, ten documented get/set pairs, then
    /// `SetDefaultEndpoint` — so 14 slots, and the call sits at byte offset 13.
    #[test]
    fn the_vtable_puts_set_default_endpoint_at_slot_13() {
        unsafe extern "system" fn query(
            _: *mut c_void,
            _: *const GUID,
            _: *mut *mut c_void,
        ) -> HRESULT {
            HRESULT(0)
        }
        unsafe extern "system" fn add_ref(_: *mut c_void) -> u32 {
            1
        }
        unsafe extern "system" fn release(_: *mut c_void) -> u32 {
            0
        }
        unsafe extern "system" fn set_default(_: *mut c_void, _: PCWSTR, _: ERole) -> HRESULT {
            HRESULT(0)
        }

        assert_eq!(
            std::mem::size_of::<PolicyConfigVtbl>(),
            14 * std::mem::size_of::<usize>(),
            "the vtable must be 14 slots: 3 IUnknown + 10 get/set pairs + SetDefaultEndpoint"
        );

        let vtbl = PolicyConfigVtbl {
            query_interface: query,
            add_ref,
            release,
            _reserved: [0; 10],
            set_default_endpoint: set_default,
        };
        let base = &vtbl as *const PolicyConfigVtbl as usize;
        let slot = (&vtbl.set_default_endpoint) as *const _ as usize;
        assert_eq!(
            slot - base,
            13 * std::mem::size_of::<usize>(),
            "SetDefaultEndpoint must be the 14th slot"
        );
    }

    /// The three roles must be distinct, or one would be set twice and another
    /// left pointing at the old device.
    #[test]
    fn the_roles_are_all_distinct() {
        assert_eq!(ROLES.len(), 3);
        let mut values: Vec<i32> = ROLES.iter().map(|r| r.0).collect();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), 3, "the roles must be distinct");
    }

    /// On any real Windows box the coclass must activate and hand back
    /// `IPolicyConfig` itself.
    ///
    /// Run on a **fresh thread** deliberately. The test harness reuses threads
    /// across tests and other tests here initialise COM first; on a reused
    /// thread `CoInitializeEx` returns `S_FALSE`, the guard does not own the
    /// apartment and never calls `CoUninitialize`, so an
    /// interface-released-after-uninitialise bug is invisible. A brand-new
    /// thread has a clean apartment, which is the only place that shows up.
    #[test]
    fn the_policy_config_interface_is_obtainable_on_a_fresh_apartment() {
        let ok = std::thread::spawn(available)
            .join()
            .expect("the policy probe must not fault");
        assert!(
            ok,
            "IPolicyConfig could not be activated; switching would never work"
        );
    }

    /// The regression that cost the most to find: activating the coclass and
    /// using the `IUnknown` vtable returns `S_OK` and changes nothing, because
    /// slot 13 there is `SetEndpointVisibility`. `IPolicyConfig` must be
    /// queried for explicitly, and it is a *different* vtable.
    #[test]
    fn the_interface_we_use_is_not_the_primary_iunknown_vtable() {
        let (used, primary) = std::thread::spawn(|| {
            let _com = crate::com::ComGuard::new();
            let config = PolicyConfig::new().expect("IPolicyConfig");
            let used = unsafe { vtable_of(config.iface) } as usize;
            let primary = unsafe { vtable_of(Interface::as_raw(&config.owner)) } as usize;
            (used, primary)
        })
        .join()
        .expect("must not fault");

        assert_ne!(
            used, primary,
            "the code is using the coclass's primary vtable; slot 13 there is \
             SetEndpointVisibility, which returns S_OK and does nothing"
        );
    }

    /// Repeated activation cycles must stay reference-count balanced; a leak or
    /// a double release shows up as a fault or a hang here rather than in the
    /// field.
    #[test]
    fn repeated_activation_cycles_stay_balanced() {
        std::thread::spawn(|| {
            for _ in 0..25 {
                assert!(available());
            }
        })
        .join()
        .expect("repeated activation must not fault");
    }
}