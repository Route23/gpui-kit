use std::{
    cell::RefCell,
    ops::Range,
    rc::Rc,
    sync::{Arc, Mutex},
};

use gpui::{
    point, px, quad, size, App, AvailableSpace, BorderStyle, Bounds, CursorStyle, Edges, Element,
    ElementId, GlobalElementId, Half, HighlightStyle, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, StyledText,
    TextLayout, TextRun, Window, WrappedLine,
};

use crate::{global_state::GlobalState, input::Selection, text::node::LinkMark, ActiveTheme};

/// A inline element used to render a inline text and support selectable.
///
/// All text in TextView (including the CodeBlock) used this for text rendering.
pub(super) struct Inline {
    id: ElementId,
    text: SharedString,
    links: Rc<Vec<(Range<usize>, LinkMark)>>,
    highlights: Vec<(Range<usize>, HighlightStyle)>,
    styled_text: StyledText,

    state: Arc<Mutex<InlineState>>,
    /// See [`crate::text::TextViewStyle::fixed_layout`].
    fixed: bool,
    /// The layout at the theme's rem, while `fixed` and the rem around the view differs from it.
    scaled: Option<Rc<RefCell<Option<FixedLayout>>>>,
}

/// Text laid out at the theme's rem (dopamine #1186), painted scaled by `k`.
struct FixedLayout {
    lines: Vec<WrappedLine>,
    /// Byte offset of each line in the text (lines are split at `\n`).
    starts: Vec<usize>,
    runs: Vec<TextRun>,
    font_size: Pixels,
    line_height: Pixels,
    k: f32,
    wrap_width: Pixels,
}

/// The inline text state, used RefCell to keep the selection state.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct InlineState {
    hovered_index: Option<usize>,
    /// The text that actually rendering, matched with selection.
    pub(super) text: SharedString,
    pub(super) selection: Option<Selection>,
    /// The wrap width (at the theme's rem) this text was last laid out with in fixed layout
    /// (dopamine #1186). Kept while the width seen through the zoom stays within a pixel or so of
    /// it: layout widths are rounded to the pixel at every zoom, and the list markers next to the
    /// text are measured at the zoomed size, so `width / k` drifts by a fraction of a pixel —
    /// enough to re-wrap a line that only just fits.
    /// A few of them: taffy also measures with trial widths (a flex item against its whole row),
    /// and those must not push out the one the text is actually laid out at.
    fixed_wrap: Vec<Pixels>,
}

impl InlineState {
    /// Save actually rendered text for selected text to use.
    pub(crate) fn set_text(&mut self, text: SharedString) {
        self.text = text;
    }
}

impl Inline {
    pub(super) fn new(
        id: impl Into<ElementId>,
        state: Arc<Mutex<InlineState>>,
        links: Vec<(Range<usize>, LinkMark)>,
        highlights: Vec<(Range<usize>, HighlightStyle)>,
    ) -> Self {
        let text = state.lock().unwrap().text.clone();
        Self {
            id: id.into(),
            links: Rc::new(links),
            highlights,
            text: text.clone(),
            styled_text: StyledText::new(text),
            state,
            fixed: false,
            scaled: None,
        }
    }

    /// Lay out at the theme's rem and paint scaled (see [`crate::text::TextViewStyle::fixed_layout`]).
    pub(super) fn fixed_layout(mut self, fixed: bool) -> Self {
        self.fixed = fixed;
        self
    }

    /// Paint the fixed layout: every glyph and decoration of the base-size layout, scaled by `k`
    /// from `origin`.
    fn paint_fixed(layout: &FixedLayout, origin: Point<Pixels>, window: &mut Window, cx: &mut App) {
        let k = layout.k;
        let text_system = cx.text_system().clone();
        let mut row_top = px(0.);
        for (line, &start) in layout.lines.iter().zip(&layout.starts) {
            let unwrapped = &line.unwrapped_layout;
            let padding_top = (layout.line_height - unwrapped.ascent - unwrapped.descent) / 2.;
            let baseline = padding_top + unwrapped.ascent;
            let mut wraps = line.wrap_boundaries.iter().peekable();
            let mut row_x = px(0.);
            let mut row = 0usize;
            for (run_ix, run) in unwrapped.runs.iter().enumerate() {
                let max_w = text_system.bounding_box(run.font_id, layout.font_size).size.width;
                for (glyph_ix, glyph) in run.glyphs.iter().enumerate() {
                    if wraps.peek().is_some_and(|w| w.run_ix == run_ix && w.glyph_ix == glyph_ix) {
                        wraps.next();
                        row += 1;
                        row_x = glyph.position.x;
                    }
                    let x = glyph.position.x - row_x;
                    let y = row_top + layout.line_height * row as f32;
                    let byte = start + glyph.index;
                    let style = run_at(&layout.runs, byte);
                    let next_x = run
                        .glyphs
                        .get(glyph_ix + 1)
                        .map(|g| g.position.x - row_x)
                        .unwrap_or(x + max_w.min(layout.font_size));
                    if let Some(style) = style {
                        if let Some(bg) = style.background_color {
                            let b = Bounds::new(
                                point(origin.x + x * k, origin.y + y * k),
                                size((next_x - x) * k, layout.line_height * k),
                            );
                            window.paint_quad(gpui::fill(b, bg));
                        }
                    }
                    let at = point(origin.x + x * k, origin.y + (y + baseline) * k);
                    let font_size = layout.font_size * k;
                    if glyph.is_emoji {
                        let _ = window.paint_emoji(at, run.font_id, glyph.id, font_size);
                    } else if let Some(style) = style {
                        let _ = window.paint_glyph(at, run.font_id, glyph.id, font_size, style.color);
                    }
                    if let Some(style) = style {
                        if let Some(u) = style.underline {
                            let thick = u.thickness * k;
                            let b = Bounds::new(
                                point(origin.x + x * k, origin.y + (y + baseline) * k + thick),
                                size((next_x - x) * k, thick),
                            );
                            window.paint_quad(gpui::fill(b, u.color.unwrap_or(style.color)));
                        }
                        if let Some(st) = style.strikethrough {
                            let thick = st.thickness * k;
                            let b = Bounds::new(
                                point(origin.x + x * k, origin.y + (y + baseline - unwrapped.ascent / 3.) * k),
                                size((next_x - x) * k, thick),
                            );
                            window.paint_quad(gpui::fill(b, st.color.unwrap_or(style.color)));
                        }
                    }
                }
            }
            row_top += layout.line_height * (line.wrap_boundaries.len() + 1) as f32;
        }
    }

    /// Get link at given mouse position.
    fn link_for_position(
        layout: &TextLayout,
        links: &Vec<(Range<usize>, LinkMark)>,
        position: Point<Pixels>,
    ) -> Option<LinkMark> {
        let offset = layout.index_for_position(position).ok()?;
        for (range, link) in links.iter() {
            if range.contains(&offset) {
                return Some(link.clone());
            }
        }

        None
    }

    /// Paint selected bounds for debug.
    #[allow(unused)]
    fn paint_selected_bounds(&self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        window.paint_quad(gpui::PaintQuad {
            bounds,
            background: cx.theme().blue.alpha(0.01).into(),
            corner_radii: gpui::Corners::default(),
            border_color: gpui::transparent_black(),
            border_style: BorderStyle::default(),
            border_widths: gpui::Edges::all(px(0.)),
        });
    }

    fn layout_selections(
        &self,
        text_layout: &TextLayout,
        window: &mut Window,
        cx: &mut App,
    ) -> (bool, bool, Option<Selection>) {
        let Some(text_view_state) = GlobalState::global(cx).text_view_state() else {
            return (false, false, None);
        };

        let text_view_state = text_view_state.read(cx);
        let is_selectable = text_view_state.is_selectable();
        if !text_view_state.has_selection() {
            return (is_selectable, false, None);
        }

        let line_height = window.line_height();
        let selection_bounds = text_view_state.selection_bounds();

        // Use for debug selection bounds
        // self.paint_selected_bounds(selection_bounds, window, cx);

        let mut selection: Option<Selection> = None;
        let mut offset = 0;
        let mut chars = self.text.chars().peekable();
        while let Some(c) = chars.next() {
            let Some(pos) = text_layout.position_for_index(offset) else {
                offset += c.len_utf8();
                continue;
            };

            let mut char_width = line_height.half();
            if let Some(next_pos) = text_layout.position_for_index(offset + 1) {
                if next_pos.y == pos.y {
                    char_width = next_pos.x - pos.x;
                }
            }

            if point_in_text_selection(pos, char_width, &selection_bounds, line_height) {
                if selection.is_none() {
                    selection = Some((offset..offset).into());
                }

                let next_offset = offset + c.len_utf8();
                selection.as_mut().unwrap().end = next_offset;
            }

            offset += c.len_utf8();
        }

        (true, true, selection)
    }

    /// Paint the selection background.
    fn paint_selection(
        selection: &Selection,
        text_layout: &TextLayout,
        bounds: &Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut start = selection.start;
        let mut end = selection.end;
        if end < start {
            std::mem::swap(&mut start, &mut end);
        }
        let Some(start_position) = text_layout.position_for_index(start) else {
            return;
        };
        let Some(end_position) = text_layout.position_for_index(end) else {
            return;
        };

        let line_height = text_layout.line_height();
        if start_position.y == end_position.y {
            window.paint_quad(quad(
                Bounds::from_corners(
                    start_position,
                    point(end_position.x, end_position.y + line_height),
                ),
                px(0.),
                cx.theme().selection,
                Edges::default(),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));
        } else {
            window.paint_quad(quad(
                Bounds::from_corners(
                    start_position,
                    point(bounds.right(), start_position.y + line_height),
                ),
                px(0.),
                cx.theme().selection,
                Edges::default(),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));

            if end_position.y > start_position.y + line_height {
                window.paint_quad(quad(
                    Bounds::from_corners(
                        point(bounds.left(), start_position.y + line_height),
                        point(bounds.right(), end_position.y),
                    ),
                    px(0.),
                    cx.theme().selection,
                    Edges::default(),
                    gpui::transparent_black(),
                    BorderStyle::default(),
                ));
            }

            window.paint_quad(quad(
                Bounds::from_corners(
                    point(bounds.left(), end_position.y),
                    point(end_position.x, end_position.y + line_height),
                ),
                px(0.),
                cx.theme().selection,
                Edges::default(),
                gpui::transparent_black(),
                BorderStyle::default(),
            ));
        }
    }
}

impl IntoElement for Inline {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Inline {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        global_element_id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text_style = window.text_style();

        let mut runs = Vec::new();
        let mut ix = 0;
        for (range, highlight) in self.highlights.iter() {
            if ix < range.start {
                runs.push(text_style.clone().to_run(range.start - ix));
            }
            runs.push(text_style.clone().highlight(*highlight).to_run(range.len()));
            ix = range.end;
        }
        if ix < self.text.len() {
            runs.push(text_style.to_run(self.text.len() - ix));
        }

        // Fixed layout (dopamine #1186): lay out at the theme's rem, report the size scaled.
        let rem = window.rem_size();
        let base_rem = cx.theme().font_size;
        let k = if f32::from(base_rem) > 0.0 { f32::from(rem) / f32::from(base_rem) } else { 1.0 };
        if self.fixed {
            let font_size = text_style.font_size.to_pixels(rem) / k;
            let line_height = text_style.line_height.to_pixels(font_size.into(), base_rem).round();
            let text = self.text.clone();
            let inline_state = self.state.clone();
            let cell = Rc::new(RefCell::new(None::<FixedLayout>));
            self.scaled = Some(cell.clone());
            let layout_id = window.request_measured_layout(Default::default(), move |known, available, window, _cx| {
                let width = known.width.or(match available.width {
                    AvailableSpace::Definite(w) => Some(w),
                    _ => None,
                });
                let wrap_width = width.map(|w| {
                    let target = w / k;
                    let tolerance = px(1.5f32.max(1.0 / k));
                    let mut st = inline_state.lock().unwrap();
                    match st.fixed_wrap.iter().position(|prev| (target - *prev).abs() <= tolerance) {
                        Some(i) => {
                            // Most recently used first, so the least used is the one to go.
                            let prev = st.fixed_wrap.remove(i);
                            st.fixed_wrap.insert(0, prev);
                            prev
                        }
                        None => {
                            st.fixed_wrap.insert(0, target);
                            st.fixed_wrap.truncate(4);
                            target
                        }
                    }
                });
                let mut slot = cell.borrow_mut();
                let reuse = slot.as_ref().is_some_and(|l| Some(l.wrap_width) == wrap_width && (l.k - k).abs() < 1e-6);
                if !reuse {
                    let lines: Vec<WrappedLine> = window
                        .text_system()
                        .shape_text(text.clone(), font_size, &runs, wrap_width, None)
                        .map(|l| l.into_iter().collect())
                        .unwrap_or_default();
                    let mut starts = Vec::with_capacity(lines.len());
                    let mut at = 0;
                    for l in &lines {
                        starts.push(at);
                        at += l.len() + 1;
                    }
                    *slot = Some(FixedLayout {
                        lines,
                        starts,
                        runs: runs.clone(),
                        font_size,
                        line_height,
                        k,
                        wrap_width: wrap_width.unwrap_or_default(),
                    });
                }
                let l = slot.as_ref().unwrap();
                let rows: usize = l.lines.iter().map(|w| w.wrap_boundaries.len() + 1).sum();
                let widest = l.lines.iter().map(|w| w.unwrapped_layout.width).fold(px(0.), |a, b| a.max(b));
                size(width.unwrap_or(widest * k), l.line_height * rows.max(1) as f32 * k)
            });
            return (layout_id, ());
        }
        self.scaled = None;

        self.styled_text = StyledText::new(self.text.clone()).with_runs(runs);
        let (layout_id, _) =
            self.styled_text
                .request_layout(global_element_id, inspector_id, window, cx);

        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if self.scaled.is_none() {
            self.styled_text
                .prepaint(id, inspector_id, bounds, &mut (), window, cx);
        }

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        hitbox
    }

    fn paint(
        &mut self,
        global_id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(cell) = self.scaled.take() {
            if let Some(layout) = cell.borrow().as_ref() {
                Self::paint_fixed(layout, bounds.origin, window, cx);
            }
            return;
        }
        let current_view = window.current_view();
        let hitbox = prepaint;
        let mut state = self.state.lock().unwrap();

        let text_layout = self.styled_text.layout().clone();
        self.styled_text
            .paint(global_id, None, bounds, &mut (), &mut (), window, cx);

        // layout selections
        let (is_selectable, is_selection, selection) =
            self.layout_selections(&text_layout, window, cx);

        state.selection = selection;

        if is_selection || is_selectable {
            window.set_cursor_style(CursorStyle::IBeam, &hitbox);
        }

        // link cursor pointer
        let mouse_position = window.mouse_position();
        if let Some(_) = Self::link_for_position(&text_layout, &self.links, mouse_position) {
            window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
        }

        if let Some(selection) = &state.selection {
            Self::paint_selection(selection, &text_layout, &bounds, window, cx);
        }

        // mouse move, update hovered link
        window.on_mouse_event({
            let hitbox = hitbox.clone();
            let text_layout = text_layout.clone();
            let mut hovered_index = state.hovered_index;
            move |event: &MouseMoveEvent, phase, window, cx| {
                if !phase.bubble() || !hitbox.is_hovered(window) {
                    return;
                }

                let current = hovered_index;
                let updated = text_layout.index_for_position(event.position).ok();
                //  notify update when hovering over different links
                if current != updated {
                    hovered_index = updated;
                    cx.notify(current_view);
                }
            }
        });

        if !is_selection {
            // click to open link
            window.on_mouse_event({
                let links = self.links.clone();
                let text_layout = text_layout.clone();

                move |event: &MouseUpEvent, phase, _, cx| {
                    if !bounds.contains(&event.position) || !phase.bubble() {
                        return;
                    }

                    if let Some(link) =
                        Self::link_for_position(&text_layout, &links, event.position)
                    {
                        cx.stop_propagation();
                        cx.open_url(&link.url);
                    }
                }
            });
        }
    }
}

/// The run covering byte `byte` of the text.
fn run_at(runs: &[TextRun], byte: usize) -> Option<&TextRun> {
    let mut end = 0;
    for r in runs {
        end += r.len;
        if byte < end {
            return Some(r);
        }
    }
    runs.last()
}

/// Check if a `pos` is within a `bounds`, considering multi-line selections.
fn point_in_text_selection(
    pos: Point<Pixels>,
    char_width: Pixels,
    bounds: &Bounds<Pixels>,
    line_height: Pixels,
) -> bool {
    let top = bounds.top();
    let bottom = bounds.bottom();
    let left = bounds.left();
    let right = bounds.right();

    // Out of the vertical bounds
    if pos.y + line_height < top || pos.y >= bottom {
        return false;
    }

    let single_line = (bottom - top) <= line_height;
    if single_line {
        // If it's a single line selection, just check horizontal bounds
        return pos.x + char_width.half() >= left && pos.x + char_width.half() <= right;
    }

    let is_above = pos.y <= top;
    let is_below = pos.y + line_height >= bottom;

    if is_above {
        return pos.x + char_width.half() >= left;
    } else if is_below {
        return pos.x + char_width.half() <= right;
    } else {
        return true;
    }
}

#[cfg(test)]
mod tests {
    use super::point_in_text_selection;
    use gpui::{point, px, size, Bounds};

    #[test]
    fn test_point_in_text_selection() {
        let line_height = px(20.);
        let char_width = px(10.);
        let bounds = Bounds {
            origin: point(px(50.), px(50.)),
            size: size(px(100.), px(100.)),
        };

        // First line but haft line height, true
        // | p --------|
        // | selection |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(50.), px(40.)),
            char_width,
            &bounds,
            line_height
        ));

        // First line in selection, true
        // | p --------|
        // | selection |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(50.), px(50.)),
            char_width,
            &bounds,
            line_height
        ));
        // First line, but left out of selection, false
        // p |-----------|
        //   | selection |
        //   |-----------|
        assert!(!point_in_text_selection(
            point(px(40.), px(50.)),
            char_width,
            &bounds,
            line_height
        ));
        // First line but right out of selection, true
        // |-----------| p
        // | selection |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(160.), px(50.)),
            char_width,
            &bounds,
            line_height
        ));

        // Middle line in selection, true
        // |-----------|
        // |     p     |
        // |-----------|
        assert!(point_in_text_selection(
            point(px(100.), px(70.)),
            char_width,
            &bounds,
            line_height
        ));
        // Middle line, but left out of selection, true
        //   |-----------|
        // p | selection |
        //   |-----------|
        assert!(point_in_text_selection(
            point(px(40.), px(70.)),
            char_width,
            &bounds,
            line_height
        ));
        // Middle line, but right out of selection, true
        // |-----------|
        // | selection | p
        // |-----------|
        assert!(point_in_text_selection(
            point(px(160.), px(70.)),
            char_width,
            &bounds,
            line_height
        ));

        // Last line in selection, true
        // |-----------|
        // | selection |
        // |------- p -|
        assert!(point_in_text_selection(
            point(px(100.), px(140.)),
            char_width,
            &bounds,
            line_height
        ));
        // Last line, but left out of selection, true
        //
        //   |-----------|
        //   | selection |
        // p |-----------|
        assert!(point_in_text_selection(
            point(px(40.), px(140.)),
            char_width,
            &bounds,
            line_height
        ));
        // Last line, but right out of selection, false
        // |-----------|
        // | selection |
        // |-----------| p
        assert!(!point_in_text_selection(
            point(px(160.), px(140.)),
            char_width,
            &bounds,
            line_height
        ));

        // Out of vertical bounds (top), false
        //       p
        // |-----------|
        // | selection |
        // |-----------|
        assert!(!point_in_text_selection(
            point(px(100.), px(20.)),
            char_width,
            &bounds,
            line_height
        ));
        // Out of vertical bounds (bottom), false
        // |-----------|
        // | selection |
        // |-----------|
        //       p
        assert!(!point_in_text_selection(
            point(px(100.), px(160.)),
            char_width,
            &bounds,
            line_height
        ));
    }
}
