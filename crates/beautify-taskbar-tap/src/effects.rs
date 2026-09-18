//! The D2D effect graph behind Blur mode: a Gaussian blur over the backdrop.
//!
//! `Compositor.CreateEffectFactory` walks an `IGraphicsEffect` tree through
//! `IGraphicsEffectD2D1Interop`; the windows crate ships the interfaces but
//! no concrete effect classes, so the node lives here, mirroring the property
//! table of `d2d1effects.h`.
//!
//! The engine only accepts a fixed subset of D2D effects through the
//! composition factory — Gaussian blur is on it, but flood and colour-source
//! tint nodes are not, or not reliably, so tinting is done with the shell's
//! own `AcrylicBrush` in Service instead of a composite graph.

use windows::core::{implement, HSTRING, IInspectable, Interface, Result, PCWSTR};
use windows::Foundation::{IPropertyValue, PropertyValue};
use windows::Graphics::Effects::{IGraphicsEffect, IGraphicsEffectSource};
use windows::Graphics::Effects::{IGraphicsEffect_Impl, IGraphicsEffectSource_Impl};
use windows::Win32::Graphics::Direct2D::CLSID_D2D1GaussianBlur;
use windows::Win32::System::WinRT::Graphics::Direct2D::{
    GRAPHICS_EFFECT_PROPERTY_MAPPING, GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
    IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl,
};

/// `d2d1effects.h` property index.
const D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION: u32 = 0;

/// Look up a named property in a `(name, index)` table.
fn map_property(
    name: &PCWSTR,
    index: *mut u32,
    mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
    table: &[(&str, u32)],
) -> Result<()> {
    unsafe {
        let name = windows::core::HSTRING::from_wide(core::slice::from_raw_parts(
            name.0,
            (0..).take_while(|&i| *name.0.add(i) != 0).count(),
        ));
        let name = name.to_string_lossy();
        for (property, slot) in table {
            if name == *property {
                index.write(*slot);
                mapping.write(GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT);
                return Ok(());
            }
        }
        Err(windows::core::Error::empty())
    }
}

macro_rules! effect {
    ($impl_ty:ident, $ty:ident, effect_id = $clsid:expr, property_count = $count:expr, properties = $table:expr) => {
        impl IGraphicsEffect_Impl for $impl_ty {
            fn Name(&self) -> Result<HSTRING> {
                Ok(HSTRING::from(self.name.clone()))
            }
            fn SetName(&self, name: &HSTRING) -> Result<()> {
                // The framework never renames our nodes; keep the fixed name.
                let _ = name;
                Ok(())
            }
        }
        impl IGraphicsEffectSource_Impl for $impl_ty {}
        impl IGraphicsEffectD2D1Interop_Impl for $impl_ty {
            fn GetEffectId(&self) -> Result<windows::core::GUID> {
                Ok($clsid)
            }
            fn GetNamedPropertyMapping(
                &self,
                name: &PCWSTR,
                index: *mut u32,
                mapping: *mut GRAPHICS_EFFECT_PROPERTY_MAPPING,
            ) -> Result<()> {
                map_property(name, index, mapping, $table)
            }
            fn GetPropertyCount(&self) -> Result<u32> {
                Ok($count)
            }
            fn GetProperty(&self, index: u32) -> Result<IPropertyValue> {
                let node: &$ty = self;
                let value = node.get_property(index)?;
                value.cast::<IPropertyValue>()
            }
            fn GetSource(&self, index: u32) -> Result<IGraphicsEffectSource> {
                let node: &$ty = self;
                node.get_source(index)
            }
            fn GetSourceCount(&self) -> Result<u32> {
                let node: &$ty = self;
                Ok(node.source_count())
            }
        }
    };
}

/// `CLSID_D2D1GaussianBlur`; only the standard-deviation property matters.
#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
pub struct GaussianBlurEffect {
    name: String,
    pub blur_amount: f32,
    pub source: Option<IGraphicsEffectSource>,
}

impl GaussianBlurEffect {
    fn get_property(&self, index: u32) -> Result<IInspectable> {
        if index == D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION {
            PropertyValue::CreateSingle(self.blur_amount)
        } else {
            // Optimization and BorderMode keep their D2D defaults; the
            // property table still has to answer them.
            PropertyValue::CreateUInt32(0)
        }
    }

    fn get_source(&self, index: u32) -> Result<IGraphicsEffectSource> {
        match (index, self.source.clone()) {
            (0, Some(source)) => Ok(source),
            _ => Err(windows::core::Error::empty()),
        }
    }

    fn source_count(&self) -> u32 {
        1
    }
}

effect!(
    GaussianBlurEffect_Impl,
    GaussianBlurEffect,
    effect_id = CLSID_D2D1GaussianBlur,
    property_count = 3,
    properties = &[("BlurAmount", D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION)]
);

impl GaussianBlurEffect {
    pub fn new(blur_amount: f32, source: IGraphicsEffectSource) -> Self {
        Self {
            name: "GaussianBlurEffect".into(),
            blur_amount,
            source: Some(source),
        }
    }
}
