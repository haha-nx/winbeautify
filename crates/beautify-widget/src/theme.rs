//! Resolved colours for the widget bar.
//!
//! Configuration carries hex strings and a 0..1 opacity; the renderer wants
//! premultiplied-friendly floats. Resolving that here — rather than inside the
//! paint code — keeps the light/dark decisions testable without a device.

use beautify_core::geometry::Color;

/// Straight (non-premultiplied) RGBA in 0..1, which is what Direct2D takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const TRANSPARENT: Rgba = Rgba {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    };

    pub fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// From a config colour, with the alpha given separately so a single
    /// opacity slider can drive the tint.
    pub fn from_color(color: Color, alpha: f32) -> Self {
        Self {
            r: color.r as f32 / 255.0,
            g: color.g as f32 / 255.0,
            b: color.b as f32 / 255.0,
            a: alpha.clamp(0.0, 1.0),
        }
    }

    pub fn from_rgb(r: u8, g: u8, b: u8) -> Self {
        Self::new(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
    }

    pub fn with_alpha(self, alpha: f32) -> Self {
        Self {
            a: alpha.clamp(0.0, 1.0),
            ..self
        }
    }

    /// Blend `self` over `other` as if `other` were the backdrop.
    ///
    /// Used to preview what a translucent pill will look like against a known
    /// background, and to figure out whether the foreground needs to be light
    /// or dark.
    pub fn over(self, other: Rgba) -> Rgba {
        let a = self.a + other.a * (1.0 - self.a);
        if a <= f32::EPSILON {
            return Rgba::TRANSPARENT;
        }
        let mix = |f: f32, b: f32| (f * self.a + b * other.a * (1.0 - self.a)) / a;
        Rgba::new(mix(self.r, other.r), mix(self.g, other.g), mix(self.b, other.b), a)
    }

    /// Perceived luminance, 0..1.
    pub fn luminance(self) -> f32 {
        0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b
    }

    pub fn to_d2d(self) -> windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
        windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a,
        }
    }
}

/// Alpha of the hairline around the pill, *before* the pill's own opacity is
/// applied. The same on light and dark, because it is a contrast edge either
/// way rather than a tint.
const PILL_BORDER_ALPHA: f32 = 0.10;

/// Everything the paint code needs colour-wise, resolved once per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub pill: Rgba,
    /// Hairline inner stroke so the pill still reads on a bright wallpaper.
    pub pill_border: Rgba,
    pub foreground: Rgba,
    pub foreground_dim: Rgba,
    pub accent: Rgba,
    /// Text drawn on top of the accent fill.
    pub on_accent: Rgba,
    /// Overlay painted behind the launcher on hover.
    pub hover: Rgba,
    /// Overlay painted behind the launcher while the flyout is open.
    pub active: Rgba,
    /// Base colour of the spectrum bars; per-bar alpha is applied on top.
    pub spectrum: Rgba,
    /// True when the pill reads as light, so glyphs must go dark.
    pub light: bool,
}

impl Theme {
    pub fn resolve(cfg: &beautify_core::Config) -> Self {
        let widget = &cfg.widget;
        let pill = Rgba::from_color(widget.background, widget.opacity);
        let accent = Rgba::from_color(cfg.ui.accent, 1.0);

        // Sample the pill against mid grey to decide the foreground. The pill is
        // translucent and sits on whatever the user's taskbar happens to be, so
        // the decision has to reflect the composited result rather than the raw
        // colour — a light pill over a bright taskbar needs dark glyphs, and a
        // dark pill over a light taskbar still needs light ones.
        let backdrop = Rgba::from_rgb(0x80, 0x80, 0x80);
        let composited = pill.over(backdrop);
        let light = composited.luminance() > 0.5;

        let (foreground, foreground_dim) = if light {
            (Rgba::from_rgb(0x1b, 0x1f, 0x2a), Rgba::from_rgb(0x5a, 0x63, 0x76))
        } else {
            (Rgba::from_rgb(0xe9, 0xeb, 0xf2), Rgba::from_rgb(0x9b, 0xa3, 0xb7))
        };

        let overlay = if light {
            Rgba::from_rgb(0x00, 0x00, 0x00)
        } else {
            Rgba::from_rgb(0xff, 0xff, 0xff)
        };

        Self {
            pill,
            // Scaled by the pill's own alpha, not a fixed value. The stroke only
            // exists to separate the fill from a wallpaper of the same tone, so
            // it has to fade out with the fill: at zero opacity the pill is gone
            // and an unscaled hairline is left floating on the taskbar as a
            // stray ring around nothing.
            pill_border: overlay.with_alpha(PILL_BORDER_ALPHA * pill.a),
            foreground,
            foreground_dim,
            accent,
            on_accent: Rgba::from_rgb(0xff, 0xff, 0xff),
            hover: overlay.with_alpha(0.12),
            active: accent,
            spectrum: foreground,
            light,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beautify_core::config::Config;

    #[test]
    fn over_composites_like_alpha_blending() {
        let white = Rgba::from_rgb(255, 255, 255);
        let black_half = Rgba::from_rgb(0, 0, 0).with_alpha(0.5);
        let result = black_half.over(white);
        assert!((result.r - 0.5).abs() < 0.01, "50% black over white is mid grey");
        assert!((result.a - 1.0).abs() < 0.01);
    }

    #[test]
    fn a_transparent_layer_changes_nothing() {
        let backdrop = Rgba::from_rgb(10, 20, 30);
        assert_eq!(Rgba::TRANSPARENT.over(backdrop), backdrop);
    }

    #[test]
    fn default_config_resolves_to_a_dark_pill_with_light_glyphs() {
        let theme = Theme::resolve(&Config::default());
        assert!(!theme.light, "the shipped default background is dark");
        assert!(theme.foreground.luminance() > 0.6);
        assert!(theme.pill.a > 0.7, "the default pill must be opaque enough to read against");
    }

    #[test]
    fn a_dark_pill_stays_dark_over_a_bright_taskbar() {
        // The case that regressed: a translucent dark pill on a light taskbar
        // composites to mid grey, and light glyphs on mid grey are unreadable.
        let cfg = Config::default();
        let theme = Theme::resolve(&cfg);
        let on_white = theme.pill.over(Rgba::from_rgb(255, 255, 255));
        assert!(
            on_white.luminance() < 0.5,
            "default pill should still read as dark over a bright taskbar, got {}",
            on_white.luminance()
        );
    }

    #[test]
    fn a_light_pill_flips_the_foreground() {
        let mut cfg = Config::default();
        cfg.widget.background = beautify_core::geometry::Color::rgb(0xF2, 0xF3, 0xF6);
        cfg.widget.opacity = 0.95;
        let theme = Theme::resolve(&cfg);
        assert!(theme.light);
        assert!(theme.foreground.luminance() < 0.3, "dark glyphs on a light pill");
    }

    #[test]
    fn a_very_translucent_pill_is_judged_on_what_shows_through() {
        let mut cfg = Config::default();
        cfg.widget.background = beautify_core::geometry::Color::rgb(0xFF, 0xFF, 0xFF);
        cfg.widget.opacity = 0.08;
        let theme = Theme::resolve(&cfg);

        // At 8% the pill's own colour barely moves the composited result, which
        // is the whole point: the taskbar behind it decides the contrast.
        let composited = theme.pill.over(Rgba::from_rgb(0x80, 0x80, 0x80));
        assert!(
            (composited.luminance() - 0.5).abs() < 0.06,
            "a near-transparent pill should read as its backdrop, got {}",
            composited.luminance()
        );
    }

    #[test]
    fn accent_comes_from_the_ui_config() {
        let mut cfg = Config::default();
        cfg.ui.accent = beautify_core::geometry::Color::rgb(0xFF, 0x00, 0x00);
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.accent.r, 1.0);
        assert_eq!(theme.active, theme.accent);
    }

    /// The reported artifact: with the pill switched off, an unscaled hairline
    /// was still stroked around where the pill would be, so the bar showed a
    /// ring of border floating on the taskbar with nothing inside it.
    #[test]
    fn a_fully_transparent_pill_draws_no_border() {
        let mut cfg = Config::default();
        cfg.widget.opacity = 0.0;
        let theme = Theme::resolve(&cfg);
        assert_eq!(theme.pill.a, 0.0);
        assert_eq!(
            theme.pill_border.a, 0.0,
            "no fill means no edge to separate it from the wallpaper"
        );
    }

    #[test]
    fn the_border_fades_out_with_the_pill() {
        let mut cfg = Config::default();
        cfg.widget.opacity = 0.5;
        let half = Theme::resolve(&cfg).pill_border.a;
        cfg.widget.opacity = 1.0;
        let full = Theme::resolve(&cfg).pill_border.a;

        assert!(half > 0.0 && half < full, "half a pill gets a fainter edge");
        assert!((full - PILL_BORDER_ALPHA).abs() < 1e-6, "an opaque pill keeps the full hairline");
    }
}
