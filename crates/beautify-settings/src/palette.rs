//! Colours for the settings window.
//!
//! Resolved from the app's theme and accent, so the window matches the widget
//! bar and follows a theme change without being told twice.

use beautify_core::config::{Config, Theme};
use beautify_widget::theme::Rgba;

/// The colour set the painter draws from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Behind everything, including the sidebar gutter.
    pub window: Rgba,
    pub sidebar: Rgba,
    /// The card panels the rows sit on.
    pub card: Rgba,
    pub card_border: Rgba,
    /// Separator between rows inside a card.
    pub divider: Rgba,
    pub text: Rgba,
    /// Hints and secondary labels.
    pub text_dim: Rgba,
    /// Placeholders and disabled text.
    pub text_faint: Rgba,
    pub accent: Rgba,
    /// Text on top of the accent.
    pub on_accent: Rgba,
    /// Input controls: boxes, switches that are off, slider tracks.
    pub control: Rgba,
    pub control_border: Rgba,
    /// The title bar strip.
    pub titlebar: Rgba,
    /// Hover wash over a control.
    pub hover: Rgba,
    /// Destructive buttons.
    pub danger: Rgba,
    /// Status pill tones.
    pub ok: Rgba,
    pub warn: Rgba,
    /// True when the surface reads light, so text goes dark.
    pub light: bool,
}

/// A colour and an alpha, in the form the widget's theme module uses.
const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Rgba {
    Rgba {
        r: r as f32 / 255.0,
        g: g as f32 / 255.0,
        b: b as f32 / 255.0,
        a,
    }
}

impl Palette {
    /// Resolve for the configured theme.
    ///
    /// `system_is_light` is the Windows app theme, used when the config says to
    /// follow the system.
    pub fn resolve(config: &Config, system_is_light: bool) -> Self {
        let light = match config.ui.theme {
            Theme::Light => true,
            Theme::Dark => false,
            Theme::Auto => system_is_light,
        };
        let accent = Rgba::from_color(config.ui.accent, 1.0);

        if light {
            Self {
                window: rgba(0xF4, 0xF5, 0xF8, 1.0),
                sidebar: rgba(0xEA, 0xEC, 0xF1, 1.0),
                card: rgba(0xFF, 0xFF, 0xFF, 1.0),
                card_border: rgba(0x00, 0x00, 0x00, 0.08),
                divider: rgba(0x00, 0x00, 0x00, 0.07),
                text: rgba(0x1B, 0x1F, 0x2A, 1.0),
                text_dim: rgba(0x5A, 0x63, 0x76, 1.0),
                text_faint: rgba(0x8B, 0x93, 0xA5, 1.0),
                accent,
                on_accent: rgba(0xFF, 0xFF, 0xFF, 1.0),
                control: rgba(0xFF, 0xFF, 0xFF, 1.0),
                control_border: rgba(0x00, 0x00, 0x00, 0.16),
                titlebar: rgba(0xEA, 0xEC, 0xF1, 1.0),
                hover: rgba(0x00, 0x00, 0x00, 0.06),
                danger: rgba(0xD1, 0x3B, 0x3B, 1.0),
                ok: rgba(0x1F, 0x8A, 0x4C, 1.0),
                warn: rgba(0xB4, 0x7A, 0x0B, 1.0),
                light: true,
            }
        } else {
            Self {
                window: rgba(0x1B, 0x1D, 0x23, 1.0),
                sidebar: rgba(0x16, 0x18, 0x1D, 1.0),
                card: rgba(0x24, 0x27, 0x2E, 1.0),
                card_border: rgba(0xFF, 0xFF, 0xFF, 0.07),
                divider: rgba(0xFF, 0xFF, 0xFF, 0.06),
                text: rgba(0xE9, 0xEB, 0xF2, 1.0),
                text_dim: rgba(0x9B, 0xA3, 0xB7, 1.0),
                text_faint: rgba(0x6E, 0x76, 0x88, 1.0),
                accent,
                on_accent: rgba(0xFF, 0xFF, 0xFF, 1.0),
                control: rgba(0x2E, 0x32, 0x3B, 1.0),
                control_border: rgba(0xFF, 0xFF, 0xFF, 0.12),
                titlebar: rgba(0x16, 0x18, 0x1D, 1.0),
                hover: rgba(0xFF, 0xFF, 0xFF, 0.07),
                danger: rgba(0xE0, 0x5A, 0x5A, 1.0),
                ok: rgba(0x4C, 0xC0, 0x7A, 1.0),
                warn: rgba(0xE0, 0xA9, 0x4B, 1.0),
                light: false,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_setting_picks_the_surface() {
        let mut config = Config::default();
        config.ui.theme = Theme::Dark;
        assert!(!Palette::resolve(&config, true).light, "an explicit dark wins");

        config.ui.theme = Theme::Light;
        assert!(Palette::resolve(&config, false).light, "an explicit light wins");

        config.ui.theme = Theme::Auto;
        assert!(Palette::resolve(&config, true).light);
        assert!(!Palette::resolve(&config, false).light);
    }

    #[test]
    fn text_contrasts_with_the_surface_it_sits_on() {
        for light in [true, false] {
            let mut config = Config::default();
            config.ui.theme = if light { Theme::Light } else { Theme::Dark };
            let palette = Palette::resolve(&config, light);
            let text = palette.text.luminance();
            let surface = palette.card.luminance();
            assert!(
                (text - surface).abs() > 0.5,
                "text {text} on card {surface} is not readable"
            );
            // Hints are dimmer than the label but still not the same tone.
            assert!(palette.text_dim.luminance() != text);
        }
    }

    #[test]
    fn the_accent_comes_from_the_config() {
        let mut config = Config::default();
        config.ui.accent = beautify_core::geometry::Color::rgb(0xFF, 0x00, 0x00);
        let palette = Palette::resolve(&config, false);
        assert_eq!(palette.accent.r, 1.0);
        assert_eq!(palette.accent.g, 0.0);
    }

    #[test]
    fn the_danger_tone_is_distinguishable_from_the_accent() {
        let palette = Palette::resolve(&Config::default(), false);
        assert!((palette.danger.r - palette.accent.r).abs() > 0.1);
    }
}
