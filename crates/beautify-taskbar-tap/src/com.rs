//! Hand-rolled COM/WinRT bindings for the interfaces `windows-rs` does not
//! ship: the XAML diagnostics OM (`xamlOM.h`), the `DesktopWindowXamlSource`
//! native interop, and the flat vtables of the `Windows.UI.Xaml` interfaces we
//! call.
//!
//! Every interface here derives from `IInspectable` in the ABI (WinRT interface
//! inheritance is requirement-based, not vtable-based), so a vtable is always
//! three `IUnknown` slots followed by the interface's own methods in header
//! order. Slot numbers and IIDs were taken from the Windows SDK 26100 headers
//! and are stable API contracts.
//!
//! Foreign objects are carried as [`windows::core::IUnknown`]; the helpers at
//! the bottom bridge between owned raw pointers and the refcounted wrapper.

use windows::core::{GUID, IUnknown, Interface, Result as WResult, HRESULT};
use std::sync::atomic::AtomicU32;
use windows::Win32::Foundation::{E_FAIL, E_NOINTERFACE, S_OK};

// ---------------------------------------------------------------------------
// IIDs
// ---------------------------------------------------------------------------

/// `IXamlDiagnostics`, from `xamlOM.h`.
pub const IID_IXAML_DIAGNOSTICS: GUID = GUID::from_u128(0x18c9e2b6_3f43_4116_9f2b_ff935d7770d2);
/// `IVisualTreeService3`, from `xamlOM.h`.
pub const IID_IVISUAL_TREE_SERVICE3: GUID = GUID::from_u128(0x0e79c6e0_85a0_4be8_b41a_655cf1fd19bd);
/// `IVisualTreeServiceCallback`, from `xamlOM.h`.
pub const IID_IVISUAL_TREE_SERVICE_CALLBACK: GUID =
    GUID::from_u128(0xaa7a8931_80e4_4fec_8f3b_553f87b4966e);
/// `IVisualTreeServiceCallback2`, from `xamlOM.h`.
pub const IID_IVISUAL_TREE_SERVICE_CALLBACK2: GUID =
    GUID::from_u128(0xbad9eb88_ae77_4397_b948_5fa2db0a19ea);
/// `IObjectWithSite` (ocidl.h). Not to be confused with `IServiceProvider`
/// (6D5140C1-...), which is what a from-memory IID nearly produced here.
pub const IID_IOBJECT_WITH_SITE: GUID = GUID::from_u128(0xfc4801a3_2ba9_11cf_a229_00aa003d7352);
/// `IClassFactory`.
pub const IID_ICLASS_FACTORY: GUID = GUID::from_u128(0x00000001_0000_0000_c000_000000000046);

/// `IDesktopWindowXamlSourceNative`, from
/// `windows.ui.xaml.hosting.desktopwindowxamlsource.h`.
pub const IID_IDESKTOP_WINDOW_XAML_SOURCE_NATIVE: GUID =
    GUID::from_u128(0x3cbcf1bf_2f76_4e9c_96ab_e84b37972554);

// Windows.UI.Xaml interface IIDs, from the SDK 26100 MIDL headers.
pub const IID_IDESKTOP_WINDOW_XAML_SOURCE: GUID =
    GUID::from_u128(0xd585bfe1_00ff_51be_ba1d_a1329956ea0a);
pub const IID_IUI_ELEMENT: GUID = GUID::from_u128(0x676d0be9_b65c_41c6_ba40_58cf87f201c1);
pub const IID_IDEPENDENCY_OBJECT: GUID = GUID::from_u128(0x5c526665_f60e_4912_af59_5fe0680f089d);
pub const IID_IFRAMEWORK_ELEMENT: GUID = GUID::from_u128(0xa391d09b_4a99_4b7c_9d8d_6fa5d01f6fbf);
pub const IID_IBRUSH: GUID = GUID::from_u128(0x8806a321_1e06_422c_a1cc_01696559e021);
pub const IID_ISHAPE: GUID = GUID::from_u128(0x786f2b75_9aa0_454d_ae06_a2466e37c832);
pub const IID_ISOLID_COLOR_BRUSH: GUID = GUID::from_u128(0x9d850850_66f3_48df_9a8f_824bd5e070af);
pub const IID_IACRYLIC_BRUSH: GUID = GUID::from_u128(0x79bbcf4e_cd66_4f1b_a8b6_cd6d2977c18d);
pub const IID_IBORDER: GUID = GUID::from_u128(0x797c4539_45bd_4633_a044_bfb02ef5170f);
/// `IXamlReaderStatics` (Windows.UI.Xaml.Markup.XamlReader). XAML types cannot
/// be default-activated (`IActivationFactory.ActivateInstance` answers
/// E_NOTIMPL), so the only way to create an `AcrylicBrush` from inside the TAP
/// is to parse it.
pub const IID_IXAML_READER_STATICS: GUID =
    GUID::from_u128(0x9891c6bd_534f_4955_b85a_8a8dc0dca602);
/// `Windows.UI.Xaml.Controls.IControl` — the interface `Taskbar.TaskbarFrame`
/// itself implements. Its `BorderBrush` with a top-only `BorderThickness` is
/// the strongest candidate for what paints the taskbar's top hairline.
pub const IID_ICONTROL: GUID = GUID::from_u128(0xa8912263_2951_4f58_a9c5_5a134eaa7f07);
pub const IID_IVISUAL_TREE_HELPER_STATICS: GUID =
    GUID::from_u128(0xe75758c4_d25d_4b1d_971f_596f17f12baa);
pub const IID_IELEMENT_COMPOSITION_PREVIEW_STATICS: GUID =
    GUID::from_u128(0x08c92b38_ec99_4c55_bc85_a1c180b27646);
pub const IID_ICOMPOSITION_OBJECT: GUID =
    GUID::from_u128(0xbcb4ad45_7609_4550_934f_16002a68fded);

// ---------------------------------------------------------------------------
// Foreign vtables
// ---------------------------------------------------------------------------
//
// Each struct below mirrors a foreign vtable from slot 0, so a field's offset
// *is* its slot index. Slots we never call still have to be present as
// placeholders: leaving one out shifts every following field onto the wrong
// method, which is how a `QueryInterface` ends up being called where a getter
// was meant. `IUnknown` always occupies slots 0-2, and a WinRT interface adds
// `IInspectable` at 3-5, so a WinRT interface's own methods only start at slot
// 6. The `slots` tests at the bottom of this file pin every offset against
// `xamlOM.idl`, the SDK's `desktopwindowxamlsource.idl`, and the WinRT
// metadata (`Windows.UI.Xaml.winmd`).
//
// Every field is `pub`, including the placeholders: the layout is the contract,
// and most of these structs are only ever read out of foreign memory.

/// `IUnknown`: `QueryInterface`, `AddRef`, `Release`.
pub type UnknownSlots = [usize; 3];

/// The `IUnknown` + `IInspectable` block every `Windows.*` WinRT interface
/// starts with.
pub type WinRtSlots = [usize; 6];

/// `IXamlDiagnostics` (`xamlOM.h`). Method order: `GetDispatcher`,
/// `GetUiLayer`, `GetApplication`, `GetIInspectableFromHandle`,
/// `GetHandleFromIInspectable`, `HitTest`, `RegisterInstance`,
/// `GetInitializationData`.
#[repr(C)]
pub struct IXamlDiagnosticsVtbl {
    pub unknown: UnknownSlots, // 0-2
    /// `GetUiLayer` — the root of the XAML content, which is the only way into
    /// an island whose "added" events happened before we connected.
    pub get_ui_layer:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 3
    pub get_dispatcher: usize,  // 4
    pub get_application: usize, // 5
    pub get_iinspectable_from_handle:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, handle: u64, out: *mut *mut core::ffi::c_void) -> HRESULT, // 6
    pub get_handle_from_iinspectable:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, instance: *mut core::ffi::c_void, out: *mut u64) -> HRESULT, // 7
}

/// The vtable of `IVisualTreeService3`, which inherits `IVisualTreeService2` →
/// `IVisualTreeService` → `IUnknown`. Classic COM concatenates inherited
/// vtables, and `AdviseVisualTreeChange` is the *first* method of
/// `IVisualTreeService`, so it sits at slot 3 — not slot 0.
#[repr(C)]
pub struct IVisualTreeServiceVtbl {
    pub unknown: UnknownSlots, // 0-2
    pub advise_visual_tree_change:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, callback: *mut core::ffi::c_void) -> HRESULT, // 3
}

/// `IDesktopWindowXamlSourceNative`
/// (`windows.ui.xaml.hosting.desktopwindowxamlsource.idl`), a classic COM
/// interface: `AttachToWindow` at 3, `WindowHandle` at 4.
#[repr(C)]
pub struct IDesktopWindowXamlSourceNativeVtbl {
    pub unknown: UnknownSlots,     // 0-2
    pub attach_to_window: usize,   // 3
    pub get_window_handle:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut isize) -> HRESULT, // 4
}

/// `IDesktopWindowXamlSource`, whose `Content` property is its first method.
#[repr(C)]
pub struct IDesktopWindowXamlSourceVtbl {
    pub winrt: WinRtSlots, // 0-5
    pub get_content:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 6
}

/// `IFrameworkElement`. `Size` comes before `Name` in the interface, so the
/// three methods we call are not adjacent: `ActualWidth`/`ActualHeight` at
/// 13/14 and `Name` at 33.
#[repr(C)]
pub struct IFrameworkElementVtbl {
    pub winrt: WinRtSlots,        // 0-5
    pub before_size: [usize; 7],  // 6-12  Triggers .. put_Language
    pub get_actual_width:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut f64) -> HRESULT, // 13
    pub get_actual_height:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut f64) -> HRESULT, // 14
    pub before_name: [usize; 18], // 15-32 get_Width .. put_Margin
    pub get_name: unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut u16) -> HRESULT, // 33
}

/// Slots 6-11 of `IShape`: `Fill`/`Fill`, then `Stroke`. The hairline
/// rectangle is literally named `BackgroundStroke` — its line comes from the
/// stroke brush on its top edge, while the fill reads as fully transparent.
#[repr(C)]
pub struct IShapeVtbl {
    pub winrt: WinRtSlots, // 0-5
    pub get_fill:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 6
    pub put_fill:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> HRESULT, // 7
    pub get_stroke:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 8
    pub put_stroke:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> HRESULT, // 9
    /// `StrokeMiterLimit` is get-only.
    pub get_stroke_miter_limit: usize, // 10
    pub get_stroke_thickness:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut f64) -> HRESULT, // 11
}

/// Slots 6/7 of `ISolidColorBrush` (`get_Color`/`put_Color`).
#[repr(C)]
pub struct ISolidColorBrushVtbl {
    pub winrt: WinRtSlots, // 0-5
    pub get_color: unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut Color) -> HRESULT, // 6
    pub put_color: unsafe extern "system" fn(this: *mut core::ffi::c_void, color: Color) -> HRESULT, // 7
}

/// Slots 6-9 of `IAcrylicBrush`; the background source is a plain `i32` enum in
/// the ABI (`AcrylicBackgroundSource.Backdrop` == 0), not a boxed value.
#[repr(C)]
pub struct IAcrylicBrushVtbl {
    pub winrt: WinRtSlots,                // 0-5
    pub get_background_source: usize,     // 6
    pub put_background_source: unsafe extern "system" fn(this: *mut core::ffi::c_void, value: i32) -> HRESULT, // 7
    pub get_tint_color: usize,            // 8
    pub put_tint_color: unsafe extern "system" fn(this: *mut core::ffi::c_void, color: Color) -> HRESULT, // 9
}

/// Slots 6-11 of `IBorder`. The hairline along the taskbar's top edge is the
/// `BorderBrush` of one of the island's `Border` elements, which is why this
/// interface exists here: a brush no shape can reach.
#[repr(C)]
pub struct IBorderVtbl {
    pub winrt: WinRtSlots, // 0-5
    pub get_border_brush:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 6
    pub put_border_brush:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> HRESULT, // 7
    /// `Thickness` is 32 bytes, so the WinRT ABI passes and returns it through a
    /// hidden pointer rather than in registers.
    pub get_border_thickness:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut Thickness) -> HRESULT, // 8
    pub put_border_thickness:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, value: *const Thickness) -> HRESULT, // 9
    pub get_background:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 10
    pub put_background:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> HRESULT, // 11
}

/// `IControl`: slots 6-29 are the font/tab properties (`FontSize` .. `TabNavigation`),
/// then `Template`, `Padding`, the content alignments, and finally the
/// background and border properties the taskbar frame's hairline comes through.
#[repr(C)]
pub struct IControlVtbl {
    pub winrt: WinRtSlots,        // 0-5
    pub before_padding: [usize; 24], // 6-29 FontSize .. TabNavigation, Template
    pub get_padding: usize,       // 30
    pub put_padding: usize,       // 31
    pub before_background: [usize; 4], // 32-35 Horizontal/VerticalContentAlignment
    pub get_background:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 36
    pub put_background:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> HRESULT, // 37
    pub get_border_thickness:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut Thickness) -> HRESULT, // 38
    pub put_border_thickness:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, value: *const Thickness) -> HRESULT, // 39
    pub get_border_brush:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT, // 40
    pub put_border_brush:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> HRESULT, // 41
}

/// `IXamlReaderStatics`: `Load` is the first method of the interface.
#[repr(C)]
pub struct IXamlReaderStaticsVtbl {
    pub winrt: WinRtSlots, // 0-5
    pub load:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, xaml: *mut u16, out: *mut *mut core::ffi::c_void) -> HRESULT, // 6
    pub load_with_initial_template_validation: usize, // 7
}

/// `IVisualTreeHelperStatics`. Method order: four `FindElementsInHostCoordinates`
/// overloads, `GetChild`, `GetChildrenCount`, `GetParent`,
/// `DisconnectChildrenRecursive`.
#[repr(C)]
pub struct IVisualTreeHelperStaticsVtbl {
    pub winrt: WinRtSlots,             // 0-5
    pub before_child: [usize; 4],      // 6-9  FindElementsInHostCoordinates ×4
    pub get_child: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        element: *mut core::ffi::c_void,
        index: i32,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT, // 10
    pub get_children_count: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        element: *mut core::ffi::c_void,
        out: *mut i32,
    ) -> HRESULT, // 11
    pub get_parent: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        object: *mut core::ffi::c_void,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT, // 12
}

/// Slots 6-8 of `IElementCompositionPreviewStatics`.
#[repr(C)]
pub struct IElementCompositionPreviewStaticsVtbl {
    pub winrt: WinRtSlots, // 0-5
    pub get_element_visual: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        element: *mut core::ffi::c_void,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT, // 6
    pub get_element_child_visual: usize, // 7
    pub set_element_child_visual: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        element: *mut core::ffi::c_void,
        visual: *mut core::ffi::c_void,
    ) -> HRESULT, // 8
}

/// Slot 6 of `ICompositionObject` (`get_Compositor`), the first method of that
/// interface. Kept here next to its siblings even though `xaml.rs` owns the
/// call site.
#[repr(C)]
pub struct ICompositionObjectVtbl {
    pub winrt: WinRtSlots, // 0-5
    pub get_compositor: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT, // 6
}

// ---------------------------------------------------------------------------
// Vtables of the COM objects this crate implements
// ---------------------------------------------------------------------------

pub type ComQueryInterface = unsafe extern "system" fn(
    this: *mut core::ffi::c_void,
    iid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT;
pub type ComRefCount =
    unsafe extern "system" fn(this: *mut core::ffi::c_void) -> u32;

/// `IVisualTreeServiceCallback2` as we implement it.
#[repr(C)]
pub struct CallbackVtbl {
    pub query_interface: ComQueryInterface,
    pub add_ref: ComRefCount,
    pub release: ComRefCount,
    pub on_visual_tree_change:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, relation: *const ParentChildRelation, element: *const VisualElement, mutation: i32) -> HRESULT,
    pub on_element_state_changed:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, handle: u64, state: i32, context: *const u16) -> HRESULT,
    /// Slots the interface does not declare, filled with a stub rather than
    /// left as zero.
    ///
    /// explorer's XAML reads past the end of this table when it dispatches a
    /// tree callback, and a slot holding zero is an indirect call to a null
    /// target — which the kernel answers with
    /// `FAST_FAIL_GUARD_ICALL_CHECK_FAILURE`, taking the shell down. What it
    /// would do with those slots is not documented; pointed at a stub that
    /// reports itself they are at worst a logged no-op.
    pub reserved: [unsafe extern "system" fn(*mut core::ffi::c_void, usize, usize, usize) -> HRESULT; 7],
}

/// `IObjectWithSite` as we implement it.
#[repr(C)]
pub struct SiteVtbl {
    pub query_interface: ComQueryInterface,
    pub add_ref: ComRefCount,
    pub release: ComRefCount,
    pub set_site: unsafe extern "system" fn(this: *mut core::ffi::c_void, site: *mut core::ffi::c_void) -> HRESULT,
    pub get_site: unsafe extern "system" fn(this: *mut core::ffi::c_void, iid: *const GUID, out: *mut *mut core::ffi::c_void) -> HRESULT,
}

/// `IClassFactory` as we implement it.
#[repr(C)]
pub struct FactoryVtbl {
    pub query_interface: ComQueryInterface,
    pub add_ref: ComRefCount,
    pub release: ComRefCount,
    pub create_instance: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        outer: *mut core::ffi::c_void,
        iid: *const GUID,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
    pub lock_server: unsafe extern "system" fn(this: *mut core::ffi::c_void, lock: i32) -> HRESULT,
}

/// `Windows.UI.Xaml.Thickness` ABI: four `f64` offsets, in declaration order.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[repr(C)]
pub struct Thickness {
    pub left: f64,
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
}

/// `Windows.UI.Color` ABI: four bytes, A first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct Color {
    pub a: u8,
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub fn from_argb(argb: u32) -> Self {
        Self {
            a: (argb >> 24) as u8,
            r: (argb >> 16) as u8,
            g: (argb >> 8) as u8,
            b: argb as u8,
        }
    }
}

/// `xamlOM.h`: `InstanceHandle` is a 64-bit cookie, the mutation/state enums
/// are `i32`, and the two callback structs are passed by reference (both are
/// larger than 8 bytes, so the x64 calling convention passes their address).
pub type InstanceHandle = u64;

pub const VISUAL_MUTATION_ADD: i32 = 0;
pub const VISUAL_MUTATION_REMOVE: i32 = 1;

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct ParentChildRelation {
    pub parent: InstanceHandle,
    pub child: InstanceHandle,
    pub child_index: u32,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct SourceInfo {
    /// BSTR; ownership passes to the callback.
    pub file_name: *mut u16,
    pub line_number: u32,
    pub column_number: u32,
    pub char_position: u32,
    /// BSTR; ownership passes to the callback.
    pub hash: *mut u16,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct VisualElement {
    pub handle: InstanceHandle,
    pub src_info: SourceInfo,
    /// BSTR; ownership passes to the callback.
    pub type_name: *mut u16,
    /// BSTR; ownership passes to the callback.
    pub name: *mut u16,
    pub num_children: u32,
}

// ---------------------------------------------------------------------------
// COM object plumbing
// ---------------------------------------------------------------------------

/// A vtable pointer that may live in `static`/shared state; the pointed-to
/// static vtable is immutable forever.
#[derive(Clone, Copy)]
pub struct VtblPtr(pub *const core::ffi::c_void);
unsafe impl Send for VtblPtr {}
unsafe impl Sync for VtblPtr {}

/// What the generic `IUnknown` shims need from a hand-rolled COM object.
///
/// Every object in this crate is created as a `Box` (reference count 1) and
/// handed out through [`new_com_object`]; the count reaching zero hands the
/// box back to Rust.
pub trait ComObj: Sized {
    /// IIDs answered with `S_OK` besides `IUnknown`.
    const SUPPORTED: &'static [GUID];
    /// Whether the object should also answer `IAgileObject`.
    ///
    /// It usually should *not*. The reference TAP is `winrt::implements<...,
    /// non_agile>`, and the diagnostics framework relies on that: a non-agile
    /// callback is marshalled onto the XAML UI thread before it is invoked,
    /// which is what makes it legal to touch XAML's thread-affine objects from
    /// the callback. Answering `IAgileObject` lets the framework invoke us on
    /// whatever thread it likes.
    const AGILE_CALLBACK: bool = false;
    fn ref_count(&self) -> &core::sync::atomic::AtomicU32;
}

fn com_cell<'a, T: ComObj>(this: *mut core::ffi::c_void) -> &'a T {
    unsafe { &*(this as *const T) }
}

pub unsafe extern "system" fn com_query_interface<T: ComObj>(
    this: *mut core::ffi::c_void,
    iid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return E_FAIL;
    }
    let requested = unsafe { &*iid };
    let known = *requested == <IUnknown as Interface>::IID
        || T::SUPPORTED.iter().any(|g| g == requested);
    // C++/WinRT objects are weak-reference sources; the framework's advise
    // machinery resolves callbacks through one instead of building a COM
    // proxy, and a missing source deadlocks the advise call. See the shim
    // below for the two interfaces involved.
    const IID_IWEAK_REFERENCE_SOURCE: GUID =
        GUID::from_u128(0x00000038_0000_0000_c000_000000000046);
    if *requested == IID_IWEAK_REFERENCE_SOURCE {
        let cell = com_cell::<T>(this);
        let shim = unsafe { weak_source_shim_for(cell) };
        unsafe { *out = shim };
        trace_query(requested, "IWeakReferenceSource shim");
        return S_OK;
    }
    // The WinRT agility marker: an agile callback is stored as a raw pointer
    // and invoked from any apartment without COM proxying.
    const IID_IAGILE_OBJECT: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);
    if *requested == IID_IAGILE_OBJECT && T::AGILE_CALLBACK {
        let cell = com_cell::<T>(this);
        cell.ref_count().fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        unsafe { *out = this };
        trace_query(requested, "agile object");
        return S_OK;
    }

    // The multiple-window tree callback: answering it is what makes the
    // framework replay *every* XAML island's tree as "added" elements instead of
    // reporting mutations only. Without it the taskbar islands that already
    // existed when we connected never announce a `TaskbarFrame`, which is why
    // only the primary taskbar could ever be claimed. It is safe here only
    // because this object also answers `IAgileObject`: the crash that made an
    // earlier build refuse this interface was the framework marshalling a
    // non-agile callback (see `ComObj::AGILE_CALLBACK`).
    if *requested == IID_IVISUAL_TREE_SERVICE_CALLBACK2 {
        let cell = com_cell::<T>(this);
        cell.ref_count().fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        unsafe { *out = this };
        trace_query(requested, "multiple-window callback");
        return S_OK;
    }

    if !known {
        trace_query(requested, "E_NOINTERFACE");
        return E_NOINTERFACE;
    }
    let cell = com_cell::<T>(this);
    cell.ref_count().fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    unsafe { *out = this };
    trace_query(requested, "ok");
    S_OK
}

/// Record what the framework asked a hand-rolled object for and what it got.
///
/// Written synchronously and without allocating: the interesting case is the
/// one where the answer is `E_NOINTERFACE` and the framework then calls a null
/// pointer, and by the time the logger thread woke up, the process is gone.
fn trace_query(requested: &GUID, answer: &str) {
    crate::logging::debug_log_fmt_sync(format_args!("qi {requested:?} -> {answer}"));
}

// ---------------------------------------------------------------------------
// IWeakReferenceSource / IWeakReference shims
// ---------------------------------------------------------------------------

/// `IWeakReferenceSource` view of a hand-rolled COM object: a tiny shim whose
/// only job is handing out an [`IWeakReference`] over the owner.
#[repr(C)]
struct WeakSourceShim {
    vtable: *const core::ffi::c_void,
    ref_count: AtomicU32,
    owner: IUnknown,
}

#[repr(C)]
struct WeakRefVtbl {
    query_interface: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        iid: *const GUID,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
    add_ref: unsafe extern "system" fn(this: *mut core::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(this: *mut core::ffi::c_void) -> u32,
    resolve: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        riid: *const GUID,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
}

#[repr(C)]
struct WeakRefShim {
    vtable: *const core::ffi::c_void,
    ref_count: AtomicU32,
    owner: IUnknown,
}

unsafe extern "system" fn shim_qi_delegate(
    this: *mut core::ffi::c_void,
    iid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let shim = &*(this as *const WeakSourceShim);
    match qi_from_raw(shim.owner.as_raw(), unsafe { &*iid }) {
        Some(raw) => {
            unsafe { *out = raw };
            S_OK
        }
        None => E_NOINTERFACE,
    }
}

unsafe extern "system" fn shim_add_ref(this: *mut core::ffi::c_void) -> u32 {
    let shim = &*(this as *const WeakSourceShim);
    shim.ref_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed) + 1
}

unsafe extern "system" fn weak_source_release(this: *mut core::ffi::c_void) -> u32 {
    let shim = &*(this as *mut WeakSourceShim);
    let left = shim
        .ref_count
        .fetch_sub(1, core::sync::atomic::Ordering::Release);
    if left == 1 {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        drop(unsafe { Box::from_raw(this as *mut WeakSourceShim) });
    }
    left
}

unsafe extern "system" fn weak_source_get_weak_reference(
    this: *mut core::ffi::c_void,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let shim = &*(this as *const WeakSourceShim);
    // The weak reference holds a strong reference to the owner; when the
    // owner dies the owner pointer below dangles, but owner death also means
    // no one can still be calling Resolve through us.
    let owner = shim.owner.clone();
    let weak = Box::new(WeakRefShim {
        vtable: &WEAK_REF_VTABLE as *const WeakRefVtbl as *const core::ffi::c_void,
        ref_count: AtomicU32::new(1),
        owner,
    });
    unsafe { *out = Box::into_raw(weak) as *mut core::ffi::c_void };
    S_OK
}

unsafe extern "system" fn weak_ref_release(this: *mut core::ffi::c_void) -> u32 {
    let shim = &*(this as *mut WeakRefShim);
    let left = shim
        .ref_count
        .fetch_sub(1, core::sync::atomic::Ordering::Release);
    if left == 1 {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        drop(unsafe { Box::from_raw(this as *mut WeakRefShim) });
    }
    left
}

unsafe extern "system" fn weak_ref_resolve(
    this: *mut core::ffi::c_void,
    riid: *const GUID,
    out: *mut *mut core::ffi::c_void,
) -> HRESULT {
    let shim = &*(this as *const WeakRefShim);
    match qi_from_raw(shim.owner.as_raw(), unsafe { &*riid }) {
        Some(raw) => {
            unsafe { *out = raw };
            S_OK
        }
        None => E_NOINTERFACE,
    }
}

#[repr(C)]
struct WeakSourceVtbl {
    query_interface: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        iid: *const GUID,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
    add_ref: unsafe extern "system" fn(this: *mut core::ffi::c_void) -> u32,
    release: unsafe extern "system" fn(this: *mut core::ffi::c_void) -> u32,
    get_weak_reference: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
}

static WEAK_SOURCE_VTABLE: WeakSourceVtbl = WeakSourceVtbl {
    query_interface: shim_qi_delegate,
    add_ref: shim_add_ref,
    release: weak_source_release,
    get_weak_reference: weak_source_get_weak_reference,
};

static WEAK_REF_VTABLE: WeakRefVtbl = WeakRefVtbl {
    query_interface: shim_qi_delegate,
    add_ref: shim_add_ref,
    release: weak_ref_release,
    resolve: weak_ref_resolve,
};

/// Build the `IWeakReferenceSource` view of a hand-rolled COM object. The
/// shim holds one strong reference to the owner.
unsafe fn weak_source_shim_for<T: ComObj>(owner: &T) -> *mut core::ffi::c_void {
    let this = owner as *const T as *mut core::ffi::c_void;
    // AddRef the owner: the shim owns one reference, handed to `from_abi`.
    let vtbl = *(this as *mut *mut windows::core::IUnknown_Vtbl);
    ((*vtbl).AddRef)(this);
    use windows::core::Type;
    let Ok(owner_unk) = IUnknown::from_abi(this) else {
        release_raw(this);
        return core::ptr::null_mut();
    };
    let shim = Box::new(WeakSourceShim {
        vtable: &WEAK_SOURCE_VTABLE as *const WeakSourceVtbl as *const core::ffi::c_void,
        ref_count: AtomicU32::new(1),
        owner: owner_unk,
    });
    Box::into_raw(shim) as *mut core::ffi::c_void
}

pub unsafe extern "system" fn com_add_ref<T: ComObj>(this: *mut core::ffi::c_void) -> u32 {
    com_cell::<T>(this)
        .ref_count()
        .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
        + 1
}

pub unsafe extern "system" fn com_release<T: ComObj>(this: *mut core::ffi::c_void) -> u32 {
    let cell = com_cell::<T>(this);
    let left = cell
        .ref_count()
        .fetch_sub(1, core::sync::atomic::Ordering::Release);
    if left == 1 {
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
        drop(unsafe { Box::from_raw(this as *mut T) });
    }
    left
}

/// Build a COM object from a Rust value: the box becomes the object, the
/// reference count starts at 1 for the returned pointer.
pub fn new_com_object<T: ComObj>(value: T) -> *mut T {
    Box::into_raw(Box::new(value))
}

// ---------------------------------------------------------------------------
// Raw pointer helpers
// ---------------------------------------------------------------------------

/// Adopt a raw COM pointer we own into a refcounted wrapper.
///
/// `IUnknown::from_abi` takes ownership of the callee's reference without
/// bumping the refcount, so nothing is released here.
pub unsafe fn adopt(raw: *mut core::ffi::c_void) -> WResult<IUnknown> {
    use windows::core::Type;
    <IUnknown as Type<IUnknown>>::from_abi(raw)
}

/// `QueryInterface` for an interface we address through hand-rolled vtables.
/// The returned pointer carries one reference the caller must release.
pub unsafe fn qi_raw(unk: &impl Interface, iid: &GUID) -> Option<*mut core::ffi::c_void> {
    qi_from_raw(unk.as_raw(), iid)
}

/// Same, starting from a raw interface pointer.
pub unsafe fn qi_from_raw(
    this: *mut core::ffi::c_void,
    iid: &GUID,
) -> Option<*mut core::ffi::c_void> {
    if this.is_null() {
        return None;
    }
    let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
    let vtbl = *(this as *mut *mut windows::core::IUnknown_Vtbl);
    let ok = ((*vtbl).QueryInterface)(this, iid, &mut out);
    if ok.is_ok() && !out.is_null() {
        Some(out)
    } else {
        None
    }
}

/// Read a foreign vtable out of a raw interface pointer.
pub unsafe fn vtbl_of<'a, V>(this: *mut core::ffi::c_void) -> WResult<&'a V> {
    let slot = *(this as *mut *mut V);
    if slot.is_null() {
        return Err(windows::core::Error::from_hresult(E_FAIL));
    }
    Ok(&*slot)
}

/// Identity comparison: are these two references the same object?
pub fn same_object(a: &IUnknown, b: &IUnknown) -> bool {
    unsafe { same_raw_identity(a, b.as_raw()) }
}

pub unsafe fn release_raw(raw: *mut core::ffi::c_void) {
    if !raw.is_null() {
        let vtbl = *(raw as *mut *mut windows::core::IUnknown_Vtbl);
        ((*vtbl).Release)(raw);
    }
}

/// Run a COM entry point body, turning a panic into a failed `HRESULT`.
///
/// A panic must never reach an `extern "system"` boundary: Rust aborts the
/// process, and here that process is explorer — the user's desktop dies and
/// restarts, the host notices a new taskbar and injects again, and the whole
/// thing loops. So every entry point the framework, the OS or our own hooks can
/// reach goes through this, and the panic text goes to the log instead.
pub fn guard(body: impl FnOnce() -> HRESULT) -> HRESULT {
    guard_value(E_FAIL, body)
}

/// [`guard`] for entry points whose return type is not an `HRESULT` (the hook
/// proc returns an `LRESULT`).
pub fn guard_value<R>(fallback: R, body: impl FnOnce() -> R) -> R {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)) {
        Ok(value) => value,
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "opaque panic".into());
            crate::logging::debug_log_fmt(format_args!("COM entry point panicked: {text}"));
            fallback
        }
    }
}

/// Take a reference on a raw interface pointer, for the cases where a caller
/// hands us a borrowed one (a COM `[in]` parameter) that has to outlive the
/// call.
pub unsafe fn add_ref_raw(raw: *mut core::ffi::c_void) {
    if !raw.is_null() {
        let vtbl = *(raw as *mut *mut windows::core::IUnknown_Vtbl);
        ((*vtbl).AddRef)(raw);
    }
}

/// Identity comparison between an interface wrapper and a raw interface
/// pointer. Both sides are resolved through `IUnknown` first: two references
/// to one object do not necessarily hold the same interface pointer, so
/// comparing them directly reports a mismatch for an object that is in fact
/// the same.
pub unsafe fn same_raw_identity(unknown: &IUnknown, raw: *mut core::ffi::c_void) -> bool {
    if raw.is_null() {
        return false;
    }
    let iid = <IUnknown as Interface>::IID;
    let mine = qi_raw(unknown, &iid);
    let theirs = qi_from_raw(raw, &iid);
    let equal = matches!((mine, theirs), (Some(a), Some(b)) if a == b);
    release_raw(mine.unwrap_or(core::ptr::null_mut()));
    release_raw(theirs.unwrap_or(core::ptr::null_mut()));
    equal
}

/// Describe a code address as `module+0xoffset`.
///
/// Foreign vtables are the one thing in this crate that cannot be checked by
/// reading our own code: a slot that holds garbage, or a null, only shows up as
/// a crash inside the framework. Logging what the slots point at turns "it
/// crashed in explorer" into a readable answer.
pub fn describe_address(address: usize) -> String {
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    if address == 0 {
        return "null".to_string();
    }
    let mut module = windows::Win32::Foundation::HMODULE::default();
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            windows::core::PCWSTR(address as *const u16),
            &mut module,
        )
    };
    if found.is_err() {
        return format!("{address:#x} (in no loaded module)");
    }
    let mut buffer = [0u16; 260];
    let len = unsafe { GetModuleFileNameW(Some(module), &mut buffer) };
    let len = (len as usize).min(buffer.len());
    let path = String::from_utf16_lossy(&buffer[..len]);
    let name = path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string();
    match address.checked_sub(module.0 as usize) {
        Some(offset) => format!("{name}+{offset:#x}"),
        None => format!("{name} {address:#x}"),
    }
}

/// Read the first `count` pointers of a foreign vtable, as descriptions.
pub unsafe fn describe_vtable(vtable_owner: *mut core::ffi::c_void, count: usize) -> String {
    let slots = vtable_owner as *const usize;
    let mut text = String::new();
    for index in 0..count {
        let address = unsafe { *slots.add(index) };
        let described = describe_address(address);
        if index > 0 {
            text.push_str(", ");
        }
        text.push_str(&format!("{index}:{described}"));
    }
    text
}

/// Free a BSTR the XAML diagnostics framework handed to a callback.
pub unsafe fn free_bstr(bstr: *mut u16) {
    if !bstr.is_null() {
        // The BSTR wrapper takes ownership and frees on drop.
        drop(windows::core::BSTR::from_raw(bstr));
    }
}

/// Read a BSTR the framework handed to a callback, as a UTF-16 string view. The BSTR
/// itself is freed immediately; only the view is borrowed.
pub unsafe fn borrow_bstr<'a>(bstr: *mut u16) -> Option<&'a [u16]> {
    if bstr.is_null() {
        return None;
    }
    // BSTRs carry their length in bytes as a u32 right before the characters.
    let len_bytes = *(bstr as *const u32).sub(1) as usize;
    Some(core::slice::from_raw_parts(bstr, len_bytes / 2))
}

// ---------------------------------------------------------------------------
// Vtable layout tests
// ---------------------------------------------------------------------------
//
// These pin every method against the vtable slot the OS expects, because a
// wrong slot does not fail loudly: it calls a neighbouring method with the
// wrong arguments. Slot sources are named per test.

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{offset_of, size_of};

    /// Offset, in bytes, of vtable entry `slot`.
    fn at(slot: usize) -> usize {
        slot * size_of::<usize>()
    }

    #[test]
    fn xaml_diagnostics_slots() {
        // xamlOM.idl: GetDispatcher, GetUiLayer, GetApplication,
        // GetIInspectableFromHandle, GetHandleFromIInspectable, HitTest, ...
        assert_eq!(at(3), offset_of!(IXamlDiagnosticsVtbl, get_ui_layer));
        assert_eq!(at(6), offset_of!(IXamlDiagnosticsVtbl, get_iinspectable_from_handle));
        assert_eq!(at(7), offset_of!(IXamlDiagnosticsVtbl, get_handle_from_iinspectable));
        // Only the prefix we call is modelled; HitTest, RegisterInstance and
        // GetInitializationData follow.
        assert_eq!(at(8), size_of::<IXamlDiagnosticsVtbl>());
    }

    #[test]
    fn visual_tree_service_advise_is_slot_three() {
        // IVisualTreeService3 : IVisualTreeService2 : IVisualTreeService :
        // IUnknown, and AdviseVisualTreeChange is IVisualTreeService's first
        // method.
        assert_eq!(at(3), offset_of!(IVisualTreeServiceVtbl, advise_visual_tree_change));
        assert_eq!(at(4), size_of::<IVisualTreeServiceVtbl>());
    }

    #[test]
    fn visual_tree_service_callback_slots() {
        // Our own object, called by the framework: OnVisualTreeChange is the
        // first method IVisualTreeServiceCallback adds to IUnknown.
        assert_eq!(at(3), offset_of!(CallbackVtbl, on_visual_tree_change));
        assert_eq!(at(4), offset_of!(CallbackVtbl, on_element_state_changed));
    }

    #[test]
    fn object_with_site_and_class_factory_slots() {
        assert_eq!(at(3), offset_of!(SiteVtbl, set_site));
        assert_eq!(at(4), offset_of!(SiteVtbl, get_site));
        assert_eq!(at(3), offset_of!(FactoryVtbl, create_instance));
        assert_eq!(at(4), offset_of!(FactoryVtbl, lock_server));
    }

    #[test]
    fn desktop_window_xaml_source_slots() {
        // desktopwindowxamlsource.idl: classic COM, AttachToWindow then the
        // WindowHandle property.
        assert_eq!(
            at(4),
            offset_of!(IDesktopWindowXamlSourceNativeVtbl, get_window_handle)
        );
        // WinRT: IUnknown + IInspectable, then get_Content at 6.
        assert_eq!(at(6), offset_of!(IDesktopWindowXamlSourceVtbl, get_content));
        assert_eq!(at(7), size_of::<IDesktopWindowXamlSourceVtbl>());
    }

    #[test]
    fn framework_element_slots() {
        // Windows.UI.Xaml.winmd, IFrameworkElement's own method order:
        // 7 get_ActualWidth, 8 get_ActualHeight, 27 get_Name — after the
        // six-slot IUnknown + IInspectable prefix.
        assert_eq!(at(13), offset_of!(IFrameworkElementVtbl, get_actual_width));
        assert_eq!(at(14), offset_of!(IFrameworkElementVtbl, get_actual_height));
        assert_eq!(at(33), offset_of!(IFrameworkElementVtbl, get_name));
        assert_eq!(at(34), size_of::<IFrameworkElementVtbl>());
    }

    #[test]
    fn brush_and_shape_slots() {
        // Shapes.IShape: get_Fill, put_Fill, get_Stroke, put_Stroke,
        // get_StrokeMiterLimit (get-only), get_StrokeThickness.
        assert_eq!(at(6), offset_of!(IShapeVtbl, get_fill));
        assert_eq!(at(7), offset_of!(IShapeVtbl, put_fill));
        assert_eq!(at(8), offset_of!(IShapeVtbl, get_stroke));
        assert_eq!(at(9), offset_of!(IShapeVtbl, put_stroke));
        assert_eq!(at(11), offset_of!(IShapeVtbl, get_stroke_thickness));
        assert_eq!(at(12), size_of::<IShapeVtbl>());
        // Media.ISolidColorBrush: get_Color, put_Color.
        assert_eq!(at(7), offset_of!(ISolidColorBrushVtbl, put_color));
        // Media.IAcrylicBrush: BackgroundSource, TintColor, ...
        assert_eq!(
            at(7),
            offset_of!(IAcrylicBrushVtbl, put_background_source)
        );
        assert_eq!(at(9), offset_of!(IAcrylicBrushVtbl, put_tint_color));
        assert_eq!(at(10), size_of::<IAcrylicBrushVtbl>());
    }

    #[test]
    fn border_slots() {
        // Controls.IBorder, from the Windows.UI.Xaml.winmd that ships with the
        // OS (extracted from C:\Windows\System32\WinMetadata): BorderBrush,
        // BorderThickness, Background, CornerRadius, Padding, Child, ...
        assert_eq!(at(6), offset_of!(IBorderVtbl, get_border_brush));
        assert_eq!(at(7), offset_of!(IBorderVtbl, put_border_brush));
        assert_eq!(at(8), offset_of!(IBorderVtbl, get_border_thickness));
        assert_eq!(at(9), offset_of!(IBorderVtbl, put_border_thickness));
        assert_eq!(at(10), offset_of!(IBorderVtbl, get_background));
        assert_eq!(at(11), offset_of!(IBorderVtbl, put_background));
        assert_eq!(at(12), size_of::<IBorderVtbl>());
    }

    #[test]
    fn control_slots() {
        // Controls.IControl, from the OS's Windows.UI.Xaml.winmd: the font and
        // tab properties occupy slots 6-29, then Template, Padding and the
        // content alignments, and Background/BorderThickness/BorderBrush at
        // 36-41. Cross-checked against IUIElement's known GUID with the same
        // extraction script that produced these slot numbers.
        assert_eq!(at(30), offset_of!(IControlVtbl, get_padding));
        assert_eq!(at(36), offset_of!(IControlVtbl, get_background));
        assert_eq!(at(38), offset_of!(IControlVtbl, get_border_thickness));
        assert_eq!(at(39), offset_of!(IControlVtbl, put_border_thickness));
        assert_eq!(at(40), offset_of!(IControlVtbl, get_border_brush));
        assert_eq!(at(41), offset_of!(IControlVtbl, put_border_brush));
        assert_eq!(at(42), size_of::<IControlVtbl>());
    }

    #[test]
    fn xaml_reader_slots() {
        // Markup.IXamlReaderStatics, from the OS winmd: Load, then
        // LoadWithInitialTemplateValidation.
        assert_eq!(at(6), offset_of!(IXamlReaderStaticsVtbl, load));
        assert_eq!(at(7), offset_of!(IXamlReaderStaticsVtbl, load_with_initial_template_validation));
        assert_eq!(at(8), size_of::<IXamlReaderStaticsVtbl>());
    }

    #[test]
    fn statics_slots() {
        // Media.IVisualTreeHelperStatics: four FindElementsInHostCoordinates
        // overloads, GetChild, GetChildrenCount, then GetParent.
        assert_eq!(at(10), offset_of!(IVisualTreeHelperStaticsVtbl, get_child));
        assert_eq!(
            at(11),
            offset_of!(IVisualTreeHelperStaticsVtbl, get_children_count)
        );
        assert_eq!(at(12), offset_of!(IVisualTreeHelperStaticsVtbl, get_parent));
        assert_eq!(at(13), size_of::<IVisualTreeHelperStaticsVtbl>());
        // Hosting.IElementCompositionPreviewStatics: GetElementVisual,
        // GetElementChildVisual, SetElementChildVisual.
        assert_eq!(
            at(6),
            offset_of!(IElementCompositionPreviewStaticsVtbl, get_element_visual)
        );
        assert_eq!(
            at(8),
            offset_of!(
                IElementCompositionPreviewStaticsVtbl,
                set_element_child_visual
            )
        );
        assert_eq!(at(9), size_of::<IElementCompositionPreviewStaticsVtbl>());
        // UI.Composition.ICompositionObject: get_Compositor first.
        assert_eq!(at(6), offset_of!(ICompositionObjectVtbl, get_compositor));
        assert_eq!(at(7), size_of::<ICompositionObjectVtbl>());
    }
}
