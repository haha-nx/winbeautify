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
pub const IID_IVISUAL_TREE_HELPER_STATICS: GUID =
    GUID::from_u128(0xe75758c4_d25d_4b1d_971f_596f17f12baa);
pub const IID_IELEMENT_COMPOSITION_PREVIEW_STATICS: GUID =
    GUID::from_u128(0x08c92b38_ec99_4c55_bc85_a1c180b27646);

// ---------------------------------------------------------------------------
// Foreign vtables
// ---------------------------------------------------------------------------

/// Slots 3 (`GetIInspectableFromHandle`) and 7 (`GetHandleFromIInspectable`)
/// of `IXamlDiagnostics`; everything we do not call is a placeholder.
#[repr(C)]
pub struct IXamlDiagnosticsVtbl {
    base: [usize; 3], // GetDispatcher, GetUiLayer, GetApplication
    pub get_iinspectable_from_handle:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, handle: u64, out: *mut *mut core::ffi::c_void) -> HRESULT,
    pub get_handle_from_iinspectable:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, instance: *mut core::ffi::c_void, out: *mut u64) -> HRESULT,
}

/// Slot 3 of `IVisualTreeService3` is the inherited
/// `IVisualTreeService::AdviseVisualTreeChange`; classic COM concatenates
/// inherited vtables.
#[repr(C)]
pub struct IVisualTreeServiceVtbl {
    pub advise_visual_tree_change:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, callback: *mut core::ffi::c_void) -> HRESULT,
}

/// Slot 4 of `IDesktopWindowXamlSourceNative` (`get_WindowHandle`).
#[repr(C)]
pub struct IDesktopWindowXamlSourceNativeVtbl {
    attach_to_window: usize,
    pub get_window_handle:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut isize) -> HRESULT,
}

/// Slot 3 of `IDesktopWindowXamlSource` (`get_Content`).
#[repr(C)]
pub struct IDesktopWindowXamlSourceVtbl {
    pub get_content:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT,
}

/// Slot 30 (`get_Name`), 10 (`get_ActualWidth`) and 11 (`get_ActualHeight`)
/// of `IFrameworkElement`; the remaining slots are placeholders.
#[repr(C)]
pub struct IFrameworkElementVtbl {
    before_name: [usize; 27], // Triggers .. DataContext
    pub get_name: unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut u16) -> HRESULT,
    after_name: [usize; 7], // put_Name .. get_Parent
    pub get_actual_width:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut f64) -> HRESULT,
    pub get_actual_height:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut f64) -> HRESULT,
}

/// Slots 3/4 of `IShape` (`get_Fill`/`put_Fill`).
#[repr(C)]
pub struct IShapeVtbl {
    pub get_fill:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, out: *mut *mut core::ffi::c_void) -> HRESULT,
    pub put_fill:
        unsafe extern "system" fn(this: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> HRESULT,
}

/// Slot 4 of `ISolidColorBrush` (`put_Color`).
#[repr(C)]
pub struct ISolidColorBrushVtbl {
    get_color: usize,
    pub put_color: unsafe extern "system" fn(this: *mut core::ffi::c_void, color: Color) -> HRESULT,
}

/// Slots 4 and 6 of `IAcrylicBrush`; the background source is a plain `i32`
/// enum in the ABI (`AcrylicBackgroundSource.Backdrop` == 0), not a boxed
/// value.
#[repr(C)]
pub struct IAcrylicBrushVtbl {
    get_background_source: usize,
    pub put_background_source: unsafe extern "system" fn(this: *mut core::ffi::c_void, value: i32) -> HRESULT,
    get_tint_color: usize,
    pub put_tint_color: unsafe extern "system" fn(this: *mut core::ffi::c_void, color: Color) -> HRESULT,
}

/// Slot 9 of `IVisualTreeHelperStatics` (`GetParent`); the preceding six slots
/// are the FindElementsInHostCoordinates/GetChild family.
#[repr(C)]
pub struct IVisualTreeHelperStaticsVtbl {
    before_get_parent: [usize; 6],
    pub get_parent: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        object: *mut core::ffi::c_void,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
}

/// Slots 3 and 5 of `IElementCompositionPreviewStatics`.
#[repr(C)]
pub struct IElementCompositionPreviewStaticsVtbl {
    pub get_element_visual: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        element: *mut core::ffi::c_void,
        out: *mut *mut core::ffi::c_void,
    ) -> HRESULT,
    get_element_child_visual: usize,
    pub set_element_child_visual: unsafe extern "system" fn(
        this: *mut core::ffi::c_void,
        element: *mut core::ffi::c_void,
        visual: *mut core::ffi::c_void,
    ) -> HRESULT,
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
    /// Whether the object should also answer `IAgileObject`. Only safe for
    /// objects whose methods are thread-safe; the tree callback qualifies
    /// because every method it runs is marshalled back to the UI thread by
    /// the framework before reaching our state.
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
        return S_OK;
    }
    // The WinRT agility marker: an agile callback is stored as a raw pointer
    // and invoked from any apartment without COM proxying.
    const IID_IAGILE_OBJECT: GUID = GUID::from_u128(0x94ea2b94_e9cc_49e0_c0ff_ee64ca8f5b90);
    if *requested == IID_IAGILE_OBJECT && T::AGILE_CALLBACK {
        let cell = com_cell::<T>(this);
        cell.ref_count().fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        unsafe { *out = this };
        return S_OK;
    }

    if !known {
        return E_NOINTERFACE;
    }
    let cell = com_cell::<T>(this);
    cell.ref_count().fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    unsafe { *out = this };
    S_OK
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

/// Identity comparison: two interface pointers are the same object when
/// `QueryInterface(IUnknown)` hands back the same address.
pub fn same_object(a: &IUnknown, b: &IUnknown) -> bool {
    unsafe {
        let pa = qi_raw(a, &<IUnknown as Interface>::IID);
        let pb = qi_raw(b, &<IUnknown as Interface>::IID);
        let equal = match (pa, pb) {
            (Some(x), Some(y)) => x == y,
            _ => false,
        };
        release_raw(pa.unwrap_or(core::ptr::null_mut()));
        release_raw(pb.unwrap_or(core::ptr::null_mut()));
        equal
    }
}

pub unsafe fn release_raw(raw: *mut core::ffi::c_void) {
    if !raw.is_null() {
        let vtbl = *(raw as *mut *mut windows::core::IUnknown_Vtbl);
        ((*vtbl).Release)(raw);
    }
}

/// Identity comparison between an interface wrapper and a raw pointer.
pub unsafe fn same_raw_identity(unknown: &IUnknown, raw: *mut core::ffi::c_void) -> bool {
    if raw.is_null() {
        return false;
    }
    let mine = qi_raw(unknown, &<IUnknown as Interface>::IID);
    
    match mine {
        Some(ptr) => {
            release_raw(ptr);
            ptr == raw
        }
        None => false,
    }
}

/// Free a BSTR the XAML diagnostics framework handed to a callback.
pub unsafe fn free_bstr(bstr: *mut u16) {
    if !bstr.is_null() {
        // The BSTR wrapper takes ownership and frees on drop.
        drop(windows::core::BSTR::from_raw(bstr));
    }
}

/// Read a BSTR the framework handed us, as a UTF-16 string view. The BSTR
/// itself is freed immediately; only the view is borrowed.
pub unsafe fn borrow_bstr<'a>(bstr: *mut u16) -> Option<&'a [u16]> {
    if bstr.is_null() {
        return None;
    }
    // BSTRs carry their length in bytes as a u32 right before the characters.
    let len_bytes = *(bstr as *const u32).sub(1) as usize;
    Some(core::slice::from_raw_parts(bstr, len_bytes / 2))
}
