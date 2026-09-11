//! The shape and the blinking of the text caret.
//!
//! Everything in this file is pure: the element asks for a rectangle and an
//! appearance, then draws what it gets back. Mirrors VS Code's
//! `editor.cursorStyle` / `editor.cursorBlinking` / `editor.cursorWidth` and
//! Zed's `cursor_shape` / `vertical_scroll_margin` / `autoscroll_on_clicks`.

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
    /// Whether the caret has to be repainted every frame.
    ///
    /// [`Self::Blink`] is driven by the 500ms timer in `blink_cursor.rs`, and
    /// [`Self::Solid`] never changes -- neither needs an animation frame.
    pub fn needs_animation(self) -> bool {
        matches!(
            self,
            CursorBlinking::Smooth | CursorBlinking::Phase | CursorBlinking::Expand
        )
    }
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
pub fn appearance(blinking: CursorBlinking, phase: f32) -> (f32, f32) {
    let phase = phase.clamp(0., 1.);
    // 1 at both ends of the cycle, 0 in the middle.
    let wave = smoothstep((2. * phase - 1.).abs());

    match blinking {
        CursorBlinking::Blink => (if phase < 0.5 { 1. } else { 0. }, 1.),
        CursorBlinking::Smooth => (wave, 1.),
        // Never reaches 0: "phase" dims the caret, it does not hide it.
        CursorBlinking::Phase => (0.35 + 0.65 * wave, 1.),
        CursorBlinking::Expand => (1., 0.2 + 0.8 * wave),
        CursorBlinking::Solid => (1., 1.),
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

    #[test]
    fn blink_is_a_square_wave_and_solid_never_moves() {
        assert_eq!(appearance(CursorBlinking::Blink, 0.), (1., 1.));
        assert_eq!(appearance(CursorBlinking::Blink, 0.49), (1., 1.));
        assert_eq!(appearance(CursorBlinking::Blink, 0.5), (0., 1.));
        assert_eq!(appearance(CursorBlinking::Blink, 0.99), (0., 1.));

        for phase in [0., 0.25, 0.5, 0.75, 1.] {
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
    fn only_the_interpolating_styles_need_a_frame() {
        assert!(!CursorBlinking::Blink.needs_animation());
        assert!(!CursorBlinking::Solid.needs_animation());
        assert!(CursorBlinking::Smooth.needs_animation());
        assert!(CursorBlinking::Phase.needs_animation());
        assert!(CursorBlinking::Expand.needs_animation());
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
