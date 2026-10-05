//! The shape and the blinking of the text caret.
//!
//! Everything in this file is pure: the element asks for a rectangle and an
//! appearance, then draws what it gets back. Mirrors VS Code's
//! `editor.cursorStyle` / `editor.cursorBlinking` / `editor.cursorWidth` and
//! Zed's `cursor_shape` / `vertical_scroll_margin` / `autoscroll_on_clicks`.

use std::time::Duration;

use gpui::{point, px, size, Bounds, Pixels, Point};

/// The width a line caret gets when the width is left at `0` (auto).
///
/// This is the width the caret had before it was settable, so `0` keeps the
/// old look.
pub const DEFAULT_CURSOR_WIDTH: Pixels = px(1.5);

/// The width of a `line-thin` caret. Not affected by the width setting --
/// "thin" is the whole point of the shape.
pub const THIN_CURSOR_WIDTH: Pixels = px(1.);

/// How tall an `underline` / `underline-thin` caret is.
const UNDERLINE_HEIGHT: Pixels = px(2.);
const UNDERLINE_THIN_HEIGHT: Pixels = px(1.);

/// The narrowest a glyph-wide caret may get, so a caret at the end of a line
/// (where there is no glyph to measure) is still visible.
const MIN_GLYPH_WIDTH: Pixels = px(6.);

/// The shape of the caret.
///
/// Mirrors VS Code's `editor.cursorStyle`. Zed's `bar` is [`Self::Line`] and
/// its `hollow` is [`Self::BlockOutline`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CursorStyle {
    /// A vertical bar. The default, and the only shape before this existed.
    #[default]
    Line,
    /// A vertical bar always 1px wide.
    LineThin,
    /// A filled box over the glyph.
    Block,
    /// The outline of that box, leaving the glyph readable.
    BlockOutline,
    /// A bar under the glyph.
    Underline,
    /// The same bar, 1px tall.
    UnderlineThin,
}

impl CursorStyle {
    /// Whether this is a vertical bar, i.e. the shapes the width and the
    /// height settings apply to.
    pub fn is_line(self) -> bool {
        matches!(self, CursorStyle::Line | CursorStyle::LineThin)
    }

    /// Whether the caret is drawn as an outline instead of a filled quad.
    pub fn is_outline(self) -> bool {
        matches!(self, CursorStyle::BlockOutline)
    }

    /// Whether the caret is as wide as the glyph it sits on.
    pub fn covers_glyph(self) -> bool {
        !self.is_line()
    }
}

/// How the caret blinks.
///
/// Mirrors VS Code's `editor.cursorBlinking`. Zed's `cursor_blink` is
/// [`Self::Blink`] / [`Self::Solid`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CursorBlinking {
    /// On, off, on -- a square wave. The default.
    #[default]
    Blink,
    /// Fades all the way out and back.
    Smooth,
    /// Fades, but never all the way out.
    Phase,
    /// Stays solid; the height grows and shrinks instead.
    Expand,
    /// Never blinks.
    Solid,
}

impl CursorBlinking {
    /// Whether the caret is animated: its opacity or its height is somewhere
    /// else every time it is drawn, instead of being switched.
    ///
    /// [`Self::Blink`] is switched by the 500ms timer in `blink_cursor.rs`, and
    /// [`Self::Solid`] never changes. The others are drawn a step at a time,
    /// on a timer as well -- see [`next_fade_frame`]. None of them asks the
    /// display for frames.
    pub fn needs_animation(self) -> bool {
        matches!(
            self,
            CursorBlinking::Smooth | CursorBlinking::Phase | CursorBlinking::Expand
        )
    }
}

/// How long between two steps of an animated caret
/// ([`CursorBlinking::needs_animation`]).
///
/// A step draws the window again -- all of it, in an application that caches
/// none of its views -- so this is what the setting costs for as long as the
/// editor sits idle with the focus. 30 a second: the biggest step of a fade is
/// then a tenth of it.
///
/// The fades used to ask for a frame of the display on every frame they drew
/// instead, which is 60 to 144 windows a second (dopamine #902).
pub const FADE_TICK: Duration = Duration::from_nanos(1_000_000_000 / 30);

/// How long to wait before drawing an animated caret again, `elapsed` into
/// its cycle.
///
/// The steps are on a grid counted from the start of the cycle, not one
/// [`FADE_TICK`] after the frame that asks. A frame is only drawn on the
/// display's next refresh after its timer, so "a tick from now" lands just
/// past a refresh every time and waits for the one after: 20 frames a second
/// on a 60Hz display, not 30. On the grid, one frame being late does not push
/// the next one back.
///
/// `elapsed` is taken while the caret is painted, and that is where the wait
/// counts from: the frame it is painted in still has to be drawn and
/// presented before anything can wait, and what that takes comes off the
/// wait (`BlinkCursor::wait_for_step`).
pub fn next_fade_frame(elapsed: Duration) -> Duration {
    // Never wait for less than this. A frame drawn so late that the next
    // step is all but due -- or a timer that fired a hair early -- skips that
    // step instead of drawing it right behind.
    const MIN: Duration = Duration::from_millis(4);

    let tick = FADE_TICK.as_nanos();
    let mut wait = tick - elapsed.as_nanos() % tick;
    if wait < MIN.as_nanos() {
        wait += tick;
    }
    Duration::from_nanos(wait as u64)
}

/// How the caret moves between two positions.
///
/// Mirrors VS Code's `editor.cursorSmoothCaretAnimation`. This is the
/// **position**; [`CursorBlinking::Smooth`] is the **opacity**. They are
/// different things and can be on at the same time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum CaretAnimation {
    /// Jump, as the caret always did. The default.
    #[default]
    Off,
    /// Slide only when the caret was moved on purpose -- a key or a click.
    /// An edit that pushes the caret along does not animate.
    Explicit,
    /// Slide whenever the caret moves, typing included.
    On,
}

/// How long one slide takes.
///
/// Short on purpose: this is meant to be felt, not watched. VS Code is in the
/// same range.
pub const SLIDE_DURATION: Duration = Duration::from_millis(90);

/// How far the caret will slide instead of jumping, in rows.
///
/// A click across the file or a jump from a search hit should land, not draw
/// a line across the pane.
const MAX_SLIDE_ROWS: f32 = 3.;

/// Whether a move from `from` to `to` is close enough to animate.
///
/// Only the vertical distance is capped -- moving to the far end of the same
/// row (end of line, home) still reads well as a slide.
pub fn should_slide(from: Bounds<Pixels>, to: Bounds<Pixels>, line_height: Pixels) -> bool {
    if from.origin == to.origin {
        return false;
    }
    let dy = (to.origin.y - from.origin.y).abs();
    dy <= line_height * MAX_SLIDE_ROWS
}

/// The caret part way from `from` to `to`.
///
/// The size is interpolated too, so a block caret moving between a wide glyph
/// and a narrow one does not pop.
pub fn slide(from: Bounds<Pixels>, to: Bounds<Pixels>, t: f32) -> Bounds<Pixels> {
    let t = smoothstep(t);
    Bounds::new(
        point(
            lerp(from.origin.x, to.origin.x, t),
            lerp(from.origin.y, to.origin.y, t),
        ),
        size(
            lerp(from.size.width, to.size.width, t),
            lerp(from.size.height, to.size.height, t),
        ),
    )
}

fn lerp(a: Pixels, b: Pixels, t: f32) -> Pixels {
    a + (b - a) * t
}

/// When the rows kept above and below the caret are enforced.
///
/// Mirrors VS Code's `editor.cursorSurroundingLinesStyle`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SurroundingLinesStyle {
    /// Only while the caret is being moved with the keyboard. The default,
    /// and what the editor did before this was settable.
    #[default]
    OnMove,
    /// Every time the caret is scrolled into view, however it got there.
    Always,
}

/// How visible the caret is, and how tall, at a point in its blink cycle.
///
/// `phase` runs 0.0 -> 1.0 over one full cycle and **starts solid**, so the
/// caret is at its strongest right after a keystroke resets the cycle.
///
/// Returns `(opacity, height factor)`, both 0.0..=1.0.
///
/// **Only the interpolating styles are shaped here.** [`CursorBlinking::Blink`]
/// is switched on and off by the 500ms timer in `blink_cursor.rs`, which the
/// element consults through `InputState::show_cursor`; shaping it a second
/// time from the phase would multiply two square waves that drift apart --
/// anti-phase, and the caret never shows at all.
pub fn appearance(blinking: CursorBlinking, phase: f32) -> (f32, f32) {
    let phase = phase.clamp(0., 1.);
    // 1 at both ends of the cycle, 0 in the middle.
    let wave = smoothstep((2. * phase - 1.).abs());

    match blinking {
        // Drawn at all => drawn fully. The timer owns the on/off.
        CursorBlinking::Blink | CursorBlinking::Solid => (1., 1.),
        CursorBlinking::Smooth => (wave, 1.),
        // Never reaches 0: "phase" dims the caret, it does not hide it.
        CursorBlinking::Phase => (0.35 + 0.65 * wave, 1.),
        CursorBlinking::Expand => (1., 0.2 + 0.8 * wave),
    }
}

/// The classic 3t² - 2t³ ease, so the fades have no corners.
fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    t * t * (3. - 2. * t)
}

/// The rectangle to draw the caret in.
///
/// `origin` is the top-left of the line box at the caret's column, `advance`
/// the width of the glyph under it (used by the shapes that cover it), and
/// `width` / `height_pct` the settings -- both `None` / `0` meaning "auto",
/// which reproduces the caret as it was before the settings existed.
///
/// `auto_height` is the share of the line height an auto-height line caret
/// gets; the element takes it from the input's size.
pub fn cursor_bounds(
    style: CursorStyle,
    origin: Point<Pixels>,
    line_height: Pixels,
    advance: Pixels,
    width: Option<Pixels>,
    height_pct: u8,
    auto_height: f32,
) -> Bounds<Pixels> {
    match style {
        CursorStyle::Line | CursorStyle::LineThin => {
            let w = if style == CursorStyle::LineThin {
                THIN_CURSOR_WIDTH
            } else {
                width.unwrap_or(DEFAULT_CURSOR_WIDTH)
            };
            let factor = if height_pct == 0 {
                auto_height
            } else {
                f32::from(height_pct) / 100.
            };
            let h = line_height * factor;
            // Centred in the line box, like it always was.
            Bounds::new(point(origin.x, origin.y + (line_height - h) / 2.), size(w, h))
        }
        CursorStyle::Block | CursorStyle::BlockOutline => {
            Bounds::new(origin, size(advance.max(MIN_GLYPH_WIDTH), line_height))
        }
        CursorStyle::Underline | CursorStyle::UnderlineThin => {
            let h = if style == CursorStyle::UnderlineThin {
                UNDERLINE_THIN_HEIGHT
            } else {
                UNDERLINE_HEIGHT
            };
            Bounds::new(
                point(origin.x, origin.y + line_height - h),
                size(advance.max(MIN_GLYPH_WIDTH), h),
            )
        }
    }
}

/// Shrink `bounds` to `factor` of its height, keeping the bottom edge put.
///
/// [`CursorBlinking::Expand`] grows the caret upwards from the baseline, the
/// way VS Code's does.
pub fn scale_height(bounds: Bounds<Pixels>, factor: f32) -> Bounds<Pixels> {
    if factor >= 1. {
        return bounds;
    }
    let h = bounds.size.height * factor.max(0.);
    Bounds::new(
        point(bounds.origin.x, bounds.origin.y + bounds.size.height - h),
        size(bounds.size.width, h),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_caret_we_had() {
        assert_eq!(CursorStyle::default(), CursorStyle::Line);
        assert_eq!(CursorBlinking::default(), CursorBlinking::Blink);
        assert_eq!(SurroundingLinesStyle::default(), SurroundingLinesStyle::OnMove);

        // 1.5px wide, 85% of a 20px line, centred -- the old hard-coded quad.
        let b = cursor_bounds(
            CursorStyle::Line,
            point(px(10.), px(40.)),
            px(20.),
            px(8.),
            None,
            0,
            0.85,
        );
        assert_eq!(b.size.width, DEFAULT_CURSOR_WIDTH);
        assert_eq!(b.size.height, px(17.));
        assert_eq!(b.origin, point(px(10.), px(41.5)));
    }

    #[test]
    fn only_line_carets_take_the_width_and_height() {
        let wide = cursor_bounds(
            CursorStyle::Line,
            point(px(0.), px(0.)),
            px(20.),
            px(8.),
            Some(px(4.)),
            50,
            0.85,
        );
        assert_eq!(wide.size, size(px(4.), px(10.)));

        // `line-thin` ignores the width setting; that is what makes it thin.
        let thin = cursor_bounds(
            CursorStyle::LineThin,
            point(px(0.), px(0.)),
            px(20.),
            px(8.),
            Some(px(4.)),
            0,
            0.85,
        );
        assert_eq!(thin.size.width, THIN_CURSOR_WIDTH);

        // A block covers the glyph and the whole line, whatever the settings.
        let block = cursor_bounds(
            CursorStyle::Block,
            point(px(0.), px(0.)),
            px(20.),
            px(8.),
            Some(px(4.)),
            50,
            0.85,
        );
        assert_eq!(block.size, size(px(8.), px(20.)));
        assert_eq!(block.origin, point(px(0.), px(0.)));
    }

    #[test]
    fn a_caret_with_no_glyph_under_it_is_still_visible() {
        // End of line: `advance` comes back as zero.
        let b = cursor_bounds(
            CursorStyle::Block,
            point(px(0.), px(0.)),
            px(20.),
            px(0.),
            None,
            0,
            0.85,
        );
        assert_eq!(b.size.width, MIN_GLYPH_WIDTH);
    }

    #[test]
    fn underline_sits_on_the_bottom_edge() {
        let b = cursor_bounds(
            CursorStyle::Underline,
            point(px(0.), px(100.)),
            px(20.),
            px(8.),
            None,
            0,
            0.85,
        );
        assert_eq!(b.origin.y + b.size.height, px(120.));
        assert_eq!(b.size.height, UNDERLINE_HEIGHT);

        let thin = cursor_bounds(
            CursorStyle::UnderlineThin,
            point(px(0.), px(100.)),
            px(20.),
            px(8.),
            None,
            0,
            0.85,
        );
        assert_eq!(thin.size.height, UNDERLINE_THIN_HEIGHT);
    }

    /// The two styles the element must not shape.
    ///
    /// `Blink`'s on/off lives in the 500ms timer, which the element reads
    /// through `show_cursor`. If `appearance` also returned a square wave the
    /// two would drift into anti-phase and the caret would never be drawn --
    /// which is exactly what happened on the device before this was fixed.
    #[test]
    fn blink_and_solid_are_never_shaped_here() {
        for phase in [0., 0.25, 0.49, 0.5, 0.75, 0.99, 1.] {
            assert_eq!(appearance(CursorBlinking::Blink, phase), (1., 1.));
            assert_eq!(appearance(CursorBlinking::Solid, phase), (1., 1.));
        }
    }

    #[test]
    fn the_fades_start_solid_and_only_phase_stays_visible() {
        // Both start at full strength, so a keystroke leaves a solid caret.
        assert_eq!(appearance(CursorBlinking::Smooth, 0.).0, 1.);
        assert_eq!(appearance(CursorBlinking::Phase, 0.).0, 1.);

        // Half way through the cycle is the dim end.
        assert_eq!(appearance(CursorBlinking::Smooth, 0.5).0, 0.);
        assert_eq!(appearance(CursorBlinking::Phase, 0.5).0, 0.35);

        // Expand keeps the colour and moves the height instead.
        let (opacity, height) = appearance(CursorBlinking::Expand, 0.5);
        assert_eq!(opacity, 1.);
        assert_eq!(height, 0.2);
        assert_eq!(appearance(CursorBlinking::Expand, 0.).1, 1.);
    }

    #[test]
    fn only_the_interpolating_styles_are_animated() {
        assert!(!CursorBlinking::Blink.needs_animation());
        assert!(!CursorBlinking::Solid.needs_animation());
        assert!(CursorBlinking::Smooth.needs_animation());
        assert!(CursorBlinking::Phase.needs_animation());
        assert!(CursorBlinking::Expand.needs_animation());
    }

    /// The steps of a fade are a tick apart, counted from the start of its
    /// cycle -- wherever in between the caret happens to be painted.
    #[test]
    fn a_fade_is_stepped_on_a_grid() {
        let ms = Duration::from_millis;
        assert_eq!(next_fade_frame(Duration::ZERO), FADE_TICK);
        assert_eq!(next_fade_frame(ms(10)), FADE_TICK - ms(10));
        assert_eq!(next_fade_frame(FADE_TICK), FADE_TICK);
        assert_eq!(next_fade_frame(FADE_TICK * 7 + ms(5)), FADE_TICK - ms(5));
        // Hours in, the grid has not moved.
        assert_eq!(
            next_fade_frame(FADE_TICK * 1_000_000 + ms(20)),
            FADE_TICK - ms(20)
        );

        // Painted so late that the next step is all but due (or woken a hair
        // before its own): that one is skipped, not drawn right behind.
        assert_eq!(next_fade_frame(FADE_TICK - ms(1)), FADE_TICK + ms(1));
        assert_eq!(next_fade_frame(FADE_TICK * 3 - ms(3)), FADE_TICK + ms(3));

        for micros in (0..200_000).step_by(37) {
            let wait = next_fade_frame(Duration::from_micros(micros));
            assert!(wait >= ms(4), "{micros}us: {wait:?}");
            assert!(wait < FADE_TICK + ms(4), "{micros}us: {wait:?}");
        }
    }

    /// A fade as a display shows it, as the refreshes its frames are drawn on.
    ///
    /// The timer wakes it, the window is drawn on the next refresh of the
    /// display, and the caret is painted `into_frame` after that refresh
    /// began: that is where it asks for its next step. The frame is drawn and
    /// presented `rest(n)` after that, the nth of them, and only then does
    /// the wait start. `wait` is how long it is, given how far into its cycle
    /// the caret was painted and how long ago that is by then. The cycle
    /// began `before` the first refresh.
    fn refreshes_drawn(
        hz: u64,
        before: Duration,
        into_frame: Duration,
        rest: impl Fn(usize) -> Duration,
        wait: impl Fn(Duration, Duration) -> Duration,
    ) -> Vec<u64> {
        const SECONDS: u64 = 10;
        let refresh = |n: u64| Duration::from_nanos(n * 1_000_000_000 / hz);
        let mut drawn = vec![];
        let mut painted = into_frame;
        loop {
            let rest = rest(drawn.len());
            let wake = painted + rest + wait(before + painted, rest);
            let mut n = (wake.as_nanos() * u128::from(hz) / 1_000_000_000) as u64;
            while refresh(n) < wake {
                n += 1;
            }
            if n > SECONDS * hz {
                return drawn;
            }
            drawn.push(n);
            painted = refresh(n) + into_frame;
        }
    }

    /// What the cursor does: when the step is due is settled while the caret
    /// is painted, and the wait is for what is left of that.
    fn from_the_paint(painted: Duration, rest: Duration) -> Duration {
        next_fade_frame(painted).saturating_sub(rest)
    }

    /// A frame that is out of the way the moment its caret is painted.
    fn at_once(_: usize) -> Duration {
        Duration::ZERO
    }

    fn gaps(drawn: &[u64]) -> Vec<u64> {
        drawn.windows(2).map(|pair| pair[1] - pair[0]).collect()
    }

    /// 30 frames a second on the displays there are, and evenly: every other
    /// refresh at 60Hz, every fourth at 120Hz -- wherever between two
    /// refreshes the cycle began, however far into its frame the caret is
    /// painted, and however long the rest of that frame takes, the same every
    /// time or not.
    #[test]
    fn a_fade_is_thirty_frames_a_second_on_a_display() {
        let ms = Duration::from_millis;
        let befores = [ms(0), ms(3), ms(8), ms(15), ms(21), ms(30)];
        let rests: [(&str, fn(usize) -> Duration); 4] = [
            ("no time", at_once),
            ("3ms", |_| Duration::from_millis(3)),
            ("9ms", |_| Duration::from_millis(9)),
            ("1 to 8ms", |frame| {
                Duration::from_millis([2, 6, 1, 8, 4][frame % 5])
            }),
        ];
        for (before, (rest_is, rest)) in befores
            .into_iter()
            .flat_map(|before| rests.map(|rest| (before, rest)))
        {
            for into_frame in [ms(0), ms(1), ms(4), ms(9), ms(12)] {
                let drawn = refreshes_drawn(60, before, into_frame, rest, from_the_paint);
                let at = format!(
                    "60Hz, began {before:?} before, painted {into_frame:?} in, \
                     {rest_is} for the rest"
                );
                assert!((299..=300).contains(&drawn.len()), "{at}: {}", drawn.len());
                assert!(gaps(&drawn).iter().all(|gap| *gap == 2), "{at}");
            }
            for into_frame in [ms(0), ms(1), ms(4), ms(7)] {
                let drawn = refreshes_drawn(120, before, into_frame, rest, from_the_paint);
                let at = format!(
                    "120Hz, began {before:?} before, painted {into_frame:?} in, \
                     {rest_is} for the rest"
                );
                assert!((299..=300).contains(&drawn.len()), "{at}: {}", drawn.len());
                assert!(gaps(&drawn).iter().all(|gap| *gap == 4), "{at}");
            }
            // 144 is not a multiple of 30: four or five refreshes apart.
            for into_frame in [ms(0), ms(1), ms(4)] {
                let drawn = refreshes_drawn(144, before, into_frame, rest, from_the_paint);
                let at = format!(
                    "144Hz, began {before:?} before, painted {into_frame:?} in, \
                     {rest_is} for the rest"
                );
                assert!((299..=300).contains(&drawn.len()), "{at}: {}", drawn.len());
                assert!(gaps(&drawn).iter().all(|gap| (4..=5).contains(gap)), "{at}");
            }
        }
    }

    /// Why the steps are on a grid: a tick counted from the frame that asks
    /// comes due just after a refresh, and is drawn on the one after.
    #[test]
    fn a_tick_after_each_frame_would_be_twenty_a_second() {
        let ms = Duration::from_millis;
        for into_frame in [ms(1), ms(4), ms(9)] {
            let drawn = refreshes_drawn(60, ms(0), into_frame, at_once, |_, _| FADE_TICK);
            assert_eq!(drawn.len(), 200, "{into_frame:?} into the frame");
            assert!(gaps(&drawn).iter().all(|gap| *gap == 3), "{into_frame:?}");
        }
    }

    /// Why the wait counts from the paint, and not from when it starts: the
    /// rest of the frame comes in between, and does not take the same time
    /// twice. With the steps 3ms ahead of the display and a frame that needs
    /// 2ms one time and 6ms the next, a step that is that much late makes its
    /// refresh one time and misses it the next.
    #[test]
    fn a_wait_counted_from_the_end_of_the_frame_would_wobble() {
        let ms = Duration::from_millis;
        let rest = |frame: usize| if frame % 2 == 0 { ms(2) } else { ms(6) };

        let late = refreshes_drawn(60, ms(3), ms(1), rest, |painted, _| {
            next_fade_frame(painted)
        });
        // 30 a second still -- one refresh apart, then three.
        assert!((299..=300).contains(&late.len()), "{}", late.len());
        let late = gaps(&late);
        assert!(late.contains(&1) && late.contains(&3), "{late:?}");
        assert!(!late.contains(&2), "{late:?}");

        let drawn = refreshes_drawn(60, ms(3), ms(1), rest, from_the_paint);
        assert!((299..=300).contains(&drawn.len()), "{}", drawn.len());
        assert!(gaps(&drawn).iter().all(|gap| *gap == 2));
    }

    /// A window that takes longer to draw than a tick skips the steps it is
    /// too late for: it is not asked for two frames where it had the time for
    /// one. Whether the time goes before the caret is painted or after.
    #[test]
    fn a_slow_window_skips_steps() {
        let ms = Duration::from_millis;
        for (into_frame, rest) in [(ms(40), ms(0)), (ms(15), ms(25)), (ms(2), ms(38))] {
            let drawn = refreshes_drawn(
                60,
                Duration::ZERO,
                into_frame,
                move |_| rest,
                from_the_paint,
            );
            let at = format!("painted {into_frame:?} in, {rest:?} for the rest");
            assert!(drawn.len() <= 200, "{at}: {}", drawn.len());
            assert!(gaps(&drawn).iter().all(|gap| *gap >= 3), "{at}");
        }
    }

    #[test]
    fn a_slide_starts_at_from_and_lands_on_to() {
        let a = Bounds::new(point(px(0.), px(0.)), size(px(2.), px(20.)));
        let b = Bounds::new(point(px(100.), px(40.)), size(px(8.), px(20.)));

        assert_eq!(slide(a, b, 0.), a);
        assert_eq!(slide(a, b, 1.), b);
        // Out of range is clamped, not extrapolated.
        assert_eq!(slide(a, b, -1.), a);
        assert_eq!(slide(a, b, 2.), b);

        // Half way is half way, and the width comes along.
        let mid = slide(a, b, 0.5);
        assert_eq!(mid.origin, point(px(50.), px(20.)));
        assert_eq!(mid.size.width, px(5.));
    }

    #[test]
    fn only_nearby_moves_slide() {
        let line = px(20.);
        let at = |x: f32, y: f32| Bounds::new(point(px(x), px(y)), size(px(2.), px(17.)));

        // Same row, far across: still a slide.
        assert!(should_slide(at(0., 0.), at(800., 0.), line));
        // Three rows: the edge of the window.
        assert!(should_slide(at(0., 0.), at(0., 60.), line));
        assert!(should_slide(at(0., 60.), at(0., 0.), line));
        // Four rows: a jump.
        assert!(!should_slide(at(0., 0.), at(0., 80.), line));
        // Not moving is not a slide.
        assert!(!should_slide(at(10., 20.), at(10., 20.), line));
    }

    #[test]
    fn the_two_smooths_are_different_settings() {
        // `CaretAnimation` is the position, `CursorBlinking` the opacity.
        assert_eq!(CaretAnimation::default(), CaretAnimation::Off);
        assert_eq!(CursorBlinking::default(), CursorBlinking::Blink);
    }

    #[test]
    fn expanding_keeps_the_bottom_edge_put() {
        let b = Bounds::new(point(px(0.), px(10.)), size(px(2.), px(20.)));
        let half = scale_height(b, 0.5);
        assert_eq!(half.size.height, px(10.));
        assert_eq!(half.origin.y, px(20.));
        assert_eq!(half.origin.y + half.size.height, px(30.));

        // A factor of 1 leaves the rectangle alone.
        assert_eq!(scale_height(b, 1.), b);
    }
}
