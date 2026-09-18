//! Thin wrappers over the `Windows.UI.Xaml` surfaces the taskbar repaint
//! needs, addressed through the hand-rolled vtables in [`crate::com`].
//!
//! Everything here runs on the taskbar's XAML UI thread; XAML objects are
//! thread-affine and the wrappers do not attempt any marshalling. Callers
//! hand in the object as an `IUnknown`; each wrapper `QueryInterface`s to the
//! interface it needs and releases it again.

use crate::com::{self, Color};
use windows::core::{GUID, IUnknown, Interface, Result as WResult, PCWSTR};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_FLAGS};
use windows::Win32::System::WinRT::{IActivationFactory, RoGetActivationFactory};

/// `AcrylicBackgroundSource.Backdrop`: sample what is behind the window
/// instead of inside the XAML island.
const ACRYLIC_BACKDROP: i32 = 0;

/// Default-activate a runtime class and QI it to a flat interface from
/// [`crate::com`]. The returned raw pointer carries one reference.
unsafe fn activate_as(class: &str, iid: &GUID) -> WResult<*mut core::ffi::c_void> {
    let factory: IActivationFactory = RoGetActivationFactory(&windows::core::HSTRING::from(class))?;
    let instance = unsafe { factory.ActivateInstance()? };
    match unsafe { com::qi_raw(&instance, iid) } {
        Some(raw) => Ok(raw),
        None => Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_NOINTERFACE,
        )),
    }
}

/// Activate a statics class and hand it back as the flat factory interface.
/// The returned raw pointer carries one reference.
unsafe fn get_statics(class: &str, iid: &GUID) -> WResult<*mut core::ffi::c_void> {
    let inspectable: windows::core::IInspectable =
        RoGetActivationFactory(&windows::core::HSTRING::from(class))?;
    match unsafe { com::qi_raw(&inspectable, iid) } {
        Some(raw) => Ok(raw),
        None => Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_NOINTERFACE,
        )),
    }
}

/// `InitializeXamlDiagnosticsEx` from `Windows.UI.Xaml.dll`.
pub type InitializeXamlDiagnosticsEx = unsafe extern "system" fn(
    connection: PCWSTR,
    pid: u32,
    xaml_diagnostics_dll: PCWSTR,
    tap_dll: PCWSTR,
    tap_clsid: windows::core::GUID,
    initialization_data: PCWSTR,
) -> windows::core::HRESULT;

/// Resolve `InitializeXamlDiagnosticsEx`, loading `Windows.UI.Xaml.dll` if the
/// hosting process has not already got it.
pub fn get_xaml_diagnostics_entry() -> WResult<InitializeXamlDiagnosticsEx> {
    unsafe {
        let module = LoadLibraryExW(
            windows::core::w!("Windows.UI.Xaml.dll"),
            None,
            LOAD_LIBRARY_FLAGS(0x0000_0800), // LOAD_LIBRARY_SEARCH_SYSTEM32
        )?;
        let addr = GetProcAddress(
            module,
            windows::core::PCSTR(c"InitializeXamlDiagnosticsEx".as_ptr().cast()),
        )
        .ok_or_else(windows::core::Error::empty)?;
        Ok(core::mem::transmute::<
            unsafe extern "system" fn() -> isize,
            InitializeXamlDiagnosticsEx,
        >(addr))
    }
}

// ---------------------------------------------------------------------------
// Object wrappers — take the object as `IUnknown`, QI internally.
// ---------------------------------------------------------------------------

/// `VisualTreeHelper.GetParent`.
pub unsafe fn parent_of(object: *mut core::ffi::c_void) -> Option<IUnknown> {
    let dependency_object = com::qi_from_raw(object, &com::IID_IDEPENDENCY_OBJECT)?;
    let factory = match get_statics(
        "Windows.UI.Xaml.Media.VisualTreeHelper",
        &com::IID_IVISUAL_TREE_HELPER_STATICS,
    ) {
        Ok(factory) => factory,
        Err(_) => {
            com::release_raw(dependency_object);
            return None;
        }
    };
    let parent = com::vtbl_of::<com::IVisualTreeHelperStaticsVtbl>(factory)
        .ok()
        .and_then(|vtbl| {
            let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
            (vtbl.get_parent)(factory, dependency_object, &mut out)
                .is_ok()
                .then_some(out)
                .filter(|p| !p.is_null())
        });
    com::release_raw(factory);
    com::release_raw(dependency_object);
    parent.and_then(|raw| com::adopt(raw).ok())
}

/// `IFrameworkElement.Name`.
pub unsafe fn name_of(object: *mut core::ffi::c_void) -> Option<String> {
    let element = com::qi_from_raw(object, &com::IID_IFRAMEWORK_ELEMENT)?;
    let name = element_name_raw(element);
    com::release_raw(element);
    name
}

/// `IFrameworkElement.ActualWidth`/`ActualHeight`, in DIPs.
pub unsafe fn actual_size_of(object: *mut core::ffi::c_void) -> Option<(f64, f64)> {
    let element = com::qi_from_raw(object, &com::IID_IFRAMEWORK_ELEMENT)?;
    let size = actual_size_raw(element);
    com::release_raw(element);
    size
}

/// `IShape.Fill`, as an owned raw brush pointer (one reference).
pub unsafe fn fill_of(shape: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let typed = com::qi_from_raw(shape, &com::IID_ISHAPE)?;
    let result = fill_raw(typed);
    com::release_raw(typed);
    result
}

/// `IShape.Fill = brush` (null clears).
pub unsafe fn set_fill(shape: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> bool {
    let Some(typed) = com::qi_from_raw(shape, &com::IID_ISHAPE) else {
        return false;
    };
    let ok = match com::vtbl_of::<com::IShapeVtbl>(typed) {
        Ok(vtbl) => (vtbl.put_fill)(typed, brush).is_ok(),
        Err(_) => false,
    };
    com::release_raw(typed);
    ok
}

/// `IDesktopWindowXamlSource.Content`, as an owned raw pointer.
pub unsafe fn source_content(source: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let typed = com::qi_from_raw(source, &com::IID_IDESKTOP_WINDOW_XAML_SOURCE)?;
    let content = source_content_raw(typed);
    com::release_raw(typed);
    content
}

/// `IDesktopWindowXamlSourceNative.WindowHandle` — the
/// `Windows.UI.Composition.DesktopWindowContentBridge` child of the taskbar.
pub unsafe fn source_window_handle(source: *mut core::ffi::c_void) -> Option<isize> {
    let native = com::qi_from_raw(source, &com::IID_IDESKTOP_WINDOW_XAML_SOURCE_NATIVE)?;
    let hwnd = match com::vtbl_of::<com::IDesktopWindowXamlSourceNativeVtbl>(native) {
        Ok(vtbl) => {
            let mut out: isize = 0;
            (vtbl.get_window_handle)(native, &mut out)
                .is_ok()
                .then_some(out)
                .filter(|h| *h != 0)
        }
        Err(_) => None,
    };
    com::release_raw(native);
    hwnd
}

/// `SolidColorBrush` with `Color`, as an owned raw brush.
pub unsafe fn create_solid_brush(color: Color) -> Option<*mut core::ffi::c_void> {
    let brush = activate_as(
        "Windows.UI.Xaml.Media.SolidColorBrush",
        &com::IID_ISOLID_COLOR_BRUSH,
    )
    .ok()?;
    let vtbl: &com::ISolidColorBrushVtbl = com::vtbl_of(brush).ok()?;
    if (vtbl.put_color)(brush, color).is_ok() {
        Some(brush)
    } else {
        com::release_raw(brush);
        None
    }
}

/// `AcrylicBrush` with a backdrop source and the given tint, as an owned raw
/// brush.
pub unsafe fn create_acrylic_brush(color: Color) -> Option<*mut core::ffi::c_void> {
    let brush =
        activate_as("Windows.UI.Xaml.Media.AcrylicBrush", &com::IID_IACRYLIC_BRUSH).ok()?;
    let vtbl: &com::IAcrylicBrushVtbl = com::vtbl_of(brush).ok()?;
    let applied = (vtbl.put_background_source)(brush, ACRYLIC_BACKDROP).is_ok()
        && (vtbl.put_tint_color)(brush, color).is_ok();
    if applied {
        Some(brush)
    } else {
        com::release_raw(brush);
        None
    }
}

/// `ElementCompositionPreview.GetElementVisual` plus the `ICompositionObject`
/// `Compositor` readout (the windows crate only generates `Compositor()` on
/// runtime classes, not on the interfaces we QI to).
pub unsafe fn element_compositor(
    element_object: *mut core::ffi::c_void,
) -> WResult<windows::UI::Composition::Compositor> {
    let visual = element_visual(element_object)?;
    let composition_object = com::qi_from_raw(visual.as_raw(), &com::IID_ICOMPOSITION_OBJECT);
    let composition_object = match composition_object {
        Some(raw) => raw,
        None => {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_NOINTERFACE,
            ))
        }
    };
    let result = (|| {
        let vtbl: &com::ICompositionObjectVtbl = com::vtbl_of(composition_object)?;
        let mut raw: *mut core::ffi::c_void = core::ptr::null_mut();
        (vtbl.get_compositor)(composition_object, &mut raw).ok()?;
        com::adopt(raw)?.cast::<windows::UI::Composition::Compositor>()
    })();
    com::release_raw(composition_object);
    result
}

/// `ElementCompositionPreview.GetElementVisual`.
pub unsafe fn element_visual(
    element_object: *mut core::ffi::c_void,
) -> WResult<windows::UI::Composition::IVisual> {
    let element = com::qi_from_raw(element_object, &com::IID_IUI_ELEMENT);
    let element = match element {
        Some(element) => element,
        None => {
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_NOINTERFACE,
            ))
        }
    };
    let result = (|| {
        let factory = get_statics(
            "Windows.UI.Xaml.Hosting.ElementCompositionPreview",
            &com::IID_IELEMENT_COMPOSITION_PREVIEW_STATICS,
        )?;
        let visual = (|| {
            let vtbl: &com::IElementCompositionPreviewStaticsVtbl = com::vtbl_of(factory)?;
            let mut raw: *mut core::ffi::c_void = core::ptr::null_mut();
            (vtbl.get_element_visual)(factory, element, &mut raw).ok()?;
            let visual = com::adopt(raw)?;
            visual.cast::<windows::UI::Composition::IVisual>()
        })();
        com::release_raw(factory);
        visual
    })();
    com::release_raw(element);
    result
}

/// `ElementCompositionPreview.SetElementChildVisual`.
pub unsafe fn set_element_child_visual(
    element_object: *mut core::ffi::c_void,
    visual: *mut core::ffi::c_void,
) -> bool {
    let Some(element) = com::qi_from_raw(element_object, &com::IID_IUI_ELEMENT) else {
        return false;
    };
    let factory = match get_statics(
        "Windows.UI.Xaml.Hosting.ElementCompositionPreview",
        &com::IID_IELEMENT_COMPOSITION_PREVIEW_STATICS,
    ) {
        Ok(factory) => factory,
        Err(_) => {
            com::release_raw(element);
            return false;
        }
    };
    let ok = match com::vtbl_of::<com::IElementCompositionPreviewStaticsVtbl>(factory) {
        Ok(vtbl) => (vtbl.set_element_child_visual)(factory, element, visual).is_ok(),
        Err(_) => false,
    };
    com::release_raw(factory);
    com::release_raw(element);
    ok
}

/// QI probe for interfaces we only need to test for.
pub unsafe fn supports(object: *mut core::ffi::c_void, iid: &GUID) -> bool {
    match com::qi_from_raw(object, iid) {
        Some(raw) => {
            com::release_raw(raw);
            true
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Raw-level helpers used right after an explicit QI.
// ---------------------------------------------------------------------------

unsafe fn element_name_raw(framework_element: *mut core::ffi::c_void) -> Option<String> {
    let vtbl: &com::IFrameworkElementVtbl = com::vtbl_of(framework_element).ok()?;
    let mut raw: *mut u16 = core::ptr::null_mut();
    if (vtbl.get_name)(framework_element, &mut raw).is_ok() && !raw.is_null() {
        let text = com::borrow_bstr(raw).map(String::from_utf16_lossy);
        com::free_bstr(raw);
        text
    } else {
        None
    }
}

unsafe fn actual_size_raw(framework_element: *mut core::ffi::c_void) -> Option<(f64, f64)> {
    let vtbl: &com::IFrameworkElementVtbl = com::vtbl_of(framework_element).ok()?;
    let mut width = 0.0f64;
    let mut height = 0.0f64;
    let w_ok = (vtbl.get_actual_width)(framework_element, &mut width).is_ok();
    let h_ok = (vtbl.get_actual_height)(framework_element, &mut height).is_ok();
    (w_ok && h_ok).then_some((width, height))
}

unsafe fn fill_raw(shape: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let vtbl: &com::IShapeVtbl = com::vtbl_of(shape).ok()?;
    let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
    if (vtbl.get_fill)(shape, &mut out).is_ok() && !out.is_null() {
        Some(out)
    } else {
        None
    }
}

unsafe fn source_content_raw(source: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let vtbl: &com::IDesktopWindowXamlSourceVtbl = com::vtbl_of(source).ok()?;
    let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
    if (vtbl.get_content)(source, &mut out).is_ok() && !out.is_null() {
        Some(out)
    } else {
        None
    }
}
