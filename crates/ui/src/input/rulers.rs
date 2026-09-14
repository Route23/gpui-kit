use gpui::{Bounds, Path, PathBuilder, Pixels, TextStyle, Window, point, px};

use crate::input::{InputState, LastLayout, element::TextElement};

/// The width of one column, in pixels.
///
/// This is the advance of `'0'`, the same measure VS Code calls the "typical
/// halfwidth character width". **Do not shape a run of spaces and divide**: on
/// a font with ligatures or any proportional metrics that drifts away from
/// where the glyphs actually land (dopamine #220), and the wrap column and the
/// rulers would then disagree with each other.
pub(super) fn column_advance(style: &TextStyle, font_size: Pixels, window: &Window) -> Pixels {
    let font_id = window.text_system().resolve_font(&style.font());
    window
        .text_system()
        .ch_advance(font_id, font_size)
        .unwrap_or(font_size * 0.5)
}

impl TextElement {
    /// A vertical line through the whole editor at each ruler column.
    ///
    /// The line spans the viewport, not the document: it does not move when
    /// the text scrolls up, but it does follow the text sideways, because the
    /// column it marks is a column of the text.
    ///
    /// `bounds` is the scrolled origin (what the text is drawn against);
    /// `input_bounds` is the element itself.
    pub(super) fn layout_rulers(
        state: &InputState,
        input_bounds: &Bounds<Pixels>,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        text_style: &TextStyle,
        window: &Window,
    ) -> Option<Path<Pixels>> {
        let columns = state.mode.rulers();
        if columns.is_empty() {
            return None;
        }

        let font_size = text_style.font_size.to_pixels(window.rem_size());
        let advance = column_advance(text_style, font_size, window);
        let text_left = last_layout.line_number_width;
        // Nothing clips the input's painting, so a ruler has to stay inside
        // the element by itself: the gutter's own quad covers the left, but
        // the right edge would otherwise bleed into the next pane.
        let left = input_bounds.origin.x + text_left;
        let right = input_bounds.origin.x + input_bounds.size.width;
        let top = input_bounds.origin.y;
        let bottom = top + input_bounds.size.height;

        let mut builder = PathBuilder::stroke(px(1.));
        let mut any = false;
        for column in columns {
            let x = bounds.origin.x + text_left + advance * *column as f32;
            if x < left || x > right {
                continue;
            }
            builder.move_to(point(x, top));
            builder.line_to(point(x, bottom));
            any = true;
        }

        if !any {
            return None;
        }
        builder.build().ok()
    }
}
