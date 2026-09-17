//! Widget bar geometry.
//!
//! Pure arithmetic, no Windows types and no drawing: given a configuration,
//! some measured text metrics and a DPI, produce every rectangle the renderer
//! and the hit-tester need. Keeping it separate is what makes the adaptive
//! width — the fiddliest part of the whole widget — testable without a screen.
//!
//! All inputs and outputs are **physical pixels**. The logical sizes below are
//! the design's 96-DPI dimensions and get scaled by [`Metrics::new`].

/// Vertical gap between the pill and the taskbar's top/bottom edge.
pub const BAR_INSET: f32 = 2.0;
/// Horizontal padding inside the pill.
pub const BAR_PAD: f32 = 6.0;
/// Launcher button edge length.
pub const LAUNCHER: f32 = 34.0;
/// Gap between the launcher and the audio component.
pub const LAUNCHER_GAP: f32 = 6.0;
/// Gap between the elements inside the audio component.
pub const AUDIO_GAP: f32 = 8.0;
/// Album art edge length.
pub const COVER: f32 = 26.0;
/// Horizontal padding inside a transport button.
pub const BUTTON: f32 = 24.0;
/// The play/pause button is slightly larger.
pub const BUTTON_PRIMARY: f32 = 28.0;
/// Spectrum canvas height.
pub const SPECTRUM_HEIGHT: f32 = 22.0;
/// The spectrum always shows this many bands. It is not a setting: the bar is
/// small enough that a different count only ever makes it look worse.
///
/// Aliased to the analyser's constant rather than repeated, so a change cannot
/// leave the renderer drawing bars no frame ever fills.
pub const SPECTRUM_BARS: usize = beautify_core::model::SPECTRUM_BANDS;
/// Width of one band, in 96-DPI pixels. Deliberately slim — at this size a
/// wider band reads as a block rather than as a level meter.
const SPECTRUM_BAR_WIDTH: f32 = 2.5;
/// Gap between bands.
const SPECTRUM_BAR_GAP: f32 = 1.5;
/// Minimum height of the pill, so a 1-px taskbar cannot produce a 0-px window.
pub const MIN_BAR_HEIGHT: f32 = 26.0;
/// Bar width is only recomputed when it changes by at least this much.
pub const WIDTH_EPSILON: f32 = 2.0;

/// Which edge of the window the pill is pinned to.
///
/// The bar animates its width, so "which way does it grow" is really "which
/// edge stays put" — the anchor edge. `End` is the `bottom-right` case from the
/// spec, where the bar grows leftwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Rect {
    pub const ZERO: Rect = Rect {
        left: 0.0,
        top: 0.0,
        right: 0.0,
        bottom: 0.0,
    };

    pub fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Self {
            left,
            top,
            right,
            bottom,
        }
    }

    pub fn width(&self) -> f32 {
        self.right - self.left
    }

    pub fn height(&self) -> f32 {
        self.bottom - self.top
    }

    pub fn center_y(&self) -> f32 {
        (self.top + self.bottom) * 0.5
    }

    /// True when `(x, y)` — in the same coordinate space — is inside.
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// DPI scaling, applied once to the 96-DPI design constants.
#[derive(Debug, Clone, Copy)]
pub struct Metrics {
    pub scale: f32,
}

impl Metrics {
    pub fn new(dpi: u32) -> Self {
        Self {
            scale: dpi as f32 / 96.0,
        }
    }

    fn px(&self, logical: f32) -> f32 {
        logical * self.scale
    }

    pub fn inset(&self) -> f32 {
        self.px(BAR_INSET)
    }

    pub fn pad(&self) -> f32 {
        self.px(BAR_PAD)
    }

    pub fn launcher(&self) -> f32 {
        self.px(LAUNCHER)
    }

    pub fn gap(&self) -> f32 {
        self.px(LAUNCHER_GAP)
    }

    pub fn audio_gap(&self) -> f32 {
        self.px(AUDIO_GAP)
    }

    pub fn cover(&self) -> f32 {
        self.px(COVER)
    }

    pub fn button(&self) -> f32 {
        self.px(BUTTON)
    }

    pub fn button_primary(&self) -> f32 {
        self.px(BUTTON_PRIMARY)
    }

    pub fn spectrum_height(&self) -> f32 {
        self.px(SPECTRUM_HEIGHT)
    }
}

/// What the widget currently has to show. Drives which parts are laid out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Content {
    /// A media session exists and the media module is enabled.
    pub audio: bool,
    pub show_spectrum: bool,
    pub show_lyrics: bool,
    pub show_badge: bool,
}

/// Widths the layout needs from outside.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Bounds on the whole audio component.
    pub audio_min: f32,
    pub audio_max: f32,
    /// Bounds on the measured lyric. The component hugs the lyric between
    /// these, which is what makes the bar grow and shrink with the song.
    pub lyric_min: f32,
    pub lyric_max: f32,
}

impl Limits {
    pub fn from_config(cfg: &beautify_core::Config) -> Self {
        let w = &cfg.widget;
        Self {
            audio_min: w.audio_min_width as f32,
            audio_max: w.audio_max_width as f32,
            lyric_min: w.lyric_min_width as f32,
            lyric_max: w.lyric_max_width as f32,
        }
    }
}

/// Every rectangle the widget draws into or reacts to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    /// The pill itself, inside the window.
    pub pill: Rect,
    pub launcher: Rect,
    /// Present only when there is an audio session to show.
    pub audio: Option<AudioLayout>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioLayout {
    pub cover: Rect,
    /// The lyric / transport-control slot. Both states share it, so hovering
    /// never resizes the bar.
    pub slot: Rect,
    pub spectrum: Rect,
}

/// The three transport buttons, in hit-test order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Previous,
    Toggle,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControlRects {
    pub previous: Rect,
    pub toggle: Rect,
    pub next: Rect,
}

impl Layout {
    /// Centre of the audio slot, where the transport controls live.
    pub fn controls(&self, metrics: &Metrics) -> Option<ControlRects> {
        let audio = self.audio?;
        let gap = metrics.px(2.0);
        let button = metrics.button();
        let primary = metrics.button_primary();
        let total = button * 2.0 + primary + gap * 2.0;
        let left = audio.slot.left + (audio.slot.width() - total) * 0.5;
        let center_y = audio.slot.center_y();

        let make = |x: f32, size: f32| {
            Rect::new(
                x,
                center_y - size * 0.5,
                x + size,
                center_y + size * 0.5,
            )
        };
        let previous = make(left, button);
        let toggle = make(previous.right + gap, primary);
        let next = make(toggle.right + gap, button);
        Some(ControlRects {
            previous,
            toggle,
            next,
        })
    }

    /// Which element is under the pointer. `None` for the pill's own body.
    pub fn hit(&self, metrics: &Metrics, x: f32, y: f32) -> Option<Hit> {
        if self.launcher.contains(x, y) {
            return Some(Hit::Launcher);
        }
        let audio = self.audio?;
        if let Some(controls) = self.controls(metrics) {
            if controls.previous.contains(x, y) {
                return Some(Hit::Transport(Transport::Previous));
            }
            if controls.toggle.contains(x, y) {
                return Some(Hit::Transport(Transport::Toggle));
            }
            if controls.next.contains(x, y) {
                return Some(Hit::Transport(Transport::Next));
            }
        }
        if audio.cover.contains(x, y) {
            return Some(Hit::Cover);
        }
        if audio.slot.contains(x, y) {
            return Some(Hit::Slot);
        }
        if audio.spectrum.contains(x, y) {
            return Some(Hit::Spectrum);
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Launcher,
    Cover,
    Slot,
    Spectrum,
    Transport(Transport),
}

impl Hit {
    /// Is this anywhere inside the audio component?
    ///
    /// The component swaps its lyric for the transport controls as one unit —
    /// entering over the cover or the spectrum has to count too, or the
    /// controls only ever appear when the pointer happens to arrive over the
    /// text.
    pub const fn is_audio(self) -> bool {
        matches!(
            self,
            Hit::Cover | Hit::Slot | Hit::Spectrum | Hit::Transport(_)
        )
    }
}

/// Width of the lyric slot.
///
/// The bar hugs the lyric — that is the whole point of the adaptive width — but
/// never narrower than the transport controls, so switching to the hover state
/// cannot clip them or resize the bar.
pub fn slot_width(limits: &Limits, metrics: &Metrics, lyric_width: f32) -> f32 {
    let controls = metrics.button() * 2.0 + metrics.button_primary() + metrics.px(2.0) * 2.0;
    lyric_width
        .clamp(limits.lyric_min, limits.lyric_max)
        .max(controls)
}

/// Width the audio component wants, before clamping.
fn natural_audio_width(
    content: &Content,
    limits: &Limits,
    metrics: &Metrics,
    lyric_width: f32,
    spectrum_width: f32,
) -> f32 {
    if !content.audio {
        return 0.0;
    }
    let mut width = metrics.cover() + metrics.audio_gap() + slot_width(limits, metrics, lyric_width);
    if content.show_spectrum && spectrum_width > 0.0 {
        width += metrics.audio_gap() + spectrum_width;
    }
    width
}

/// Widest the spectrum ever gets: a fixed number of slim bands.
pub fn spectrum_width(metrics: &Metrics, shown: bool) -> f32 {
    if !shown {
        return 0.0;
    }
    let logical =
        SPECTRUM_BARS as f32 * SPECTRUM_BAR_WIDTH + (SPECTRUM_BARS - 1) as f32 * SPECTRUM_BAR_GAP;
    logical * metrics.scale
}

/// Width and gap of a single band, so the renderer and the layout agree.
pub fn spectrum_band(metrics: &Metrics) -> (f32, f32) {
    (
        SPECTRUM_BAR_WIDTH * metrics.scale,
        SPECTRUM_BAR_GAP * metrics.scale,
    )
}

/// Total bar width, in physical pixels, for the current content.
pub fn bar_width(content: &Content, limits: &Limits, metrics: &Metrics, lyric_width: f32) -> f32 {
    let spect = spectrum_width(metrics, content.audio && content.show_spectrum);
    let natural = natural_audio_width(content, limits, metrics, lyric_width, spect);
    let audio = if content.audio {
        natural.clamp(limits.audio_min, limits.audio_max)
    } else {
        0.0
    };

    let mut width = metrics.launcher() + 2.0 * metrics.pad();
    if content.audio {
        width += metrics.gap() + audio;
    }
    width
}

/// The widest the bar can ever get, used to size the window once.
///
/// The window is created at this width and never resized: the pill is drawn
/// right-aligned inside it and animates its own width. Because a layered window
/// hit-tests by alpha, the transparent margin passes clicks straight through to
/// the taskbar, so parking a wider window over it costs nothing.
pub fn max_bar_width(content: &Content, limits: &Limits, metrics: &Metrics) -> f32 {
    let mut width = metrics.launcher() + 2.0 * metrics.pad();
    if content.audio {
        width += metrics.gap() + limits.audio_max;
    }
    width
}

/// Lay out the pill and its contents inside `window`.
///
/// `bar_width` is the animating width; the pill's anchor edge is pinned to the
/// corresponding edge of `window`, so the bar grows away from the anchor.
pub fn layout(
    window: Rect,
    bar_width: f32,
    align: Align,
    content: &Content,
    metrics: &Metrics,
) -> Layout {
    let inset = metrics.inset();
    let pad = metrics.pad();
    let gap = metrics.audio_gap();
    let cover_w = metrics.cover();

    let usable = (window.width() - 2.0 * inset).max(0.0);
    let width = bar_width.min(usable);
    let left = match align {
        Align::Start => window.left + inset,
        Align::Center => window.left + (window.width() - width) * 0.5,
        Align::End => window.right - inset - width,
    };
    let pill = Rect::new(
        left,
        window.top + inset,
        left + width,
        (window.bottom - inset).max(window.top + inset),
    );

    let launcher = Rect::new(
        pill.left + pad,
        pill.center_y() - metrics.launcher() * 0.5,
        pill.left + pad + metrics.launcher(),
        pill.center_y() + metrics.launcher() * 0.5,
    );

    if !content.audio {
        return Layout {
            pill,
            launcher,
            audio: None,
        };
    }

    let audio_left = launcher.right + metrics.gap();
    let audio_width = (pill.right - pad - audio_left).max(0.0);

    // The spectrum is a fixed size, so it is placed first and the lyric slot
    // takes whatever is left. The other order is what lets the spectrum poke
    // out of the pill while the bar is mid-animation.
    let requested = spectrum_width(metrics, content.show_spectrum);
    let spectrum_w = requested.min((audio_width - cover_w - 2.0 * gap).max(0.0));
    let slot_w =
        (audio_width - cover_w - gap - if spectrum_w > 0.0 { gap + spectrum_w } else { 0.0 }).max(0.0);

    let cover = Rect::new(
        audio_left,
        pill.center_y() - cover_w * 0.5,
        audio_left + cover_w,
        pill.center_y() + cover_w * 0.5,
    );
    let slot_left = cover.right + gap;
    let slot_right = slot_left + slot_w;
    let spectrum = if spectrum_w > 0.0 {
        Rect::new(
            slot_right + gap,
            pill.center_y() - metrics.spectrum_height() * 0.5,
            slot_right + gap + spectrum_w,
            pill.center_y() + metrics.spectrum_height() * 0.5,
        )
    } else {
        Rect::new(slot_right, pill.center_y(), slot_right, pill.center_y())
    };

    Layout {
        pill,
        launcher,
        audio: Some(AudioLayout {
            cover,
            slot: Rect::new(slot_left, pill.top, slot_right, pill.bottom),
            spectrum,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> Metrics {
        Metrics::new(96)
    }

    fn limits() -> Limits {
        Limits {
            audio_min: 168.0,
            audio_max: 420.0,
            lyric_min: 96.0,
            lyric_max: 280.0,
        }
    }

    fn content(audio: bool) -> Content {
        Content {
            audio,
            show_spectrum: true,
            show_lyrics: true,
            show_badge: true,
        }
    }

    /// Lay `c` out inside a window sized for the widest possible bar, the way
    /// the real window is created once and never resized.
    fn lay(c: &Content, lyric: f32) -> (Layout, Metrics, Limits, f32) {
        lay_aligned(c, lyric, Align::End)
    }

    fn lay_aligned(c: &Content, lyric: f32, align: Align) -> (Layout, Metrics, Limits, f32) {
        let m = metrics();
        let l = limits();
        let width = bar_width(c, &l, &m, lyric);
        let window = Rect::new(0.0, 0.0, max_bar_width(c, &l, &m) + 2.0 * BAR_INSET, 44.0);
        (layout(window, width, align, c, &m), m, l, width)
    }

    #[test]
    fn no_audio_shows_only_the_launcher() {
        let (l, _, _, width) = lay(&content(false), 0.0);
        assert!(l.audio.is_none());
        assert_eq!(l.launcher.width(), LAUNCHER);
        assert!((width - (LAUNCHER + 2.0 * BAR_PAD)).abs() < 0.01);
    }

    #[test]
    fn the_spectrum_is_a_fixed_number_of_slim_bands() {
        let m = metrics();
        let width = spectrum_width(&m, true);
        let (bar, gap) = spectrum_band(&m);
        let expected = SPECTRUM_BARS as f32 * bar + (SPECTRUM_BARS - 1) as f32 * gap;
        assert!((width - expected).abs() < 0.01);
        assert!(bar <= 3.0, "a band this wide reads as a block, not a level");
        assert_eq!(spectrum_width(&m, false), 0.0);
    }

    #[test]
    fn the_slot_absorbs_the_spectrum_so_it_reaches_the_right_edge() {
        let (l, _, _, _) = lay(&content(true), 40.0);
        let audio = l.audio.unwrap();

        // Dead space between the spectrum and the pill's inner edge is exactly
        // the defect this layout exists to avoid.
        let slack = l.pill.right - BAR_PAD - audio.spectrum.right;
        assert!(slack.abs() < 0.51, "unexpected slack of {slack}");
    }

    #[test]
    fn a_long_lyric_is_clamped_to_the_configured_maximum() {
        let (_, _, l, width) = lay(&content(true), 5000.0);
        assert!(width <= l.audio_max + LAUNCHER + 2.0 * BAR_PAD + 1.0);
        assert!(width > l.audio_min);
    }

    #[test]
    fn the_bar_follows_the_lyric() {
        // The whole point of the adaptive width: a longer line makes a wider
        // bar, up to the configured maximum.
        let (_, _, _, short) = lay(&content(true), 100.0);
        let (_, _, _, long) = lay(&content(true), 240.0);
        assert!(
            long > short,
            "a 240px lyric should widen the bar over a 100px one, got {short} -> {long}"
        );
    }

    #[test]
    fn a_very_short_lyric_still_leaves_room_for_the_controls() {
        // Switching to the hover state must not clip the transport buttons, so
        // the slot never goes below their width.
        let (l, m, _, _) = lay(&content(true), 0.0);
        let slot = l.audio.unwrap().slot;
        let controls = l.controls(&m).unwrap();
        assert!(
            slot.width() >= controls.next.right - controls.previous.left - 0.01,
            "slot {} is narrower than the controls",
            slot.width()
        );
    }

    #[test]
    fn hovering_never_changes_the_width_because_both_states_share_the_slot() {
        let (l, m, _, _) = lay(&content(true), 10.0);
        let controls = l.controls(&m).unwrap();
        let slot = l.audio.unwrap().slot;

        assert!(controls.previous.left >= slot.left - 0.01);
        assert!(controls.next.right <= slot.right + 0.01);
    }

    #[test]
    fn hit_testing_finds_each_element() {
        let (layout, m, _, _) = lay(&content(true), 120.0);

        let launcher_center = (
            (layout.launcher.left + layout.launcher.right) * 0.5,
            layout.launcher.center_y(),
        );
        assert_eq!(
            layout.hit(&m, launcher_center.0, launcher_center.1),
            Some(Hit::Launcher)
        );

        let audio = layout.audio.unwrap();
        assert_eq!(
            layout.hit(&m, audio.cover.left + 1.0, audio.cover.center_y()),
            Some(Hit::Cover)
        );
        assert_eq!(
            layout.hit(&m, audio.spectrum.left + 1.0, audio.spectrum.center_y()),
            Some(Hit::Spectrum)
        );

        let controls = layout.controls(&m).unwrap();
        assert_eq!(
            layout.hit(&m, controls.toggle.left + 1.0, controls.toggle.center_y()),
            Some(Hit::Transport(Transport::Toggle))
        );
        assert_eq!(
            layout.hit(&m, controls.next.left + 1.0, controls.next.center_y()),
            Some(Hit::Transport(Transport::Next))
        );
        assert_eq!(
            layout.hit(&m, controls.previous.left + 1.0, controls.previous.center_y()),
            Some(Hit::Transport(Transport::Previous))
        );

        // A point inside the pill but not on anything in particular.
        assert_eq!(
            layout.hit(&m, layout.pill.left + 1.0, layout.pill.top + 1.0),
            None
        );
    }

    #[test]
    fn every_part_of_the_audio_component_counts_as_hovering_it() {
        let (l, m, _, _) = lay(&content(true), 120.0);
        let audio = l.audio.unwrap();

        for (x, y) in [
            (audio.cover.left + 1.0, audio.cover.center_y()),
            (audio.slot.left + 1.0, audio.slot.center_y()),
            (audio.spectrum.left + 1.0, audio.spectrum.center_y()),
        ] {
            let hit = l.hit(&m, x, y).expect("inside the component");
            assert!(hit.is_audio(), "{hit:?} should count as audio hover");
        }

        let controls = l.controls(&m).unwrap();
        assert!(l
            .hit(&m, controls.toggle.left + 1.0, controls.toggle.center_y())
            .unwrap()
            .is_audio());

        // The launcher is not part of it.
        assert!(!Hit::Launcher.is_audio());
    }

    #[test]
    fn clicking_the_transparent_margin_hits_nothing() {
        let (l, m, _, _) = lay(&content(true), 40.0);
        // Left of the pill is the window's transparent margin; a missed hit
        // there is what lets the click fall through to the taskbar.
        assert_eq!(l.hit(&m, l.pill.left - 1.0, l.pill.center_y()), None);
    }

    #[test]
    fn content_never_overflows_even_when_the_bar_is_squeezed() {
        let m = metrics();
        let c = content(true);
        let window = Rect::new(0.0, 0.0, 200.0, 44.0);
        let layout = layout(window, 120.0, Align::End, &c, &m);
        let audio = layout.audio.unwrap();

        assert!(layout.pill.left >= window.left + BAR_INSET - 0.01);
        assert!(audio.slot.width() >= 0.0, "slot must not invert");
        assert!(
            audio.spectrum.right <= layout.pill.right - BAR_PAD + 0.51,
            "spectrum must stay inside the pill, got {} vs {}",
            audio.spectrum.right,
            layout.pill.right
        );
    }

    fn growth(align: Align, narrow: f32, wide: f32) -> (Layout, Layout) {
        let m = metrics();
        let c = content(true);
        let window = Rect::new(0.0, 0.0, 900.0, 44.0);
        (
            layout(window, narrow, align, &c, &m),
            layout(window, wide, align, &c, &m),
        )
    }

    #[test]
    fn an_end_anchor_grows_leftwards() {
        let (narrow, wide) = growth(Align::End, 200.0, 400.0);
        assert_eq!(narrow.pill.right, wide.pill.right, "the right edge is pinned");
        assert!(wide.pill.left < narrow.pill.left);
    }

    #[test]
    fn a_start_anchor_grows_rightwards() {
        let (narrow, wide) = growth(Align::Start, 200.0, 400.0);
        assert_eq!(narrow.pill.left, wide.pill.left, "the left edge is pinned");
        assert!(wide.pill.right > narrow.pill.right);
    }

    #[test]
    fn a_centre_anchor_grows_symmetrically() {
        let (narrow, wide) = growth(Align::Center, 200.0, 400.0);
        let centre = |l: &Layout| (l.pill.left + l.pill.right) * 0.5;
        assert!((centre(&narrow) - centre(&wide)).abs() < 0.01);
        assert!(wide.pill.left < narrow.pill.left);
        assert!(wide.pill.right > narrow.pill.right);
    }

    #[test]
    fn dpi_scales_every_dimension() {
        let m = Metrics::new(120); // 125%
        let l = limits();
        let c = content(false);
        let width = bar_width(&c, &l, &m, 0.0);
        let expected = (LAUNCHER + 2.0 * BAR_PAD) * 1.25;
        assert!((width - expected).abs() < 0.01);
    }
}
