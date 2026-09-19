//! The scrollbar drag: grabbing the thumb, following the pointer, and the
//! glide that keeps the list moving after a flick.
//!
//! The settings window and the flyout panel each draw their own scrollbar out
//! of their own layout, but what the thumb *does* is one behaviour, so it is
//! one state machine here where both can use it and neither can drift. Nothing
//! here touches a window or a device: every method is arithmetic on floats and
//! [`Instant`]s, which is what makes it testable.
//!
//! # The mapping
//!
//! Pointer travel and content travel are linked by geometry, not by a tuned
//! constant. The thumb's grabbed point follows the pointer, and the content
//! offset is wherever that puts the thumb inside the track — so a thumb that
//! has half the track to itself moves the content two pixels for every one
//! the pointer moves, and the whole range is always reachable however short
//! the track. Time is only read to measure velocity; position is never
//! interpolated, so the thumb cannot lag the hand.
//!
//! # The glide
//!
//! While dragging, a running estimate tracks how fast the content is moving.
//! Releasing above a stop threshold starts a *glide*: the same speed decays
//! exponentially, the flick's tail, until it drops under the threshold or the
//! content runs into either end of its range. The caller drives the glide
//! from a timer, one [`ScrollDrag::tick`] per beat, until it answers `false`.

use std::time::{Duration, Instant};

/// A glide below this speed stops, in content pixels per second.
const STOP_SPEED: f32 = 24.0;
/// No release starts a glide faster than this, in content pixels per second.
const MAX_SPEED: f32 = 6000.0;
/// How much of each new velocity sample the running estimate takes on. The
/// rest keeps the older samples, so one jittery move cannot kick the glide.
const SMOOTHING: f32 = 0.4;
/// The glide's half-life, in seconds: this much time cuts the speed in half.
const HALF_LIFE: f32 = 0.16;
/// The longest gap between ticks the glide believes in. A stalled timer must
/// not fling the list the rest of the way in one step.
const MAX_TICK: Duration = Duration::from_millis(100);
/// Moves closer together than this are one sample, not two. A `dt` of zero
/// would be divided by.
const MIN_SAMPLE: Duration = Duration::from_millis(1);
/// The longest pause the drag still calls a flick. No mouse moves arrive
/// while the hand rests, so the velocity estimate cannot decay on its own —
/// held longer than this, the release is a stop and the next move is a new
/// measurement, not a continuation of the old speed.
const STATIONARY: Duration = Duration::from_millis(120);

/// Whether the scrollbar thumb is being dragged, and how a release should
/// glide.
///
/// One per window. The caller keeps the scroll offset; this only ever moves
/// it through [`ScrollDrag::drag_to`] and [`ScrollDrag::tick`].
#[derive(Debug, Clone, Default)]
pub struct ScrollDrag {
    /// The thumb is grabbed: between button-down and button-up.
    dragging: bool,
    /// Where inside the thumb the pointer grabbed it, measured from the
    /// thumb's top edge. Held constant for the drag, so the thumb does not
    /// leap to centre itself under the pointer.
    grab: f32,
    /// Running estimate of the drag's speed, in content pixels per second,
    /// positive when the content is moving toward its end.
    velocity: f32,
    /// When the last velocity sample was taken.
    last_sample: Option<Instant>,
    /// The offset the last sample saw; its delta against the next one is the
    /// sample. `NaN` until the first move, which has nothing to differ from.
    last_scroll: f32,
    /// Released with speed: the glide is running.
    gliding: bool,
    /// When the last glide tick ran.
    last_tick: Option<Instant>,
}

impl ScrollDrag {
    /// Is the thumb grabbed right now?
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// Is a glide running right now?
    pub fn is_gliding(&self) -> bool {
        self.gliding
    }

    /// Grab the thumb whose top edge sits at `thumb_top`, with the pointer at
    /// `pointer_y`. Any glide still under way stops: the hand is on the list.
    pub fn begin(&mut self, pointer_y: f32, thumb_top: f32) {
        self.dragging = true;
        self.grab = (pointer_y - thumb_top).max(0.0);
        self.velocity = 0.0;
        self.last_sample = Some(Instant::now());
        self.last_scroll = f32::NAN;
        self.gliding = false;
        self.last_tick = None;
    }

    /// Move the grabbed point to `pointer_y` and answer the offset that puts
    /// the thumb there, clamped to `0..scroll_max` so the drag can follow the
    /// pointer outside the track without the content leaving its range.
    ///
    /// `track_top`/`track_height` are the track's span and `thumb_height` the
    /// thumb's, all in the caller's vertical coordinate.
    pub fn drag_to(
        &mut self,
        pointer_y: f32,
        track_top: f32,
        track_height: f32,
        thumb_height: f32,
        scroll_max: f32,
    ) -> f32 {
        self.drag_to_at(
            Instant::now(),
            pointer_y,
            track_top,
            track_height,
            thumb_height,
            scroll_max,
        )
    }

    /// Let go of the thumb. Answers whether a glide starts — the caller then
    /// beats [`ScrollDrag::tick`] from a timer until it answers `false`.
    ///
    /// A hand that was resting — no move for [`STATIONARY`] — releases into a
    /// stop, not a glide. The estimate only moves when the pointer does, so
    /// the speed of the last move would otherwise survive an indefinite
    /// pause and fling the list the moment the button came up.
    pub fn release(&mut self) -> bool {
        self.release_at(Instant::now())
    }

    /// [`ScrollDrag::release`] at a synthetic time, so tests do not sleep.
    fn release_at(&mut self, now: Instant) -> bool {
        self.dragging = false;
        let resting = self
            .last_sample
            .is_some_and(|last| now.duration_since(last) >= STATIONARY);
        self.last_sample = None;
        if resting {
            self.velocity = 0.0;
        }
        self.gliding = self.velocity.is_finite() && self.velocity.abs() >= STOP_SPEED;
        if self.gliding {
            self.velocity = self.velocity.clamp(-MAX_SPEED, MAX_SPEED);
            self.last_tick = Some(now);
        }
        self.gliding
    }

    /// Advance the glide by however long has passed since the last tick,
    /// moving `scroll` in place and clamping it to `0..scroll_max`.
    ///
    /// Answers `false` — once — when the glide is over: it ran out of speed,
    /// or it reached the end of the range its speed was headed for. That is
    /// the caller's cue to stop the timer.
    pub fn tick(&mut self, scroll: &mut f32, scroll_max: f32) -> bool {
        self.tick_at(Instant::now(), scroll, scroll_max)
    }

    /// Drop whatever is happening — a drag, or a glide still under way. The
    /// caller should also stop its timer; a `tick` would answer `false`
    /// anyway.
    pub fn stop(&mut self) {
        self.dragging = false;
        self.gliding = false;
        self.velocity = 0.0;
        self.last_sample = None;
        self.last_tick = None;
    }

    /// [`ScrollDrag::drag_to`] at a synthetic time, so tests do not sleep.
    fn drag_to_at(
        &mut self,
        now: Instant,
        pointer_y: f32,
        track_top: f32,
        track_height: f32,
        thumb_height: f32,
        scroll_max: f32,
    ) -> f32 {
        let travel = (track_height - thumb_height).max(1.0);
        let scroll = (((pointer_y - self.grab) - track_top) / travel * scroll_max)
            .clamp(0.0, scroll_max.max(0.0));
        self.sample(scroll, now);
        scroll
    }

    /// Blend one velocity sample in, from the offset just reached at `now`.
    ///
    /// The ratio never comes in as an argument: `drag_to` answers *content*
    /// pixels, so the deltas are already in content units and the estimate is
    /// in content pixels per second for free.
    fn sample(&mut self, scroll: f32, now: Instant) {
        let Some(last) = self.last_sample else {
            return;
        };
        let elapsed = now.duration_since(last);
        if elapsed < MIN_SAMPLE {
            // Too close to the previous move to be its own sample: the next
            // one's `dt` quietly includes this gap.
            return;
        }
        if !self.last_scroll.is_nan() {
            let instant = (scroll - self.last_scroll) / elapsed.as_secs_f32();
            // A move after a long pause is a new measurement. Blending it
            // with the old estimate would let the speed from before the
            // pause survive a hand that has since been still.
            self.velocity = if self.velocity == 0.0 || elapsed >= STATIONARY {
                instant
            } else {
                self.velocity * (1.0 - SMOOTHING) + instant * SMOOTHING
            };
        }
        self.last_scroll = scroll;
        self.last_sample = Some(now);
    }

    /// [`ScrollDrag::tick`] at a synthetic time, so tests do not sleep.
    fn tick_at(&mut self, now: Instant, scroll: &mut f32, scroll_max: f32) -> bool {
        if !self.gliding {
            return false;
        }
        let dt = self
            .last_tick
            .map_or(MAX_TICK, |last| now.duration_since(last))
            .min(MAX_TICK);
        self.last_tick = Some(now);
        let dt = dt.as_secs_f32();

        let moved = *scroll + self.velocity * dt;
        let clamped = moved.clamp(0.0, scroll_max.max(0.0));
        *scroll = clamped;
        // `2^(-dt / half_life)`: the half-life is the time one halving takes.
        self.velocity *= (-dt / HALF_LIFE).exp2();

        let held = clamped != moved;
        let at_end = held
            && ((self.velocity > 0.0 && clamped >= scroll_max)
                || (self.velocity < 0.0 && clamped <= 0.0));
        if at_end || self.velocity.abs() < STOP_SPEED {
            self.gliding = false;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A track 0..100 tall with a 20-tall thumb and 400 pixels of content
    /// behind it: the numbers every test below reads. One pointer pixel moves
    /// the content five.
    const TRACK_TOP: f32 = 0.0;
    const TRACK_HEIGHT: f32 = 100.0;
    const THUMB: f32 = 20.0;
    const MAX: f32 = 400.0;

    #[test]
    fn the_thumb_tracks_the_pointer_without_jumping() {
        let mut drag = ScrollDrag::default();
        // Grabbed 4 pixels into a thumb whose top sits at 10: holding the
        // pointer still keeps the offset exactly where the thumb was — it
        // does not leap to put its top edge under the pointer.
        drag.begin(14.0, 10.0);
        let t = Instant::now();
        assert_eq!(
            drag.drag_to_at(t, 14.0, TRACK_TOP, TRACK_HEIGHT, THUMB, MAX),
            50.0
        );
        assert_eq!(
            drag.drag_to_at(
                t + Duration::from_millis(8),
                64.0,
                TRACK_TOP,
                TRACK_HEIGHT,
                THUMB,
                MAX
            ),
            300.0
        );
    }

    #[test]
    fn content_moves_proportionally_to_pointer_travel() {
        let mut drag = ScrollDrag::default();
        drag.begin(0.0, 0.0);
        let t = Instant::now();
        let a = drag.drag_to_at(t, 25.0, TRACK_TOP, TRACK_HEIGHT, THUMB, MAX);
        let b = drag.drag_to_at(
            t + Duration::from_millis(8),
            50.0,
            TRACK_TOP,
            TRACK_HEIGHT,
            THUMB,
            MAX,
        );
        // Equal pointer steps, equal content steps — the drag's "speed" is
        // the geometry's ratio, the same everywhere in the track.
        assert!((b - a) - a < 0.001, "{a} then {b} over the same distance");
    }

    #[test]
    fn a_drag_cannot_push_the_content_out_of_its_range() {
        let mut drag = ScrollDrag::default();
        drag.begin(0.0, 0.0);
        let t = Instant::now();
        assert_eq!(
            drag.drag_to_at(t, -500.0, TRACK_TOP, TRACK_HEIGHT, THUMB, MAX),
            0.0
        );
        assert_eq!(
            drag.drag_to_at(
                t + Duration::from_millis(8),
                500.0,
                TRACK_TOP,
                TRACK_HEIGHT,
                THUMB,
                MAX
            ),
            MAX
        );
    }

    #[test]
    fn a_flick_releases_into_a_glide() {
        let mut drag = ScrollDrag::default();
        drag.begin(0.0, 0.0);
        let mut t = Instant::now();
        // A quick upward sweep, eight milliseconds a move: the thumb going up
        // is the content going toward its end.
        for step in 0..4 {
            t += Duration::from_millis(8);
            let _ = drag.drag_to_at(
                t,
                40.0 - 10.0 * step as f32,
                TRACK_TOP,
                TRACK_HEIGHT,
                THUMB,
                MAX,
            );
        }
        // Released while the sweep is still fresh — 16 ms after its last move.
        assert!(
            drag.release_at(t + Duration::from_millis(16)),
            "a fast release glides"
        );
        assert!(!drag.is_dragging());
        assert!(drag.is_gliding());
        assert!(drag.velocity < 0.0, "the glide keeps the flick's direction");
    }

    #[test]
    fn a_pause_before_release_is_a_stop_not_a_flick() {
        let mut drag = ScrollDrag::default();
        drag.begin(0.0, 0.0);
        let mut t = Instant::now();
        for step in 0..4 {
            t += Duration::from_millis(8);
            let _ = drag.drag_to_at(
                t,
                40.0 - 10.0 * step as f32,
                TRACK_TOP,
                TRACK_HEIGHT,
                THUMB,
                MAX,
            );
        }
        // The hand rests half a second — no moves arrive while it does — and
        // then the button comes up. Nothing may move: the estimate would
        // otherwise still carry the sweep's speed into the release.
        assert!(!drag.release_at(t + Duration::from_millis(500)));
        assert!(!drag.is_gliding());
        assert_eq!(drag.velocity, 0.0);
    }

    #[test]
    fn a_move_after_a_long_pause_does_not_carry_the_old_speed() {
        let mut drag = ScrollDrag::default();
        drag.begin(0.0, 0.0);
        let mut t = Instant::now();
        for step in 0..4 {
            t += Duration::from_millis(8);
            let _ = drag.drag_to_at(
                t,
                40.0 - 10.0 * step as f32,
                TRACK_TOP,
                TRACK_HEIGHT,
                THUMB,
                MAX,
            );
        }
        assert!(drag.velocity < 0.0, "set-up: the sweep was fast");

        // Half a second of stillness, then a slow one-pixel creep — the hand
        // tremor case. Measured on its own it is nearly still; blended with
        // the pre-pause speed it would have re-armed the flick.
        t += Duration::from_millis(500);
        let _ = drag.drag_to_at(t, 9.0, TRACK_TOP, TRACK_HEIGHT, THUMB, MAX);
        assert!(
            drag.velocity.abs() < 100.0,
            "stale speed leaked through the pause: {}",
            drag.velocity
        );
        assert!(!drag.release_at(t + Duration::from_millis(8)));
    }

    #[test]
    fn letting_go_without_speed_does_not_glide() {
        let mut drag = ScrollDrag::default();
        drag.begin(10.0, 0.0);
        // One slow move and the button up: however far the thumb travelled,
        // there is no flick in that.
        let t = Instant::now();
        let _ = drag.drag_to_at(t, 30.0, TRACK_TOP, TRACK_HEIGHT, THUMB, MAX);
        assert!(!drag.release());
        assert!(!drag.is_gliding());
    }

    #[test]
    fn a_glide_decays_to_a_stop() {
        let mut drag = ScrollDrag::default();
        drag.gliding = true;
        drag.velocity = 300.0;
        drag.last_tick = Some(Instant::now());

        let mut scroll = 0.0;
        let mut beats = 0;
        let start = Instant::now();
        while drag.tick_at(
            start + Duration::from_millis(16 * (beats as u64 + 1)),
            &mut scroll,
            MAX,
        ) {
            beats += 1;
            assert!(beats < 200, "the glide never stopped");
        }
        // Half a second of glide at this speed, and well short of the end of
        // the range: it stopped by decaying, not by hitting the bound.
        assert!(scroll > 0.0, "the glide moved the content");
        assert!(scroll < MAX);
        assert!(!drag.is_gliding());
    }

    #[test]
    fn a_glide_stops_at_the_end_of_its_range() {
        let mut drag = ScrollDrag::default();
        drag.gliding = true;
        drag.velocity = 5000.0;
        drag.last_tick = Some(Instant::now());

        let mut scroll = MAX - 5.0;
        let at = Instant::now();
        assert!(
            !drag.tick_at(at + Duration::from_millis(16), &mut scroll, MAX),
            "hitting the end stops the glide immediately"
        );
        assert_eq!(scroll, MAX);
    }

    #[test]
    fn a_glide_tick_never_trusts_a_stalled_timer() {
        let mut drag = ScrollDrag::default();
        drag.gliding = true;
        drag.velocity = 1000.0;
        drag.last_tick = Some(Instant::now());

        let mut scroll = 0.0;
        // Five seconds between beats: the tick clamps its own step, so the
        // content moves at most a tenth of a second's worth.
        let at = Instant::now() + Duration::from_secs(5);
        let _ = drag.tick_at(at, &mut scroll, MAX);
        assert!(scroll <= 1000.0 * 0.1 + 0.001, "{scroll} in one tick");
    }

    #[test]
    fn stopping_clears_everything() {
        let mut drag = ScrollDrag::default();
        drag.gliding = true;
        drag.velocity = 500.0;
        drag.stop();
        assert!(!drag.is_dragging() && !drag.is_gliding());
        let mut scroll = 0.0;
        assert!(!drag.tick(&mut scroll, MAX));
        assert_eq!(scroll, 0.0, "a stopped glide does not move the content");
    }

    #[test]
    fn a_zero_range_absorbs_everything() {
        let mut drag = ScrollDrag::default();
        drag.begin(0.0, 0.0);
        let t = Instant::now();
        assert_eq!(
            drag.drag_to_at(t, 50.0, TRACK_TOP, TRACK_HEIGHT, THUMB, 0.0),
            0.0
        );

        drag.gliding = true;
        drag.velocity = 100.0;
        drag.last_tick = Some(Instant::now());
        let mut scroll = 0.0;
        assert!(!drag.tick_at(t + Duration::from_millis(16), &mut scroll, 0.0));
    }
}
