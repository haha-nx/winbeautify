//! The D2D effect graph behind Blur mode: backdrop → Gaussian blur, overlaid
//! with a flood of the tint colour.
//!
//! `Compositor.CreateEffectFactory` walks an `IGraphicsEffect` tree through
//! `IGraphicsEffectD2D1Interop`; the windows crate ships the interfaces but
//! no concrete effect classes, so the three nodes live here, mirroring the
//! property tables of `d2d1effects.h`.

use windows::core::{implement, HSTRING, IInspectable, Interface, Result, PCWSTR};
use windows::Foundation::{IPropertyValue, PropertyValue};
use windows::Graphics::Effects::{IGraphicsEffect, IGraphicsEffectSource};
use windows::Graphics::Effects::{IGraphicsEffect_Impl, IGraphicsEffectSource_Impl};
use windows::Win32::Graphics::Direct2D::{
    CLSID_D2D1Composite, CLSID_D2D1Flood, CLSID_D2D1GaussianBlur,
};
use windows::Win32::System::WinRT::Graphics::Direct2D::{
    GRAPHICS_EFFECT_PROPERTY_MAPPING, GRAPHICS_EFFECT_PROPERTY_MAPPING_DIRECT,
    IGraphicsEffectD2D1Interop, IGraphicsEffectD2D1Interop_Impl,
};

/// `d2d1effects.h` property indices.
const D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION: u32 = 0;
const D2D1_FLOOD_PROP_COLOR: u32 = 0;
const D2D1_COMPOSITE_PROP_MODE: u32 = 0;
/// `D2D1_COMPOSITE_MODE_SOURCE_OVER`.
const D2D1_COMPOSITE_MODE_SOURCE_OVER: u32 = 0;

fn single(value: f32) -> Result<IInspectable> {
    PropertyValue::CreateSingle(value)
}

fn u32_value(value: u32) -> Result<IInspectable> {
    PropertyValue::CreateUInt32(value)
}

fn single_array(values: &[f32]) -> Result<IInspectable> {
    PropertyValue::CreateSingleArray(values)
}

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

/// The per-node D2D contract, kept next to each struct.
trait EffectNode {
    fn get_property(&self, index: u32) -> Result<IInspectable>;
    fn get_source(&self, index: u32) -> Result<IGraphicsEffectSource>;
    fn source_count(&self) -> u32;
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
                let value = EffectNode::get_property(node, index)?;
                value.cast::<IPropertyValue>()
            }
            fn GetSource(&self, index: u32) -> Result<IGraphicsEffectSource> {
                let node: &$ty = self;
                EffectNode::get_source(node, index)
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

impl EffectNode for GaussianBlurEffect {
    fn get_property(&self, index: u32) -> Result<IInspectable> {
        if index == D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION {
            single(self.blur_amount)
        } else {
            // Optimization and BorderMode keep their D2D defaults; the
            // property table still has to answer them.
            u32_value(0)
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

/// `CLSID_D2D1Flood`: a solid `float4` RGBA colour.
#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
pub struct FloodEffect {
    name: String,
    /// RGBA, each component 0.0..=1.0.
    pub color: [f32; 4],
}

impl EffectNode for FloodEffect {
    fn get_property(&self, index: u32) -> Result<IInspectable> {
        if index == D2D1_FLOOD_PROP_COLOR {
            single_array(&self.color)
        } else {
            Err(windows::core::Error::empty())
        }
    }

    fn get_source(&self, _index: u32) -> Result<IGraphicsEffectSource> {
        Err(windows::core::Error::empty())
    }

    fn source_count(&self) -> u32 {
        0
    }
}

effect!(
    FloodEffect_Impl,
    FloodEffect,
    effect_id = CLSID_D2D1Flood,
    property_count = 1,
    properties = &[("Color", D2D1_FLOOD_PROP_COLOR)]
);

/// `CLSID_D2D1Composite`: stacks the flood on top of the blurred backdrop.
#[implement(IGraphicsEffect, IGraphicsEffectSource, IGraphicsEffectD2D1Interop)]
pub struct CompositeEffect {
    name: String,
    pub mode: u32,
    /// Inputs in D2D order: index 0 is the destination, index 1 the source.
    pub sources: Vec<IGraphicsEffectSource>,
}

impl EffectNode for CompositeEffect {
    fn get_property(&self, index: u32) -> Result<IInspectable> {
        if index == D2D1_COMPOSITE_PROP_MODE {
            u32_value(self.mode)
        } else {
            Err(windows::core::Error::empty())
        }
    }

    fn get_source(&self, index: u32) -> Result<IGraphicsEffectSource> {
        self.sources
            .get(index as usize)
            .cloned()
            .ok_or_else(windows::core::Error::empty)
    }

    fn source_count(&self) -> u32 {
        self.sources.len() as u32
    }
}

effect!(
    CompositeEffect_Impl,
    CompositeEffect,
    effect_id = CLSID_D2D1Composite,
    property_count = 1,
    properties = &[("Mode", D2D1_COMPOSITE_PROP_MODE)]
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

impl FloodEffect {
    pub fn new(color: [f32; 4]) -> Self {
        Self {
            name: "FloodEffect".into(),
            color,
        }
    }
}

impl CompositeEffect {
    pub fn new(sources: Vec<IGraphicsEffectSource>) -> Self {
        Self {
            name: "CompositeEffect".into(),
            mode: D2D1_COMPOSITE_MODE_SOURCE_OVER,
            sources,
        }
    }
}
