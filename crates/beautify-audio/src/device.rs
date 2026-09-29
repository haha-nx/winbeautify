//! Audio endpoint enumeration.
//!
//! Everything here reads the *current* state of the machine: which render and
//! capture endpoints are active, what they are called, and which one is
//! currently the default. Nothing is cached, because a headset plugged in a
//! second ago has to show up in the settings list without a restart.
//!
//! Devices are identified by their **endpoint ID string** — the `{0.0.0.00000000}.{guid}`
//! form WASAPI hands back from `IMMDevice::GetId`. That string survives reboots
//! and re-plugs, which the enumeration index does not, so it is what the config
//! stores and what a switch looks up.

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, EndpointFormFactor, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, DEVICE_STATE_ACTIVE, Headphones, Headset, Speakers,
    PKEY_AudioEndpoint_FormFactor,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::{VT_LPWSTR, VT_UI4};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

/// Which side of the audio stack an operation applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    /// Playback devices — speakers, headphones.
    Render,
    /// Recording devices — microphones.
    Capture,
}

impl Flow {
    pub const ALL: [Flow; 2] = [Flow::Render, Flow::Capture];

    /// The `EDataFlow` value WASAPI wants.
    pub(crate) const fn data_flow(self) -> windows::Win32::Media::Audio::EDataFlow {
        match self {
            Flow::Render => eRender,
            Flow::Capture => eCapture,
        }
    }

    /// Stable identifier, also used as a config key.
    pub const fn id(self) -> &'static str {
        match self {
            Flow::Render => "render",
            Flow::Capture => "capture",
        }
    }

    /// The word the UI uses for this side.
    pub const fn label(self) -> &'static str {
        match self {
            Flow::Render => "扬声器",
            Flow::Capture => "麦克风",
        }
    }
}

/// What kind of physical thing an endpoint is.
///
/// Drives which tray icon is shown, so the three cases are the three icons the
/// app ships. Anything Windows does not label — virtual cables, uncategorised
/// HDMI outputs — is `Other` rather than being guessed at from its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    /// Headphones, a headset, or anything shaped like one.
    Headphones,
    /// Speakers, including a monitor's built-in ones.
    Speakers,
    /// Not identifiable, or not either of the above.
    Other,
}

impl DeviceKind {
    /// Classify from the endpoint's form factor.
    pub fn from_form_factor(form: EndpointFormFactor) -> Self {
        if form == Headphones || form == Headset {
            DeviceKind::Headphones
        } else if form == Speakers {
            DeviceKind::Speakers
        } else {
            DeviceKind::Other
        }
    }

    pub const fn id(self) -> &'static str {
        match self {
            DeviceKind::Headphones => "headphones",
            DeviceKind::Speakers => "speakers",
            DeviceKind::Other => "other",
        }
    }

    /// The word the settings list uses for this kind.
    pub const fn label(self) -> &'static str {
        match self {
            DeviceKind::Headphones => "耳机",
            DeviceKind::Speakers => "音箱",
            DeviceKind::Other => "其它",
        }
    }
}

/// One endpoint, as the settings page and the switcher see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioDevice {
    /// The WASAPI endpoint ID. Stable; this is what the config stores.
    pub id: String,
    /// The name Windows shows in the Sound control panel.
    pub name: String,
    /// What kind of thing it is, for the icon.
    pub kind: DeviceKind,
    /// Is this the endpoint audio currently goes to?
    pub is_default: bool,
}

/// RAII guard for this thread's COM apartment.
///
/// `CoInitializeEx` is per-thread and reference-counted. A thread that already
/// initialised COM — the widget pump, or a caller further up the stack — gets
/// `S_FALSE` (same mode) or `RPC_E_CHANGED_MODE` (another mode). Neither is a
/// failure here, but only a successful call may be balanced by
/// `CoUninitialize`, which is what `should_uninit` tracks.
struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn new() -> Self {
        // SAFETY: no pointers are involved; the call only affects this thread's
        // apartment. The result is deliberately not fatal — an
        // already-initialised thread is the common case.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            should_uninit: hr.is_ok(),
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            // SAFETY: balanced against the successful `CoInitializeEx` above,
            // and this guard owns that call.
            unsafe { CoUninitialize() };
        }
    }
}

/// Read a string property, if it really is one.
fn store_string(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: `store` is a live interface. `GetValue` hands back a PROPVARIANT
    // this call owns; the `VT_LPWSTR` arm points at memory the property store
    // owns and keeps alive for as long as the store does, so it is copied out
    // here rather than freed. The variant itself is released by
    // `PropVariantClear` once the copy is made.
    unsafe {
        let mut value = store.GetValue(key).ok()?;
        let text = if value.Anonymous.Anonymous.vt == VT_LPWSTR {
            let raw = value.Anonymous.Anonymous.Anonymous.pwszVal;
            if raw.is_null() {
                None
            } else {
                raw.to_string().ok()
            }
        } else {
            None
        };
        // Releases whatever the variant owns. The string was already copied, so
        // this cannot invalidate `text`.
        let _ = PropVariantClear(&mut value);
        text
    }
}

/// Read a `u32` property, if it really is one.
fn store_u32(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<u32> {
    // SAFETY: as above. The `VT_UI4` arm is a plain integer and owns nothing,
    // but clearing it is still the correct way to end the variant's life.
    unsafe {
        let mut value = store.GetValue(key).ok()?;
        let number = if value.Anonymous.Anonymous.vt == VT_UI4 {
            Some(value.Anonymous.Anonymous.Anonymous.ulVal)
        } else {
            None
        };
        let _ = PropVariantClear(&mut value);
        number
    }
}

/// The endpoint ID of a device.
fn device_id(device: &IMMDevice) -> Option<String> {
    // SAFETY: `GetId` hands back a `PWSTR` allocated with the COM task
    // allocator. It is copied into a `String` and released with
    // `CoTaskMemFree` before returning, so nothing leaks either way.
    unsafe {
        let raw = device.GetId().ok()?;
        let text = raw.to_string().ok();
        CoTaskMemFree(Some(raw.0 as *const core::ffi::c_void));
        text
    }
}

/// Everything the enumerator can tell us about one endpoint.
fn describe(device: &IMMDevice) -> Option<AudioDevice> {
    let id = device_id(device)?;
    // A device with no readable property store is still usable — it gets a
    // placeholder name rather than being dropped from the list, which would
    // make it impossible to select.
    // SAFETY: `device` is a live interface for the duration of the call.
    let (name, kind) = match unsafe { device.OpenPropertyStore(STGM_READ) } {
        Ok(store) => {
            let name = store_string(&store, &PKEY_Device_FriendlyName)
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| "未命名设备".to_string());
            let kind = store_u32(&store, &PKEY_AudioEndpoint_FormFactor)
                .map(|raw| DeviceKind::from_form_factor(EndpointFormFactor(raw as i32)))
                .unwrap_or(DeviceKind::Other);
            (name, kind)
        }
        Err(_) => ("未命名设备".to_string(), DeviceKind::Other),
    };
    Some(AudioDevice {
        id,
        name,
        kind,
        is_default: false,
    })
}

/// The shared enumerator.
fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    // SAFETY: `MMDeviceEnumerator` is a documented in-proc COM class, and the
    // caller has initialised this thread's apartment.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// The endpoint ID of the current default for `flow`, if there is one.
fn default_endpoint_id_with(enumerator: &IMMDeviceEnumerator, flow: Flow) -> Option<String> {
    // SAFETY: `eConsole` is the role the shell's own volume UI follows, which is
    // what a user means by "the default device".
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(flow.data_flow(), eConsole) }.ok()?;
    device_id(&device)
}

/// List the active endpoints on one side of the stack, marking the default.
///
/// Returns an empty list rather than an error when the machine genuinely has no
/// such device: a desktop with no microphone is normal, not a failure.
pub fn list(flow: Flow) -> windows::core::Result<Vec<AudioDevice>> {
    let _com = ComGuard::new();
    let enumerator = enumerator()?;

    // SAFETY: `EnumAudioEndpoints` returns a live collection owned by the
    // enumerator, and `Item` is only called below the count just read.
    let collection =
        unsafe { enumerator.EnumAudioEndpoints(flow.data_flow(), DEVICE_STATE_ACTIVE)? };
    let count = unsafe { collection.GetCount()? };
    let default_id = default_endpoint_id_with(&enumerator, flow);

    let mut devices = Vec::with_capacity(count as usize);
    for index in 0..count {
        let Ok(device) = (unsafe { collection.Item(index) }) else {
            continue;
        };
        let Some(mut info) = describe(&device) else {
            continue;
        };
        info.is_default = default_id.as_deref() == Some(info.id.as_str());
        devices.push(info);
    }
    Ok(devices)
}

/// The endpoint ID of the current default for `flow`.
pub fn default_endpoint_id(flow: Flow) -> Option<String> {
    let _com = ComGuard::new();
    let enumerator = enumerator().ok()?;
    default_endpoint_id_with(&enumerator, flow)
}

/// The kind of the current default on `flow`, for the tray icon.
pub fn default_kind(flow: Flow) -> Option<DeviceKind> {
    let _com = ComGuard::new();
    let enumerator = enumerator().ok()?;
    // SAFETY: as in `default_endpoint_id_with`.
    let device = unsafe { enumerator.GetDefaultAudioEndpoint(flow.data_flow(), eConsole) }.ok()?;
    // SAFETY: `device` is live for the duration of the call.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }.ok()?;
    store_u32(&store, &PKEY_AudioEndpoint_FormFactor)
        .map(|raw| DeviceKind::from_form_factor(EndpointFormFactor(raw as i32)))
}

/// Find the device with `id` on `flow`.
pub fn find(flow: Flow, id: &str) -> Option<AudioDevice> {
    list(flow).ok()?.into_iter().find(|device| device.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_factors_map_to_the_three_icon_kinds() {
        assert_eq!(
            DeviceKind::from_form_factor(Headphones),
            DeviceKind::Headphones
        );
        assert_eq!(DeviceKind::from_form_factor(Headset), DeviceKind::Headphones);
        assert_eq!(DeviceKind::from_form_factor(Speakers), DeviceKind::Speakers);
        // Everything else is Other rather than guessed at.
        assert_eq!(
            DeviceKind::from_form_factor(EndpointFormFactor(0)),
            DeviceKind::Other
        );
        assert_eq!(
            DeviceKind::from_form_factor(EndpointFormFactor(99)),
            DeviceKind::Other
        );
    }

    #[test]
    fn every_kind_and_flow_has_a_distinct_id() {
        let kinds = [
            DeviceKind::Headphones.id(),
            DeviceKind::Speakers.id(),
            DeviceKind::Other.id(),
        ];
        for (index, kind) in kinds.iter().enumerate() {
            assert!(
                !kinds[index + 1..].contains(kind),
                "{kind} is used twice, so its icon would be ambiguous"
            );
        }
        assert_ne!(Flow::Render.id(), Flow::Capture.id());
        assert_ne!(Flow::Render.label(), Flow::Capture.label());
    }

    /// The enumerator has to work on a real machine. This is the one test that
    /// touches hardware, so it asserts only what holds on *any* Windows box:
    /// the call succeeds, and whatever it reports is self-consistent.
    #[test]
    fn listing_a_flow_succeeds_and_agrees_with_itself() {
        for flow in Flow::ALL {
            let devices = list(flow).expect("enumeration must not fail");
            let defaults = devices.iter().filter(|d| d.is_default).count();
            assert!(
                defaults <= 1,
                "{flow:?} reported {defaults} defaults; there can only be one"
            );
            for device in &devices {
                assert!(!device.id.is_empty(), "an endpoint must have an id");
                assert!(!device.name.is_empty(), "a device must have a name to show");
            }
            // Asking for the default directly must agree with the flag.
            let direct = default_endpoint_id(flow);
            match devices.iter().find(|d| d.is_default) {
                Some(marked) => assert_eq!(Some(marked.id.clone()), direct),
                None => assert_eq!(direct, None),
            }
        }
    }

    /// `find` must locate a device the list just reported, by the id it gave.
    #[test]
    fn find_locates_a_listed_device_by_id() {
        let devices = list(Flow::Render).expect("enumeration must not fail");
        if let Some(first) = devices.first() {
            let found = find(Flow::Render, &first.id).expect("a listed id must be findable");
            assert_eq!(found.id, first.id);
            assert_eq!(found.name, first.name);
        }
    }
}