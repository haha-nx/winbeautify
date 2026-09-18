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

/// Default-activate a runtime class and QI it to a flat interface from
/// [`crate::com`]. The returned raw pointer carries one reference.
unsafe fn activate_as(class: &str, iid: &GUID) -> WResult<*mut core::ffi::c_void> {
    let factory: IActivationFactory = match RoGetActivationFactory(&windows::core::HSTRING::from(class))
    {
        Ok(factory) => factory,
        Err(error) => {
            crate::logging::debug_log_fmt(format_args!("{class}: factory failed {error:?}"));
            return Err(error);
        }
    };
    let instance = match unsafe { factory.ActivateInstance() } {
        Ok(instance) => instance,
        Err(error) => {
            crate::logging::debug_log_fmt(format_args!("{class}: activate failed {error:?}"));
            return Err(error);
        }
    };
    match unsafe { com::qi_raw(&instance, iid) } {
        Some(raw) => Ok(raw),
        None => {
            crate::logging::debug_log_fmt(format_args!("{class}: QI for {iid:?} failed"));
            Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_NOINTERFACE,
            ))
        }
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
        crate::service::debug_log("loading Windows.UI.Xaml.dll");
        let module = LoadLibraryExW(
            windows::core::w!("Windows.UI.Xaml.dll"),
            None,
            LOAD_LIBRARY_FLAGS(0x0000_0800), // LOAD_LIBRARY_SEARCH_SYSTEM32
        )?;
        crate::service::debug_log_fmt(format_args!("Windows.UI.Xaml.dll at {:p}", module.0));
        let addr = GetProcAddress(
            module,
            windows::core::PCSTR(c"InitializeXamlDiagnosticsEx".as_ptr().cast()),
        )
        .ok_or_else(windows::core::Error::empty)?;
        crate::service::debug_log("InitializeXamlDiagnosticsEx resolved");
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

/// `VisualTreeHelper.GetChildrenCount` and `GetChild`, as owned children.
///
/// Used to find the taskbar's background rectangles by looking at the frame,
/// which is the only way to reach them when the TAP connects after the shell
/// built the tree: the framework only reports mutations from then on, so the
/// rectangles' "added" events are in the past.
pub unsafe fn children_of(object: *mut core::ffi::c_void, limit: usize) -> Vec<IUnknown> {
    let mut children = Vec::new();
    let Some(dependency_object) = com::qi_from_raw(object, &com::IID_IDEPENDENCY_OBJECT) else {
        return children;
    };
    let factory = match get_statics(
        "Windows.UI.Xaml.Media.VisualTreeHelper",
        &com::IID_IVISUAL_TREE_HELPER_STATICS,
    ) {
        Ok(factory) => factory,
        Err(_) => {
            com::release_raw(dependency_object);
            return children;
        }
    };
    let Ok(vtbl) = com::vtbl_of::<com::IVisualTreeHelperStaticsVtbl>(factory) else {
        com::release_raw(factory);
        com::release_raw(dependency_object);
        return children;
    };
    let mut count = 0i32;
    if (vtbl.get_children_count)(factory, dependency_object, &mut count).is_ok() {
        for index in 0..count.min(limit as i32) {
            let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
            if (vtbl.get_child)(factory, dependency_object, index, &mut out).is_ok() && !out.is_null()
            {
                if let Ok(child) = com::adopt(out) {
                    children.push(child);
                }
            }
        }
    }
    com::release_raw(factory);
    com::release_raw(dependency_object);
    children
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

/// `IShape.Stroke`, as an owned raw brush pointer (one reference).
pub unsafe fn stroke_of(shape: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let typed = com::qi_from_raw(shape, &com::IID_ISHAPE)?;
    let brush = match com::vtbl_of::<com::IShapeVtbl>(typed) {
        Ok(vtbl) => {
            let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
            if (vtbl.get_stroke)(typed, &mut out).is_ok() && !out.is_null() {
                Some(out)
            } else {
                None
            }
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    brush
}

/// `IShape.Stroke = brush` (null clears).
pub unsafe fn set_stroke(shape: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> bool {
    let Some(typed) = com::qi_from_raw(shape, &com::IID_ISHAPE) else {
        return false;
    };
    let ok = match com::vtbl_of::<com::IShapeVtbl>(typed) {
        Ok(vtbl) => (vtbl.put_stroke)(typed, brush).is_ok(),
        Err(_) => false,
    };
    com::release_raw(typed);
    ok
}

/// `IShape.StrokeThickness`, in DIPs.
pub unsafe fn stroke_thickness_of(shape: *mut core::ffi::c_void) -> Option<f64> {
    let typed = com::qi_from_raw(shape, &com::IID_ISHAPE)?;
    let thickness = match com::vtbl_of::<com::IShapeVtbl>(typed) {
        Ok(vtbl) => {
            let mut out = 0.0f64;
            (vtbl.get_stroke_thickness)(typed, &mut out).is_ok().then_some(out)
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    thickness
}

/// `IBorder.BorderBrush`, as an owned raw brush pointer (one reference).
pub unsafe fn border_brush_of(border: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let typed = com::qi_from_raw(border, &com::IID_IBORDER)?;
    let brush = match com::vtbl_of::<com::IBorderVtbl>(typed) {
        Ok(vtbl) => {
            let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
            if (vtbl.get_border_brush)(typed, &mut out).is_ok() && !out.is_null() {
                Some(out)
            } else {
                None
            }
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    brush
}

/// `IBorder.BorderBrush = brush` (null clears).
pub unsafe fn set_border_brush(border: *mut core::ffi::c_void, brush: *mut core::ffi::c_void) -> bool {
    let Some(typed) = com::qi_from_raw(border, &com::IID_IBORDER) else {
        return false;
    };
    let ok = match com::vtbl_of::<com::IBorderVtbl>(typed) {
        Ok(vtbl) => (vtbl.put_border_brush)(typed, brush).is_ok(),
        Err(_) => false,
    };
    com::release_raw(typed);
    ok
}

/// `IBorder.BorderThickness`, in DIPs.
pub unsafe fn border_thickness_of(border: *mut core::ffi::c_void) -> Option<com::Thickness> {
    let typed = com::qi_from_raw(border, &com::IID_IBORDER)?;
    let thickness = match com::vtbl_of::<com::IBorderVtbl>(typed) {
        Ok(vtbl) => {
            let mut out = com::Thickness::default();
            (vtbl.get_border_thickness)(typed, &mut out).is_ok().then_some(out)
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    thickness
}

/// `IControl.BorderThickness`, in DIPs.
pub unsafe fn control_border_thickness_of(
    control: *mut core::ffi::c_void,
) -> Option<com::Thickness> {
    let typed = com::qi_from_raw(control, &com::IID_ICONTROL)?;
    let thickness = match com::vtbl_of::<com::IControlVtbl>(typed) {
        Ok(vtbl) => {
            let mut out = com::Thickness::default();
            (vtbl.get_border_thickness)(typed, &mut out).is_ok().then_some(out)
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    thickness
}

/// `IControl.BorderBrush`, as an owned raw brush pointer (one reference).
pub unsafe fn control_border_brush_of(
    control: *mut core::ffi::c_void,
) -> Option<*mut core::ffi::c_void> {
    let typed = com::qi_from_raw(control, &com::IID_ICONTROL)?;
    let brush = match com::vtbl_of::<com::IControlVtbl>(typed) {
        Ok(vtbl) => {
            let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
            if (vtbl.get_border_brush)(typed, &mut out).is_ok() && !out.is_null() {
                Some(out)
            } else {
                None
            }
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    brush
}

/// `IControl.BorderBrush = brush` (null clears).
pub unsafe fn set_control_border_brush(
    control: *mut core::ffi::c_void,
    brush: *mut core::ffi::c_void,
) -> bool {
    let Some(typed) = com::qi_from_raw(control, &com::IID_ICONTROL) else {
        return false;
    };
    let ok = match com::vtbl_of::<com::IControlVtbl>(typed) {
        Ok(vtbl) => (vtbl.put_border_brush)(typed, brush).is_ok(),
        Err(_) => false,
    };
    com::release_raw(typed);
    ok
}

/// `IBorder.Background`, as an owned raw brush pointer (one reference). Read
/// only for diagnostics: the hairline hiding must never touch a border's
/// background, which on the frame-sized borders is the backdrop itself.
pub unsafe fn border_background_of(border: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    let typed = com::qi_from_raw(border, &com::IID_IBORDER)?;
    let brush = match com::vtbl_of::<com::IBorderVtbl>(typed) {
        Ok(vtbl) => {
            let mut out: *mut core::ffi::c_void = core::ptr::null_mut();
            if (vtbl.get_background)(typed, &mut out).is_ok() && !out.is_null() {
                Some(out)
            } else {
                None
            }
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    brush
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

/// `XamlReader.Load`: parse a XAML snippet into an object.
///
/// XAML types cannot be default-activated from a TAP — `ActivateInstance`
/// answers E_NOTIMPL for the whole framework — but the XAML reader constructs
/// them fine, which is how the acrylic brush for the taskbar material is
/// created. The returned pointer carries one reference.
pub unsafe fn load_xaml(xaml: &str) -> Option<*mut core::ffi::c_void> {
    let factory = get_statics(
        "Windows.UI.Xaml.Markup.XamlReader",
        &com::IID_IXAML_READER_STATICS,
    )
    .ok()?;
    let out = match com::vtbl_of::<com::IXamlReaderStaticsVtbl>(factory) {
        Ok(vtbl) => {
            let hstring = windows::core::HSTRING::from(xaml);
            let mut result: *mut core::ffi::c_void = core::ptr::null_mut();
            if (vtbl.load)(factory, hstring.as_ptr() as *mut u16, &mut result).is_ok()
                && !result.is_null()
            {
                Some(result)
            } else {
                None
            }
        }
        Err(_) => None,
    };
    com::release_raw(factory);
    if out.is_none() {
        crate::logging::debug_log("load_xaml: the reader rejected the snippet");
    }
    out
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

/// The runtime class name of a WinRT object, as the ABI reports it — the only
/// way to tell what a handle points at when it did not arrive in an event.
///
/// Goes through the crate's `IInspectable` rather than a hand-rolled vtable
/// slot: the ABI hands back an `HSTRING`, and freeing that as if it were a
/// `BSTR` (which is what a hand-rolled version did) corrupts the heap.
pub unsafe fn runtime_class_name(object: *mut core::ffi::c_void) -> Option<String> {
    use windows::core::IInspectable;
    if object.is_null() {
        return None;
    }
    // Borrowed interface: take a reference of our own to wrap it. `IUnknown` and
    // `IInspectable` are the same ABI pointer; the cast is the crate's own.
    com::add_ref_raw(object);
    let inspectable: IInspectable = com::adopt(object).ok()?.cast().ok()?;
    let name = inspectable.GetRuntimeClassName().ok();
    drop(inspectable);
    name.map(|name| name.to_string_lossy())
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

/// `ISolidColorBrush.Color`, for reading back what a rectangle is painted with.
pub unsafe fn solid_color_of(brush: *mut core::ffi::c_void) -> Option<Color> {
    let typed = com::qi_from_raw(brush, &com::IID_ISOLID_COLOR_BRUSH)?;
    let color = match com::vtbl_of::<com::ISolidColorBrushVtbl>(typed) {
        Ok(vtbl) => {
            let mut out = Color::from_argb(0);
            (vtbl.get_color)(typed, &mut out).is_ok().then_some(out)
        }
        Err(_) => None,
    };
    com::release_raw(typed);
    color
}

/// The brush a shape is filled with, as an owned pointer.
pub unsafe fn current_fill_of(shape: *mut core::ffi::c_void) -> Option<*mut core::ffi::c_void> {
    fill_of(shape)
}
