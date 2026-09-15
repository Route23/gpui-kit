use std::{ops::Range, rc::Rc};

use gpui::{
    App, Bounds, Corners, Element, ElementId, ElementInputHandler, Entity, GlobalElementId, Half,
    HighlightStyle, Hitbox, Hsla, IntoElement, LayoutId, MouseButton, MouseMoveEvent, Path, Pixels,
    Point, ShapedLine, SharedString, Size, Style, TextRun, TextStyle, UnderlineStyle, Window, fill,
    point, px, relative, size,
};
use ropey::Rope;
use smallvec::SmallVec;

use crate::{
    ActiveTheme as _, Colorize, PixelsExt, Root,
    input::{RopeExt as _, brackets, caret, text_wrapper::LineLayout},
};

use super::{
    InputState, LastLayout,
    inlay::{INLAY_CHIP_PAD, INLAY_GAP},
    mode::{InputMode, WrapAt},
};

/// Rows kept below the caret in an input that is not a code editor.
///
/// A code editor takes the number from `editor.cursorSurroundingLines`
/// instead (`InputMode::cursor_surrounding_lines`).
const BOTTOM_MARGIN_ROWS: usize = 3;
/// How much room to keep to the right of the caret when scrolling it into view.
///
/// **This has to clear the vertical scrollbar.** The scrollbar is drawn over
/// the text at the right edge of the input, and it is drawn *after* the caret,
/// so a caret parked inside that band is simply covered -- which is what
/// happened at the end of a horizontally scrolled line (dopamine #324): the
/// margin was 10px, the scrollbar is 16px, and the caret sat under it.
///
/// The extra px beyond the scrollbar's width is room for the caret itself: the
/// margin positions the caret's *column*, and block and underline carets
/// extend a glyph's width to the right of it.
pub(super) const RIGHT_MARGIN: Pixels = px(WIDTH_OF_SCROLLBAR + 8.);

/// `crate::scroll::Scrollbar::width()`, which is not reachable from a `const`
/// initialiser. Kept next to the margin so the two cannot drift apart.
const WIDTH_OF_SCROLLBAR: f32 = 4. * 2. + 8.;
/// Trace `points` as a closed polygon with its corners rounded by `radius`.
///
/// **The polygon is not convex.** A selection whose rows differ in width has
/// step-out corners on the left (see the `points` walk above), and rounding
/// those the same way is what makes the shape read as one block rather than
/// a stack of boxes. The radius is cut down to half of the shorter of the two
/// sides at every corner, so a one-character selection -- or the 6px stub an
/// empty row gets -- cannot fold through itself.
fn round_corners(builder: &mut gpui::PathBuilder, points: &[Point<Pixels>], radius: Pixels) {
    let n = points.len();
    if n < 3 {
        let mut iter = points.iter();
        if let Some(first) = iter.next() {
            builder.move_to(*first);
            for p in iter {
                builder.line_to(*p);
            }
        }
        return;
    }

    /// How far along `from` -> `to` the corner is cut, at most `radius`.
    fn cut(from: Point<Pixels>, to: Point<Pixels>, radius: Pixels) -> Point<Pixels> {
        let dx = f32::from(to.x - from.x);
        let dy = f32::from(to.y - from.y);
        let len = dx.hypot(dy);
        if len <= 0. {
            return to;
        }
        let r = f32::from(radius).min(len / 2.);
        let t = r / len;
        point(from.x + px(dx * t), from.y + px(dy * t))
    }

    let start = cut(points[0], points[1], radius);
    builder.move_to(start);
    for i in 1..=n {
        let corner = points[i % n];
        let next = points[(i + 1) % n];
        // Stop short of the corner, curve through it, carry on.
        builder.line_to(cut(corner, points[i - 1], radius));
        builder.curve_to(cut(corner, next, radius), corner);
    }
    builder.close();
}

/// The corner radius the selection and its twins are drawn with, if any.
fn selection_radius(state: &InputState) -> Option<Pixels> {
    state
        .mode
        .rounded_selection()
        .then_some(crate::input::mode::SELECTION_CORNER_RADIUS)
}

/// The strip between the line numbers and the text that holds fold chevrons.
pub(super) const FOLD_CHEVRON_WIDTH: Pixels = px(14.);

/// The left edge of the fold chevron strip.
///
/// **Paint and hit-testing must agree**: the chevron sits inside
/// `LINE_NUMBER_RIGHT_MARGIN`, not flush against the text.
pub(super) fn fold_chevron_x(gutter_origin_x: Pixels, line_number_width: Pixels) -> Pixels {
    gutter_origin_x + line_number_width - LINE_NUMBER_RIGHT_MARGIN - FOLD_CHEVRON_WIDTH
}

/// The width the fold chevrons take out of the gutter, 0 when folding is off.
pub(super) fn fold_chevron_width(state: &InputState) -> Pixels {
    if state.mode.has_folding() {
        FOLD_CHEVRON_WIDTH
    } else {
        px(0.)
    }
}

pub(super) const LINE_NUMBER_RIGHT_MARGIN: Pixels = px(10.);

pub(super) struct TextElement {
    pub(crate) state: Entity<InputState>,
    placeholder: SharedString,
}

impl TextElement {
    pub(super) fn new(state: Entity<InputState>) -> Self {
        Self {
            state,
            placeholder: SharedString::default(),
        }
    }

    /// Set the placeholder text of the input field.
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    fn paint_mouse_listeners(&mut self, window: &mut Window, _: &mut App) {
        window.on_mouse_event({
            let state = self.state.clone();

            move |event: &MouseMoveEvent, _, window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    state.update(cx, |state, cx| {
                        state.on_drag_move(event, window, cx);
                    });
                }
            }
        });
    }

    /// Returns the:
    ///
    /// - cursor bounds
    /// - scroll offset
    /// - current row index (No only the visible lines, but all lines)
    ///
    /// This method also will update for track scroll to cursor.
    fn layout_cursor(
        &self,
        last_layout: &LastLayout,
        bounds: &mut Bounds<Pixels>,
        _: &mut Window,
        cx: &mut App,
    ) -> (Option<Bounds<Pixels>>, Point<Pixels>, Option<usize>) {
        let state = self.state.read(cx);

        let line_height = last_layout.line_height;
        let visible_range = &last_layout.visible_range;
        let lines = &last_layout.lines;
        let text_wrapper = &state.text_wrapper;
        let line_number_width = last_layout.line_number_width;

        let mut selected_range = state.selected_range;

        if let Some(ime_marked_range) = &state.ime_marked_range {
            selected_range = (ime_marked_range.end..ime_marked_range.end).into();
        }
        let is_selected_all = selected_range.len() == state.text.len();

        let mut cursor = state.cursor();
        if state.masked {
            // Because masked use `*`, 1 char with 1 byte.
            selected_range.start = state.text.offset_to_char_index(selected_range.start);
            selected_range.end = state.text.offset_to_char_index(selected_range.end);
            cursor = state.text.offset_to_char_index(cursor);
        }

        let mut current_row = None;
        let mut scroll_offset = state.scroll_handle.offset();
        let mut cursor_bounds = None;

        // If the input has a fixed height (Otherwise is auto-grow), we need to add a bottom margin to the input.
        let margin_rows = if state.mode.is_code_editor() {
            usize::from(state.mode.cursor_surrounding_lines())
        } else {
            BOTTOM_MARGIN_ROWS
        };
        let top_bottom_margin = if state.mode.is_auto_grow() {
            line_height
        } else if margin_rows == 0 || visible_range.len() < BOTTOM_MARGIN_ROWS * 8 {
            line_height
        } else {
            margin_rows * line_height
        };

        // The cursor corresponds to the current cursor position in the text no only the line.
        let mut cursor_pos = None;
        let mut cursor_advance = None;
        let mut cursor_start = None;
        let mut cursor_end = None;

        let mut prev_lines_offset = 0;
        // **The content starts below the pad** (dopamine #255 / ADR-0090).
        // The caret's y and the scroll-into-view both come out of this walk,
        // so seeding it here keeps them with the text instead of a pad above it.
        let mut offset_y = state.mode.padding_top();
        for (ix, wrap_line) in text_wrapper.lines.iter().enumerate() {
            let row = ix;
            let line_origin = point(px(0.), offset_y);

            // break loop if all cursor positions are found
            if cursor_pos.is_some() && cursor_start.is_some() && cursor_end.is_some() {
                break;
            }

            let in_visible_range = ix >= visible_range.start;
            if let Some(line) = in_visible_range
                .then(|| lines.get(ix.saturating_sub(visible_range.start)))
                .flatten()
            {
                // If in visible range lines
                if cursor_pos.is_none() {
                    let offset = cursor.saturating_sub(prev_lines_offset);
                    if let Some(pos) = line.position_for_index(offset, line_height) {
                        current_row = Some(row);
                        cursor_pos = Some(line_origin + pos);
                        // How wide the glyph under the caret is, for the block
                        // and underline shapes. `x_for_index` snaps to the next
                        // glyph, so probing one byte ahead steps over a
                        // multi-byte character without decoding it here. At the
                        // end of a line there is nothing ahead and the caret
                        // falls back to a minimum width.
                        cursor_advance = line
                            .position_for_index(offset + 1, line_height)
                            .filter(|next| next.y == pos.y)
                            .map(|next| next.x - pos.x);
                    }
                }
                if cursor_start.is_none() {
                    let offset = selected_range.start.saturating_sub(prev_lines_offset);
                    if let Some(pos) = line.position_for_index(offset, line_height) {
                        cursor_start = Some(line_origin + pos);
                    }
                }
                if cursor_end.is_none() {
                    let offset = selected_range.end.saturating_sub(prev_lines_offset);
                    if let Some(pos) = line.position_for_index(offset, line_height) {
                        cursor_end = Some(line_origin + pos);
                    }
                }

                offset_y += line.size(line_height).height;
                // +1 for the last `\n`
                prev_lines_offset += line.len() + 1;
            } else {
                // If not in the visible range.

                // Just increase the offset_y and prev_lines_offset.
                // This will let the scroll_offset to track the cursor position correctly.
                if prev_lines_offset >= cursor && cursor_pos.is_none() {
                    current_row = Some(row);
                    cursor_pos = Some(line_origin);
                }
                if prev_lines_offset >= selected_range.start && cursor_start.is_none() {
                    cursor_start = Some(line_origin);
                }
                if prev_lines_offset >= selected_range.end && cursor_end.is_none() {
                    cursor_end = Some(line_origin);
                }

                offset_y += wrap_line.height(line_height);
                // +1 for the last `\n`
                prev_lines_offset += wrap_line.len() + 1;
            }
        }

        if let (Some(cursor_pos), Some(cursor_start), Some(cursor_end)) =
            (cursor_pos, cursor_start, cursor_end)
        {
            let selection_changed = state.last_selected_range != Some(selected_range);
            if selection_changed && !is_selected_all {
                scroll_offset.x = if scroll_offset.x + cursor_pos.x
                    > (bounds.size.width - line_number_width - RIGHT_MARGIN)
                {
                    // cursor is out of right
                    bounds.size.width - line_number_width - RIGHT_MARGIN - cursor_pos.x
                } else if scroll_offset.x + cursor_pos.x < px(0.) {
                    // cursor is out of left
                    scroll_offset.x - cursor_pos.x
                } else {
                    scroll_offset.x
                };

                // If we change the scroll_offset.y, GPUI will render and trigger the next run loop.
                // So, here we just adjust offset by `line_height` for move smooth.
                scroll_offset.y =
                    if scroll_offset.y + cursor_pos.y > bounds.size.height - top_bottom_margin {
                        // cursor is out of bottom
                        scroll_offset.y - line_height
                    } else if scroll_offset.y + cursor_pos.y < top_bottom_margin {
                        // cursor is out of top
                        (scroll_offset.y + line_height).min(px(0.))
                    } else {
                        scroll_offset.y
                    };

                if state.selection_reversed {
                    if scroll_offset.x + cursor_start.x < px(0.) {
                        // selection start is out of left
                        scroll_offset.x = -cursor_start.x;
                    }
                    if scroll_offset.y + cursor_start.y < px(0.) {
                        // selection start is out of top
                        scroll_offset.y = -cursor_start.y;
                    }
                } else {
                    if scroll_offset.x + cursor_end.x <= px(0.) {
                        // selection end is out of left
                        scroll_offset.x = -cursor_end.x;
                    }
                    if scroll_offset.y + cursor_end.y <= px(0.) {
                        // selection end is out of top
                        scroll_offset.y = -cursor_end.y;
                    }
                }
            }

            // cursor bounds
            //
            // The shape, the width and the height all come from the mode; a
            // plain input asks for nothing and gets the bar it always had,
            // sized from `state.size`.
            let auto_height = match state.size {
                crate::Size::Large => 1.,
                crate::Size::Small => 0.75,
                _ => 0.85,
            };

            // **No scroll offset on either axis here.** `cursor_bounds_with_scroll`
            // adds it at paint time, so the rectangle stored in `LastLayout` is
            // in text coordinates -- which is what the caret slide interpolates
            // in. Mixing the two would make a plain sideways scroll look like
            // the caret slid across the pane.
            cursor_bounds = Some(caret::cursor_bounds(
                state.mode.cursor_style(),
                point(
                    bounds.left() + cursor_pos.x + line_number_width,
                    bounds.top() + cursor_pos.y,
                ),
                line_height,
                cursor_advance.unwrap_or(px(0.)),
                state.mode.cursor_width(),
                state.mode.cursor_height(),
                auto_height,
            ));
        }

        if let Some(deferred_scroll_offset) = state.deferred_scroll_offset {
            scroll_offset = deferred_scroll_offset;
        }

        bounds.origin = bounds.origin + scroll_offset;

        (cursor_bounds, scroll_offset, current_row)
    }

    /// Layout the match range to a Path.
    pub(crate) fn layout_match_range(
        range: Range<usize>,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
    ) -> Option<Path<Pixels>> {
        Self::layout_match_range_with(range, last_layout, bounds, None, None)
    }

    /// The same box with its corners rounded by `radius`.
    ///
    /// Only the filled boxes take a radius: the bracket outline is a stroke
    /// that has to sit exactly on the glyph's box, and the document colour
    /// swatches are tiny.
    pub(crate) fn layout_match_range_rounded(
        range: Range<usize>,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        radius: Option<Pixels>,
    ) -> Option<Path<Pixels>> {
        Self::layout_match_range_with(range, last_layout, bounds, None, radius)
    }

    /// The same box as [`Self::layout_match_range`], stroked instead of filled
    /// when `stroke` is given. Used to outline the matching bracket without
    /// touching the glyph underneath.
    pub(crate) fn layout_match_range_with(
        range: Range<usize>,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        stroke: Option<Pixels>,
        radius: Option<Pixels>,
    ) -> Option<Path<Pixels>> {
        if range.is_empty() {
            return None;
        }

        if range.start < last_layout.visible_range_offset.start
            || range.end > last_layout.visible_range_offset.end
        {
            return None;
        }

        let line_height = last_layout.line_height;
        let visible_top = last_layout.visible_top;
        let visible_start_offset = last_layout.visible_range_offset.start;
        let lines = &last_layout.lines;
        let line_number_width = last_layout.line_number_width;

        let start_ix = range.start;
        let end_ix = range.end;

        let mut prev_lines_offset = visible_start_offset;
        let mut offset_y = visible_top;
        let mut line_corners = vec![];

        for line in lines.iter() {
            let line_size = line.size(line_height);
            let line_wrap_width = line_size.width;

            let line_origin = point(px(0.), offset_y);

            let line_cursor_start =
                line.position_for_index(start_ix.saturating_sub(prev_lines_offset), line_height);
            let line_cursor_end =
                line.position_for_index(end_ix.saturating_sub(prev_lines_offset), line_height);

            if line_cursor_start.is_some() || line_cursor_end.is_some() {
                let start = line_cursor_start
                    .unwrap_or_else(|| line.position_for_index(0, line_height).unwrap());

                let end = line_cursor_end
                    .unwrap_or_else(|| line.position_for_index(line.len(), line_height).unwrap());

                // Split the selection into multiple items
                let wrapped_lines =
                    (end.y / line_height).ceil() as usize - (start.y / line_height).ceil() as usize;

                let mut end_x = end.x;
                if wrapped_lines > 0 {
                    end_x = line_wrap_width;
                }

                // Ensure at least 6px width for the selection for empty lines.
                end_x = end_x.max(start.x + px(6.));

                line_corners.push(Corners {
                    top_left: line_origin + point(start.x, start.y),
                    top_right: line_origin + point(end_x, start.y),
                    bottom_left: line_origin + point(start.x, start.y + line_height),
                    bottom_right: line_origin + point(end_x, start.y + line_height),
                });

                // wrapped lines
                for i in 1..=wrapped_lines {
                    let start = point(px(0.), start.y + i as f32 * line_height);
                    let mut end = point(end.x, end.y + i as f32 * line_height);
                    if i < wrapped_lines {
                        end.x = line_size.width;
                    }

                    line_corners.push(Corners {
                        top_left: line_origin + point(start.x, start.y),
                        top_right: line_origin + point(end.x, start.y),
                        bottom_left: line_origin + point(start.x, start.y + line_height),
                        bottom_right: line_origin + point(end.x, start.y + line_height),
                    });
                }
            }

            if line_cursor_start.is_some() && line_cursor_end.is_some() {
                break;
            }

            offset_y += line_size.height;
            // +1 for skip the last `\n`
            prev_lines_offset += line.len() + 1;
        }

        let mut points = vec![];
        if line_corners.is_empty() {
            return None;
        }

        // Fix corners to make sure the left to right direction
        for corners in &mut line_corners {
            if corners.top_left.x > corners.top_right.x {
                std::mem::swap(&mut corners.top_left, &mut corners.top_right);
                std::mem::swap(&mut corners.bottom_left, &mut corners.bottom_right);
            }
        }

        for corners in &line_corners {
            points.push(corners.top_right);
            points.push(corners.bottom_right);
            points.push(corners.bottom_left);
        }

        let mut rev_line_corners = line_corners.iter().rev().peekable();
        while let Some(corners) = rev_line_corners.next() {
            points.push(corners.top_left);
            if let Some(next) = rev_line_corners.peek() {
                if next.top_left.x > corners.top_left.x {
                    points.push(point(next.top_left.x, corners.top_left.y));
                }
            }
        }

        // print_points_as_svg_path(&line_corners, &points);

        let path_origin = bounds.origin + point(line_number_width, px(0.));
        let points: Vec<Point<Pixels>> = points.iter().map(|p| path_origin + *p).collect();
        let mut builder = match stroke {
            Some(width) => gpui::PathBuilder::stroke(width),
            None => gpui::PathBuilder::fill(),
        };
        match radius {
            Some(radius) if radius > px(0.) => round_corners(&mut builder, &points, radius),
            _ => {
                let first_p = points[0];
                builder.move_to(first_p);
                for p in points.iter().skip(1) {
                    builder.line_to(*p);
                }
                // A stroked outline has to come back to where it started, or
                // the box is missing one side.
                if stroke.is_some() {
                    builder.line_to(first_p);
                }
            }
        }

        builder.build().ok()
    }

    fn layout_search_matches(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &mut App,
    ) -> Vec<(Path<Pixels>, bool)> {
        let search_panel = self.state.read(cx).search_panel.clone();
        let Some((ranges, current_match_ix)) = search_panel.and_then(|panel| {
            if let Some(matcher) = panel.read(cx).matcher() {
                Some((matcher.matched_ranges.clone(), matcher.current_match_ix))
            } else {
                None
            }
        }) else {
            return vec![];
        };

        let mut paths = Vec::new();
        for (index, range) in ranges.as_ref().iter().enumerate() {
            if let Some(path) = Self::layout_match_range(range.clone(), last_layout, bounds) {
                paths.push((path, current_match_ix == index));
            }
        }

        paths
    }

    /// Every other run of the selected text that is on screen.
    ///
    /// The selection itself is left out -- it is already drawn, in a stronger
    /// colour. Only what is visible is looked at: [`Self::layout_match_range`]
    /// throws away anything outside the viewport anyway, and the scan is over
    /// the visible slice rather than the document (see
    /// [`super::selection::occurrences`] for why that matters during a drag).
    fn layout_selection_highlights(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &App,
    ) -> Vec<Path<Pixels>> {
        // **The server's answer wins.** Showing both means showing two
        // colours for the same word, one of them a guess.
        if self.state.read(cx).lsp.has_document_highlights() {
            return vec![];
        }
        let state = self.state.read(cx);
        if !state.mode.selection_highlight() || state.masked {
            return vec![];
        }
        let selected: Range<usize> = state.selected_range.into();
        let (start, end) = (selected.start.min(selected.end), selected.start.max(selected.end));
        if start == end || end > state.text.len() {
            return vec![];
        }
        let needle = state.text.slice(start..end).to_string();
        if needle.chars().count() > state.mode.selection_highlight_max_len() {
            return vec![];
        }
        if needle.contains('\n') && !state.mode.selection_highlight_multiline() {
            return vec![];
        }

        let visible = last_layout.visible_range_offset.clone();
        if visible.start >= visible.end || visible.end > state.text.len() {
            return vec![];
        }
        let haystack = state.text.slice(visible.clone()).to_string();
        let radius = selection_radius(state);
        super::selection::occurrences(&needle, &haystack, visible.start, &(start..end))
            .into_iter()
            .filter_map(|range| {
                Self::layout_match_range_rounded(range, last_layout, bounds, radius)
            })
            .collect()
    }

    /// The string and comment spans inside `range`, or nothing when the
    /// highlighter has not parsed yet (the first frame after opening a file).
    fn skipped_ranges(state: &InputState, range: &Range<usize>) -> Vec<Range<usize>> {
        let InputMode::CodeEditor { highlighter, .. } = &state.mode else {
            return vec![];
        };
        let highlighter = highlighter.borrow();
        let Some(highlighter) = highlighter.as_ref() else {
            return vec![];
        };
        highlighter.skipped_ranges(range)
    }

    /// A vertical guide from each bracket pair's opener down to its closer.
    ///
    /// Same mechanics as the indent guides (`indent.rs`): walk the visible rows
    /// accumulating y, and remember that a row hidden by a fold has no wrapped
    /// lines and therefore no height — a guide must not stretch across one.
    ///
    /// One `Path` per colour, because `paint_path` takes a single colour.
    fn layout_bracket_guides(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &App,
    ) -> Vec<(Path<Pixels>, Hsla)> {
        let state = self.state.read(cx);
        let guides = state.mode.bracket_guides();
        if matches!(guides, brackets::BracketGuides::Off) {
            return vec![];
        }

        let skip = Self::skipped_ranges(&state, &last_layout.visible_range_offset);
        let language = state.mode.language_name();
        let mut pairs = brackets::pairs_in(
            &state.text,
            last_layout.visible_range_offset.clone(),
            language,
            &skip,
        );
        // The pair the caret sits in — `Always` so it is found even when the
        // caret is not next to a bracket. Used both to narrow the list down and
        // to draw that one stronger.
        let active = brackets::match_at(
            &state.text,
            state.cursor(),
            language,
            brackets::MatchBrackets::Always,
            last_layout.visible_range_offset.clone(),
            &skip,
        );
        if matches!(guides, brackets::BracketGuides::Active) {
            let Some((open, close)) = active.clone() else {
                return vec![];
            };
            pairs.retain(|(o, c, _)| *o == open && *c == close);
        }
        if pairs.is_empty() {
            return vec![];
        }

        // Where each visible row starts, in bytes and in pixels.
        let line_height = last_layout.line_height;
        let mut tops: Vec<(usize, usize, Pixels, usize)> = Vec::new();
        let mut offset_y = last_layout.visible_top;
        let mut row_offset = last_layout.visible_range_offset.start;
        for row in last_layout.visible_range.clone() {
            let Some(line) = last_layout.line(row) else {
                continue;
            };
            let rows = line.wrapped_lines.len();
            tops.push((row, row_offset, offset_y, rows));
            row_offset += line.len() + 1;
            offset_y += rows * line_height;
        }

        // The row a byte belongs to is the last one that starts at or before it.
        let row_of = |offset: usize| {
            tops.iter()
                .rfind(|(_, start, _, _)| *start <= offset)
                .copied()
        };

        let base = Self::bracket_base_color(cx);
        let mut by_color: Vec<(Hsla, gpui::PathBuilder)> = Vec::new();
        for (open, close, depth) in pairs {
            let (Some((o_row, o_start, o_top, o_rows)), Some((_, c_start, c_top, _))) =
                (row_of(open.start), row_of(close.start))
            else {
                continue;
            };
            if o_start == c_start || o_rows == 0 {
                // Same row (or folded away): nothing to connect.
                continue;
            }
            let Some(line) = last_layout.line(o_row) else {
                continue;
            };
            let Some(pos) = line.position_for_index(open.start - o_start, line_height) else {
                continue;
            };
            let x = pos.x + last_layout.line_number_width;
            let top = o_top + pos.y + line_height;
            let bottom = c_top;
            if bottom <= top {
                continue;
            }

            // The pair the caret is in is drawn at full strength so it stands
            // out of a screen full of guides.
            let is_active = state.mode.highlight_active_bracket_pair()
                && active
                    .as_ref()
                    .is_some_and(|(o, c)| *o == open && *c == close);
            let alpha = if is_active { 1.0 } else { 0.55 };
            let color = brackets::depth_color(base, depth).opacity(alpha);
            let builder = match by_color.iter_mut().find(|(c, _)| *c == color) {
                Some((_, b)) => b,
                None => {
                    by_color.push((color, gpui::PathBuilder::stroke(px(1.))));
                    &mut by_color.last_mut().expect("just pushed").1
                }
            };
            builder.move_to(point(x, top));
            builder.line_to(point(x, bottom));

            // A short stub at each end, pointing at the bracket it belongs to
            // (VS Code's `bracketPairsHorizontal`).
            if state.mode.bracket_guides_horizontal() {
                let stub = px(4.);
                builder.move_to(point(x, top));
                builder.line_to(point(x + stub, top));
                builder.move_to(point(x, bottom));
                builder.line_to(point(x + stub, bottom));
            }
        }

        by_color
            .into_iter()
            .filter_map(|(color, mut builder)| {
                builder.translate(bounds.origin);
                builder.build().ok().map(|path| (path, color))
            })
            .collect()
    }

    /// The colour bracket depth colouring counts up from.
    fn bracket_base_color(cx: &App) -> Hsla {
        cx.theme()
            .highlight_theme
            .style("punctuation.bracket")
            .and_then(|s| s.color)
            .or(cx.theme().highlight_theme.style.editor_foreground)
            .unwrap_or(cx.theme().foreground)
    }

    /// Outline the bracket the caret is next to and the one it matches.
    ///
    /// The scan is bounded by what is on screen: a partner further away could
    /// not be drawn anyway (`layout_match_range` clips to the visible range),
    /// and scanning the whole buffer every frame would be waste.
    fn layout_bracket_match(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &mut App,
    ) -> Vec<Path<Pixels>> {
        let state = self.state.read(cx);
        let mode = state.mode.match_brackets();
        if matches!(mode, brackets::MatchBrackets::Never) {
            return vec![];
        }
        // Brackets inside strings and comments are not code, so the scan has to
        // skip them. The captures come from the same query the highlighter
        // already runs — no walking the tree by hand.
        let skip = Self::skipped_ranges(&state, &last_layout.visible_range_offset);
        let Some((open, close)) = brackets::match_at(
            &state.text,
            state.cursor(),
            state.mode.language_name(),
            mode,
            last_layout.visible_range_offset.clone(),
            &skip,
        ) else {
            return vec![];
        };

        [open, close]
            .into_iter()
            .filter_map(|range| {
                Self::layout_match_range_with(range, last_layout, bounds, Some(px(1.)), None)
            })
            .collect()
    }

    fn layout_hover_highlight(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &mut App,
    ) -> Option<Path<Pixels>> {
        let hover_popover = self.state.read(cx).hover_popover.clone();
        let Some(symbol_range) = hover_popover.map(|popover| popover.read(cx).symbol_range.clone())
        else {
            return None;
        };

        Self::layout_match_range(symbol_range, last_layout, bounds)
    }

    fn layout_document_colors(
        &self,
        document_colors: &[(Range<usize>, Hsla)],
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
    ) -> Vec<(Path<Pixels>, Hsla)> {
        let mut paths = vec![];
        for (range, color) in document_colors.iter() {
            if let Some(path) = Self::layout_match_range(range.clone(), last_layout, bounds) {
                paths.push((path, *color));
            }
        }

        paths
    }

    /// Where else the symbol under the caret appears, as the server said.
    ///
    /// Drawn the same way as the document colours, because the painter takes
    /// a range and nothing else.
    fn layout_document_highlights(
        &self,
        last_layout: &LastLayout,
        bounds: &Bounds<Pixels>,
        cx: &App,
    ) -> Vec<(Path<Pixels>, Hsla)> {
        let state = self.state.read(cx);
        let found = state
            .lsp
            .document_highlights_for_range(&state.text, &last_layout.visible_range);
        if found.is_empty() {
            return vec![];
        }
        let base = cx.theme().selection;
        found
            .into_iter()
            .filter_map(|(range, kind)| {
                let color = match kind {
                    // A write stands out from a read -- that is the whole
                    // reason to ask a server rather than match strings.
                    crate::input::HighlightKind::Write => base.saturation(0.35),
                    _ => base.saturation(0.12),
                };
                Self::layout_match_range(range, last_layout, bounds).map(|p| (p, color))
            })
            .collect()
    }

    fn layout_selections(
        &self,
        last_layout: &LastLayout,
        bounds: &mut Bounds<Pixels>,
        cx: &mut App,
    ) -> Option<Path<Pixels>> {
        let state = self.state.read(cx);
        let mut selected_range = state.selected_range;
        if let Some(ime_marked_range) = &state.ime_marked_range {
            if !ime_marked_range.is_empty() {
                selected_range = (ime_marked_range.end..ime_marked_range.end).into();
            }
        }
        if selected_range.is_empty() {
            return None;
        }

        if state.masked {
            // Because masked use `*`, 1 char with 1 byte.
            selected_range.start = state.text.offset_to_char_index(selected_range.start);
            selected_range.end = state.text.offset_to_char_index(selected_range.end);
        }

        let (start_ix, end_ix) = if selected_range.start < selected_range.end {
            (selected_range.start, selected_range.end)
        } else {
            (selected_range.end, selected_range.start)
        };

        let range = start_ix.max(last_layout.visible_range_offset.start)
            ..end_ix.min(last_layout.visible_range_offset.end);

        Self::layout_match_range_rounded(range, &last_layout, bounds, selection_radius(state))
    }

    /// Calculate the visible range of lines in the viewport.
    ///
    /// Returns
    ///
    /// - visible_range: The visible range is based on unwrapped lines (Zero based).
    /// - visible_top: The top position of the first visible line in the scroll viewport.
    fn calculate_visible_range(
        &self,
        state: &InputState,
        line_height: Pixels,
        input_height: Pixels,
    ) -> (Range<usize>, Pixels) {
        // Add extra rows to avoid showing empty space when scroll to bottom.
        let extra_rows = 1;
        // The pad above the first row is part of the content: the first row
        // genuinely starts that far down, so every walk seeded from
        // `visible_top` moves with it (dopamine #255 / ADR-0090).
        let mut visible_top = state.mode.padding_top();
        if state.mode.is_single_line() {
            return (0..1, px(0.));
        }

        // `visible_range` is a range of **buffer rows**, so it has to be seeded
        // from the row count -- `TextWrapper::len()` is the *visual* line count,
        // which is only ever >= the row count while soft wrap is the sole reason
        // the two differ. Anything that makes a row occupy fewer visual lines
        // than one (folding) would clamp the range short and stop laying out the
        // tail of the document.
        let total_rows = state.text_wrapper.lines.len();
        let scroll_top = if let Some(deferred_scroll_offset) = state.deferred_scroll_offset {
            deferred_scroll_offset.y
        } else {
            state.scroll_handle.offset().y
        };

        let mut visible_range = 0..total_rows;
        let mut line_bottom = visible_top;
        for (ix, line) in state.text_wrapper.lines.iter().enumerate() {
            let wrapped_height = line.height(line_height);
            line_bottom += wrapped_height;

            if line_bottom < -scroll_top {
                visible_top = line_bottom - wrapped_height;
                visible_range.start = ix;
            }

            if line_bottom + scroll_top >= input_height {
                visible_range.end = (ix + extra_rows).min(total_rows);
                break;
            }
        }

        (visible_range, visible_top)
    }

    /// Return (line_number_width, line_number_len)
    fn layout_line_numbers(
        state: &InputState,
        text: &Rope,
        font_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
    ) -> (Pixels, usize) {
        let total_lines = text.lines_len();
        // What the last number actually needs, never below the caller's floor.
        // The extra column is the gap between the number and the text; drop it
        // and the two run together.
        let digits = total_lines.max(1).to_string().len();
        let line_number_len = digits.max(state.mode.min_line_number_digits()) + 1;

        let line_number_width = if state.mode.line_number() {
            let empty_line_number = window.text_system().shape_line(
                "+".repeat(line_number_len).into(),
                font_size,
                &[TextRun {
                    len: line_number_len,
                    font: style.font(),
                    color: gpui::black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );

            empty_line_number.width
                + px(6.)
                + LINE_NUMBER_RIGHT_MARGIN
                + fold_chevron_width(state)
        } else {
            fold_chevron_width(state)
        };

        (line_number_width, line_number_len)
    }

    /// Compute inline completion ghost lines for rendering.
    ///
    /// Returns (first_line, ghost_lines) where:
    /// - first_line: Shaped text for the first line (goes after cursor on same line)
    /// - ghost_lines: Shaped lines for subsequent lines (shift content down)
    fn layout_inline_completion(
        state: &InputState,
        visible_range: &Range<usize>,
        font_size: Pixels,
        window: &mut Window,
        cx: &App,
    ) -> (Option<ShapedLine>, Vec<ShapedLine>) {
        // Must be focused to show inline completion
        if !state.focus_handle.is_focused(window) {
            return (None, vec![]);
        }

        let Some(completion_item) = state.inline_completion.item.as_ref() else {
            return (None, vec![]);
        };

        // Get cursor row from cursor position
        let cursor_row = state.cursor_position().line as usize;

        // Only show if cursor row is visible
        if cursor_row < visible_range.start || cursor_row >= visible_range.end {
            return (None, vec![]);
        }

        let completion_text = &completion_item.insert_text;
        let completion_color = cx.theme().muted_foreground.opacity(0.5);

        let text_style = window.text_style();
        let font = text_style.font();

        let lines: Vec<&str> = completion_text.split('\n').collect();
        if lines.is_empty() {
            return (None, vec![]);
        }

        // Shape first line (goes after cursor)
        let first_text: SharedString = lines[0].to_string().into();
        let first_line = if !first_text.is_empty() {
            let first_run = TextRun {
                len: first_text.len(),
                font: font.clone(),
                color: completion_color,
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            Some(
                window
                    .text_system()
                    .shape_line(first_text, font_size, &[first_run], None),
            )
        } else {
            None
        };

        // Shape ghost lines (lines 2+ that shift content down)
        let ghost_lines: Vec<ShapedLine> = lines[1..]
            .iter()
            .map(|line_text| {
                let text: SharedString = line_text.to_string().into();
                let len = text.len().max(1); // Ensure at least 1 for empty lines
                let run = TextRun {
                    len,
                    font: font.clone(),
                    color: completion_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                // Use space for empty lines so they take up height
                let shaped_text = if text.is_empty() { " ".into() } else { text };
                window
                    .text_system()
                    .shape_line(shaped_text, font_size, &[run], None)
            })
            .collect();

        (first_line, ghost_lines)
    }

    fn layout_lines(
        state: &InputState,
        display_text: &Rope,
        last_layout: &LastLayout,
        font_size: Pixels,
        runs: &[TextRun],
        bg_segments: &[(Range<usize>, Hsla)],
        window: &mut Window,
    ) -> Vec<LineLayout> {
        let is_single_line = state.mode.is_single_line();
        let text_wrapper = &state.text_wrapper;
        let visible_range = &last_layout.visible_range;
        let visible_range_offset = &last_layout.visible_range_offset;

        if is_single_line {
            let shaped_line = window.text_system().shape_line(
                display_text.to_string().into(),
                font_size,
                &runs,
                None,
            );

            return vec![LineLayout::new().lines(smallvec::smallvec![shaped_line])];
        }

        // Empty to use placeholder, the placeholder is not in the text_wrapper map.
        if state.text.len() == 0 {
            return display_text
                .to_string()
                .split("\n")
                .map(|line| {
                    let shaped_line = window.text_system().shape_line(
                        line.to_string().into(),
                        font_size,
                        &runs,
                        None,
                    );
                    LineLayout::new().lines(smallvec::smallvec![shaped_line])
                })
                .collect();
        }

        let visible_text = display_text
            .slice_lines(visible_range.start..visible_range.end)
            .to_string();

        let mut lines = vec![];
        let mut offset = 0;
        for (ix, line) in visible_text.split("\n").enumerate() {
            let line_item = text_wrapper
                .lines
                .get(visible_range.start + ix)
                .expect("line should exists in text_wrapper");

            debug_assert_eq!(line_item.len(), line.len());

            // A row hidden by a fold is still emitted, so `visible_range.start
            // + ix == row` keeps holding for every consumer -- it just has no
            // visual lines. See `LineLayout::folded`.
            if line_item.hidden {
                lines.push(LineLayout::folded(line.len()));
                offset += line.len() + 1;
                continue;
            }

            let mut line_layout = LineLayout::new();
            let mut wrapped_lines = SmallVec::with_capacity(1);

            for range in &line_item.wrapped_lines {
                let line_runs = runs_for_range(runs, offset, &range);
                let line_runs = if bg_segments.is_empty() {
                    line_runs
                } else {
                    split_runs_by_bg_segments(
                        visible_range_offset.start + offset,
                        &line_runs,
                        bg_segments,
                    )
                };

                let sub_line: SharedString = line[range.clone()].to_string().into();
                let shaped_line = window
                    .text_system()
                    .shape_line(sub_line, font_size, &line_runs, None);

                wrapped_lines.push(shaped_line);
            }

            line_layout.set_wrapped_lines(wrapped_lines);
            lines.push(line_layout);

            // +1 for the `\n`
            offset += line.len() + 1;
        }

        lines
    }

    /// First usize is the offset of skipped.
    fn highlight_lines(
        &mut self,
        visible_range: &Range<usize>,
        _visible_top: Pixels,
        visible_byte_range: Range<usize>,
        cx: &mut App,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        let state = self.state.read(cx);
        let text = &state.text;
        let is_multi_line = state.mode.is_multi_line();

        let (highlighter, diagnostics) = match &state.mode {
            InputMode::CodeEditor {
                highlighter,
                diagnostics,
                ..
            } => (highlighter.borrow(), diagnostics),
            _ => return None,
        };
        let highlighter = highlighter.as_ref()?;

        let mut offset = visible_byte_range.start;
        let mut styles = vec![];

        for line in text
            .iter_lines()
            .skip(visible_range.start)
            .take(visible_range.len())
        {
            let line_len = if is_multi_line {
                // +1 for `\n`
                line.len() + 1
            } else {
                line.len()
            };

            let range = offset..offset + line_len;
            let line_styles = highlighter.styles(&range, &cx.theme().highlight_theme);
            styles = gpui::combine_highlights(styles, line_styles).collect();

            offset = range.end;
        }

        let diagnostic_styles = diagnostics.styles_for_range(
            &visible_byte_range,
            self.state.read(cx).mode.diagnostic_tag_style(),
            cx,
        );

        // hover definition style
        if let Some(hover_style) = self.layout_hover_definition(cx) {
            styles.push(hover_style);
        }

        // A link the pointer is on, with the modifier held (#256).
        if let Some(link_style) = self.layout_link_hover(cx) {
            styles.push(link_style);
        }

        // Combine marker styles
        styles = gpui::combine_highlights(diagnostic_styles, styles).collect();

        // Colour bracket pairs by nesting depth (VS Code's
        // `bracketPairColorization`).
        //
        // **Overwritten, not layered.** `combine_highlights` folds overlapping
        // styles through an unordered set, so layering a colour on top of the
        // `punctuation.bracket` one the syntax pass already produced would pick
        // a winner at random and flicker between frames.
        if state.mode.bracket_colors() {
            let skip = highlighter.skipped_ranges(&visible_byte_range);
            let base = cx
                .theme()
                .highlight_theme
                .style("punctuation.bracket")
                .and_then(|s| s.color)
                .or(cx.theme().highlight_theme.style.editor_foreground)
                .unwrap_or(cx.theme().foreground);
            let colors: Vec<(Range<usize>, Hsla)> = brackets::depths_in(
                text,
                visible_byte_range.clone(),
                state.mode.language_name(),
                &skip,
                state.mode.bracket_colors_per_type(),
            )
            .into_iter()
            .map(|(range, depth)| (range, brackets::depth_color(base, depth)))
            .collect();
            styles = overwrite_colors(styles, &colors);
        }

        Some(styles)
    }
}

/// Force `color` onto the styles covering each range in `overlay`.
///
/// Ranges that straddle an overlay entry are split so the rest of the run keeps
/// its own colour. `overlay` is sorted and its entries never overlap (they are
/// single brackets), so one pass is enough.
fn overwrite_colors(
    styles: Vec<(Range<usize>, HighlightStyle)>,
    overlay: &[(Range<usize>, Hsla)],
) -> Vec<(Range<usize>, HighlightStyle)> {
    if overlay.is_empty() {
        return styles;
    }
    let mut out = Vec::with_capacity(styles.len() + overlay.len() * 2);
    for (range, style) in styles {
        let mut at = range.start;
        for (hit, color) in overlay.iter().filter(|(r, _)| {
            r.start < range.end && r.end > range.start
        }) {
            if hit.start > at {
                out.push((at..hit.start, style));
            }
            let mut colored = style;
            colored.color = Some(*color);
            out.push((hit.start.max(range.start)..hit.end.min(range.end), colored));
            at = hit.end.min(range.end);
        }
        if at < range.end {
            out.push((at..range.end, style));
        }
    }
    out
}

pub(super) struct PrepaintState {
    /// The lines of entire lines.
    last_layout: LastLayout,
    /// The lines only contains the visible lines in the viewport, based on `visible_range`.
    ///
    /// The child is the soft lines.
    line_numbers: Option<Vec<SmallVec<[ShapedLine; 1]>>>,
    /// Size of the scrollable area by entire lines.
    scroll_size: Size<Pixels>,
    cursor_bounds: Option<Bounds<Pixels>>,
    cursor_scroll_offset: Point<Pixels>,
    /// row index (zero based), no wrap, same line as the cursor.
    current_row: Option<usize>,
    selection_path: Option<Path<Pixels>>,
    hover_highlight_path: Option<Path<Pixels>>,
    search_match_paths: Vec<(Path<Pixels>, bool)>,
    /// Every other run of the selected text, faintly marked.
    selection_highlight_paths: Vec<Path<Pixels>>,
    /// The outlines of the bracket next to the caret and its partner
    bracket_match_paths: Vec<Path<Pixels>>,
    document_color_paths: Vec<(Path<Pixels>, Hsla)>,
    hover_definition_hitbox: Option<Hitbox>,
    link_hitbox: Option<Hitbox>,
    document_highlight_paths: Vec<(Path<Pixels>, Hsla)>,
    indent_guides_path: Option<Path<Pixels>>,
    rulers_path: Option<Path<Pixels>>,
    /// One vertical guide per bracket pair, grouped by colour
    bracket_guide_paths: Vec<(Path<Pixels>, Hsla)>,
    whitespaces: Vec<crate::input::whitespace::PlacedMark>,
    fold_chevrons: Vec<crate::input::fold::FoldChevron>,
    /// The chevron strip, so the cursor turns into a hand over it.
    fold_gutter_hitbox: Option<Hitbox>,
    /// The `…` badge drawn after a folded header, keyed by index into
    /// `LastLayout::lines`.
    fold_markers: Vec<(usize, ShapedLine)>,
    /// The inlay hints drawn after the end of a row, keyed the same way.
    inlay_hints: Vec<(usize, ShapedLine)>,
    /// The diagnostics drawn after the end of a row, keyed the same way.
    inline_diagnostics: Vec<(usize, ShapedLine)>,
    bounds: Bounds<Pixels>,
    // Inline completion rendering data
    /// Shaped ghost lines to paint after cursor row (completion lines 2+)
    ghost_lines: Vec<ShapedLine>,
    /// First line of inline completion (painted after cursor on same line)
    ghost_first_line: Option<ShapedLine>,
    ghost_lines_height: Pixels,
}

impl PrepaintState {
    /// Returns cursor bounds adjusted for scroll offset, if available.
    fn cursor_bounds_with_scroll(&self) -> Option<Bounds<Pixels>> {
        self.cursor_bounds.map(|bounds| Self::with_scroll(bounds, self.cursor_scroll_offset))
    }

    /// Move a caret rectangle from text coordinates into the viewport.
    fn with_scroll(mut bounds: Bounds<Pixels>, scroll: Point<Pixels>) -> Bounds<Pixels> {
        bounds.origin.x += scroll.x;
        bounds.origin.y += scroll.y;
        bounds
    }
}

impl IntoElement for TextElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// A debug function to print points as SVG path.
#[allow(unused)]
fn print_points_as_svg_path(
    line_corners: &Vec<Corners<Point<Pixels>>>,
    points: &Vec<Point<Pixels>>,
) {
    for corners in line_corners {
        println!(
            "tl: ({}, {}), tr: ({}, {}), bl: ({}, {}), br: ({}, {})",
            corners.top_left.x.as_f32() as i32,
            corners.top_left.y.as_f32() as i32,
            corners.top_right.x.as_f32() as i32,
            corners.top_right.y.as_f32() as i32,
            corners.bottom_left.x.as_f32() as i32,
            corners.bottom_left.y.as_f32() as i32,
            corners.bottom_right.x.as_f32() as i32,
            corners.bottom_right.y.as_f32() as i32,
        );
    }

    if points.len() > 0 {
        println!(
            "M{},{}",
            points[0].x.as_f32() as i32,
            points[0].y.as_f32() as i32
        );
        for p in points.iter().skip(1) {
            println!("L{},{}", p.x.as_f32() as i32, p.y.as_f32() as i32);
        }
    }
}

impl Element for TextElement {
    type RequestLayoutState = ();
    type PrepaintState = PrepaintState;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let state = self.state.read(cx);
        let line_height = window.line_height();

        let mut style = Style::default();
        style.size.width = relative(1.).into();
        if state.mode.is_multi_line() {
            style.flex_grow = 1.0;
            style.size.height = relative(1.).into();
            if state.mode.is_auto_grow() {
                // Auto grow to let height match to rows, but not exceed max rows.
                let rows = state.mode.max_rows().min(state.mode.rows());
                style.min_size.height = (rows * line_height).into();
            } else {
                style.min_size.height = line_height.into();
            }
        } else {
            // For single-line inputs, the minimum height should be the line height
            style.size.height = line_height.into();
        };

        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let style = window.text_style();
        let font = style.font();
        let text_size = style.font_size.to_pixels(window.rem_size());

        self.state.update(cx, |state, cx| {
            state.text_wrapper.set_font(font, text_size, cx);
            state.text_wrapper.prepare_if_need(&state.text, cx);
        });

        let state = self.state.read(cx);
        let line_height = window.line_height();

        let (visible_range, visible_top) =
            self.calculate_visible_range(&state, line_height, bounds.size.height);
        let visible_start_offset = state.text.line_start_offset(visible_range.start);
        let visible_end_offset = state
            .text
            .line_end_offset(visible_range.end.saturating_sub(1));

        let highlight_styles = self.highlight_lines(
            &visible_range,
            visible_top,
            visible_start_offset..visible_end_offset,
            cx,
        );

        let state = self.state.read(cx);
        let multi_line = state.mode.is_multi_line();
        let text = state.text.clone();
        let is_empty = text.len() == 0;
        let placeholder = self.placeholder.clone();

        // The element's own rectangle, before the scroll offset moves the
        // text. Anything pinned to the viewport (rulers) measures from this.
        let unscrolled_bounds = bounds;
        let mut bounds = bounds;

        let (display_text, text_color) = if is_empty {
            (
                &Rope::from(placeholder.as_str()),
                cx.theme().muted_foreground,
            )
        } else if state.masked {
            (
                &Rope::from("*".repeat(text.chars().count())),
                cx.theme().foreground,
            )
        } else {
            (&text, cx.theme().foreground)
        };

        let text_style = window.text_style();

        // Calculate the width of the line numbers
        let (line_number_width, line_number_len) =
            Self::layout_line_numbers(&state, &text, text_size, &text_style, window);

        let wrap_width = if multi_line && state.soft_wrap {
            let viewport = bounds.size.width - line_number_width - RIGHT_MARGIN;
            // A fixed column has to be measured the same way the rulers are,
            // or the text would not break on the line that marks it.
            let column = |n: usize| {
                crate::input::rulers::column_advance(&text_style, text_size, window) * n as f32
            };
            Some(match state.mode.wrap_at() {
                WrapAt::EditorWidth => viewport,
                WrapAt::Column(n) => column(n),
                WrapAt::Bounded(n) => column(n).min(viewport),
            })
        } else {
            None
        };

        let mut last_layout = LastLayout {
            visible_range,
            visible_top,
            visible_range_offset: visible_start_offset..visible_end_offset,
            line_height,
            wrap_width,
            line_number_width,
            lines: Rc::new(vec![]),
            cursor_bounds: None,
        };

        let run = TextRun {
            len: display_text.len(),
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: None,
            strikethrough: None,
        };
        let marked_run = TextRun {
            len: 0,
            font: style.font(),
            color: text_color,
            background_color: None,
            underline: Some(UnderlineStyle {
                thickness: px(1.),
                color: Some(text_color),
                wavy: false,
            }),
            strikethrough: None,
        };

        let runs = if !is_empty {
            if let Some(highlight_styles) = highlight_styles {
                let mut runs = vec![];

                runs.extend(highlight_styles.iter().map(|(range, style)| {
                    let mut run = text_style.clone().highlight(*style).to_run(range.len());
                    if let Some(ime_marked_range) = &state.ime_marked_range {
                        if range.start >= ime_marked_range.start
                            && range.end <= ime_marked_range.end
                        {
                            run.color = marked_run.color;
                            run.strikethrough = marked_run.strikethrough;
                            run.underline = marked_run.underline;
                        }
                    }

                    run
                }));

                runs.into_iter().filter(|run| run.len > 0).collect()
            } else {
                vec![run]
            }
        } else if let Some(ime_marked_range) = &state.ime_marked_range {
            // IME marked text
            vec![
                TextRun {
                    len: ime_marked_range.start,
                    ..run.clone()
                },
                TextRun {
                    len: ime_marked_range.end - ime_marked_range.start,
                    underline: marked_run.underline,
                    ..run.clone()
                },
                TextRun {
                    len: display_text.len() - ime_marked_range.end,
                    ..run.clone()
                },
            ]
            .into_iter()
            .filter(|run| run.len > 0)
            .collect()
        } else {
            vec![run]
        };

        let document_colors = state
            .lsp
            .document_colors_for_range(&text, &last_layout.visible_range);
        let lines = Self::layout_lines(
            &state,
            &display_text,
            &last_layout,
            text_size,
            &runs,
            &document_colors,
            window,
        );

        let mut longest_line_width = wrap_width.unwrap_or(px(0.));
        // 1. Single line
        // 2. Multi-line with soft wrap disabled.
        if state.mode.is_single_line() || !state.soft_wrap {
            let longest_row = state.text_wrapper.longest_row.row;
            let longest_line: SharedString = state.text.slice_line(longest_row).to_string().into();
            longest_line_width = window
                .text_system()
                .shape_line(
                    longest_line.clone(),
                    text_size,
                    &[TextRun {
                        len: longest_line.len(),
                        font: style.font(),
                        color: gpui::black(),
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    wrap_width,
                )
                .width;
        }
        last_layout.lines = Rc::new(lines);

        let (ghost_first_line, ghost_lines) = Self::layout_inline_completion(
            state,
            &last_layout.visible_range,
            text_size,
            window,
            cx,
        );
        let ghost_line_count = ghost_lines.len();
        let ghost_lines_height = ghost_line_count as f32 * line_height;

        let total_wrapped_lines = state.text_wrapper.len();
        let empty_bottom_height = if state.mode.is_code_editor() {
            bounds
                .size
                .height
                .half()
                .max(BOTTOM_MARGIN_ROWS * line_height)
        } else {
            px(0.)
        };

        let scroll_size = size(
            if longest_line_width + line_number_width + RIGHT_MARGIN > bounds.size.width {
                longest_line_width + line_number_width + RIGHT_MARGIN
            } else {
                longest_line_width
            },
            (total_wrapped_lines as f32 * line_height
                + empty_bottom_height
                + ghost_lines_height
                // Without these the last row cannot be scrolled to, and the
                // pad below it would never come into view (#255 / ADR-0090).
                + state.mode.padding_top()
                + state.mode.padding_bottom())
            .max(bounds.size.height),
        );

        // `position_for_index` for example
        //
        // #### text
        //
        // Hello 世界，this is GPUI component.
        // The GPUI Component is a collection of UI components for
        // GPUI framework, including Button, Input, Checkbox, Radio,
        // Dropdown, Tab, and more...
        //
        // wrap_width: 444px, line_height: 20px
        //
        // #### lines[0]
        //
        // | index | pos              | line |
        // |-------|------------------|------|
        // | 5     | (37 px, 0.0)     | 0    |
        // | 38    | (261.7 px, 20.0) | 0    |
        // | 40    | None             | -    |
        //
        // #### lines[1]
        //
        // | index | position              | line |
        // |-------|-----------------------|------|
        // | 5     | (43.578125 px, 0.0)   | 0    |
        // | 56    | (422.21094 px, 0.0)   | 0    |
        // | 57    | (11.6328125 px, 20.0) | 1    |
        // | 114   | (429.85938 px, 20.0)  | 1    |
        // | 115   | (11.3125 px, 40.0)    | 2    |

        // Calculate the scroll offset to keep the cursor in view

        let (cursor_bounds, cursor_scroll_offset, current_row) =
            self.layout_cursor(&last_layout, &mut bounds, window, cx);
        last_layout.cursor_bounds = cursor_bounds;

        let search_match_paths = self.layout_search_matches(&last_layout, &mut bounds, cx);
        let selection_highlight_paths =
            self.layout_selection_highlights(&last_layout, &bounds, cx);
        let selection_path = self.layout_selections(&last_layout, &mut bounds, cx);
        let hover_highlight_path = self.layout_hover_highlight(&last_layout, &mut bounds, cx);
        let bracket_match_paths = self.layout_bracket_match(&last_layout, &bounds, cx);
        let document_color_paths =
            self.layout_document_colors(&document_colors, &last_layout, &bounds);
        let document_highlight_paths =
            self.layout_document_highlights(&last_layout, &bounds, cx);

        let state = self.state.read(cx);
        let line_numbers_mode = state.mode.line_numbers();
        // The row a trailing newline leaves behind, when it is not to be
        // numbered. A buffer ending in `\n` has one more (empty) line; an
        // empty buffer has a single line and no trailing newline to blame.
        let unnumbered_final_row = (!state.mode.render_final_newline()
            && state.text.len() > 0
            && state.text.char_at(state.text.len().saturating_sub(1)) == Some('\n'))
        .then(|| state.text.lines_len().saturating_sub(1));
        let line_numbers = if line_numbers_mode.is_visible() {
            let mut line_numbers = vec![];
            let other_line_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().muted_foreground,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];
            let current_line_runs = vec![TextRun {
                len: line_number_len,
                font: style.font(),
                color: cx.theme().foreground,
                background_color: None,
                underline: None,
                strikethrough: None,
            }];

            // build line numbers
            for (ix, line) in last_layout.lines.iter().enumerate() {
                let ix = last_layout.visible_range.start + ix;
                // Folded away: no number, but still an entry so the index keeps
                // matching the row. (An empty *buffer* line still has one
                // wrapped line, so this only ever means "hidden".)
                if line.wrapped_lines.is_empty() {
                    line_numbers.push(SmallVec::new());
                    continue;
                }
                // `Relative` / `Interval` do not simply count up, and may leave a
                // row blank. Pad to `line_number_len` either way so the shaped
                // line keeps matching the `TextRun` length below.
                let line_no: SharedString = match line_numbers_mode.number_for(ix, current_row) {
                    // The trailing newline's row, when it is not numbered.
                    // Blank rather than skipped: the entry has to stay so the
                    // index keeps matching the row.
                    _ if Some(ix) == unnumbered_final_row => " ".repeat(line_number_len),
                    Some(no) => format!("{:>width$}", no, width = line_number_len),
                    None => " ".repeat(line_number_len),
                }
                .into();

                let runs = if current_row == Some(ix) {
                    &current_line_runs
                } else {
                    &other_line_runs
                };

                let mut sub_lines: SmallVec<[ShapedLine; 1]> = SmallVec::new();
                sub_lines.push(
                    window
                        .text_system()
                        .shape_line(line_no, text_size, &runs, None),
                );
                for _ in 0..line.wrapped_lines.len().saturating_sub(1) {
                    sub_lines.push(ShapedLine::default());
                }
                line_numbers.push(sub_lines);
            }
            Some(line_numbers)
        } else {
            None
        };

        let hover_definition_hitbox = self.layout_hover_definition_hitbox(state, window, cx);
        let link_hitbox = self.layout_link_hitbox(state, window, cx);
        let indent_guides_path =
            self.layout_indent_guides(state, &bounds, &last_layout, &text_style, window);
        let rulers_path = Self::layout_rulers(
            state,
            &unscrolled_bounds,
            &bounds,
            &last_layout,
            &text_style,
            window,
        );
        let bracket_guide_paths = self.layout_bracket_guides(&last_layout, &bounds, cx);
        let whitespaces = self.layout_whitespaces(state, &bounds, &last_layout);
        let fold_chevrons =
            self.layout_fold_chevrons(state, &last_layout, text_size, &text_style, window, cx);
        let fold_gutter_hitbox = (state.mode.has_folding() && !fold_chevrons.is_empty()).then(|| {
            let x = fold_chevron_x(state.input_bounds.origin.x, last_layout.line_number_width);
            window.insert_hitbox(
                Bounds::new(
                    point(x, state.input_bounds.origin.y),
                    size(FOLD_CHEVRON_WIDTH, state.input_bounds.size.height),
                ),
                gpui::HitboxBehavior::Normal,
            )
        });
        let fold_markers =
            self.layout_fold_markers(state, &last_layout, text_size, &text_style, window, cx);
        let inlay_hints =
            self.layout_inlay_hints(state, &last_layout, text_size, &text_style, window, cx);
        let inline_diagnostics = self
            .layout_inline_diagnostics(state, &last_layout, text_size, &text_style, window, cx);

        PrepaintState {
            bounds,
            last_layout,
            scroll_size,
            line_numbers,
            cursor_bounds,
            cursor_scroll_offset,
            current_row,
            selection_path,
            search_match_paths,
            selection_highlight_paths,
            bracket_match_paths,
            hover_highlight_path,
            hover_definition_hitbox,
            link_hitbox,
            document_highlight_paths,
            document_color_paths,
            indent_guides_path,
            rulers_path,
            bracket_guide_paths,
            whitespaces,
            fold_chevrons,
            fold_gutter_hitbox,
            fold_markers,
            inlay_hints,
            inline_diagnostics,
            ghost_first_line,
            ghost_lines,
            ghost_lines_height,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        input_bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.state.read(cx).focus_handle.clone();
        let show_cursor = self.state.read(cx).show_cursor(window, cx);
        let focused = focus_handle.is_focused(window);
        let bounds = prepaint.bounds;
        let selected_range = self.state.read(cx).selected_range;
        let visible_range = &prepaint.last_layout.visible_range;

        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.state.clone()),
            cx,
        );

        // Set Root focused_input when self is focused
        if focused {
            let state = self.state.clone();
            if Root::read(window, cx).focused_input.as_ref() != Some(&state) {
                Root::update(window, cx, |root, _, cx| {
                    root.focused_input = Some(state);
                    cx.notify();
                });
            }
        }

        // And reset focused_input when next_frame start
        window.on_next_frame({
            let state = self.state.clone();
            move |window, cx| {
                if !focused && Root::read(window, cx).focused_input.as_ref() == Some(&state) {
                    Root::update(window, cx, |root, _, cx| {
                        root.focused_input = None;
                        cx.notify();
                    });
                }
            }
        });

        // Paint multi line text
        let line_height = window.line_height();
        let origin = bounds.origin;

        let invisible_top_padding = prepaint.last_layout.visible_top;

        let mut mask_offset_y = px(0.);
        let state = self.state.read(cx);
        let inlay_chip = state.mode.inlay_hint_background();
        // The width of one column, measured the way the rulers measure it
        // (#256). Hoisted: `state` is borrowed from `cx`, and the paint loop
        // may not reborrow it.
        let inline_diagnostic_padding = f32::from(state.mode.inline_diagnostic_padding());
        let inline_diagnostic_min_column = f32::from(state.mode.inline_diagnostic_min_column());
        let text_style = window.text_style();
        let column_advance = crate::input::rulers::column_advance(
            &text_style,
            text_style.font_size.to_pixels(window.rem_size()),
            window,
        );
        if state.masked && state.text.len() > 0 {
            // Move down offset for vertical centering the *****
            if cfg!(target_os = "macos") {
                mask_offset_y = px(3.);
            } else {
                mask_offset_y = px(2.5);
            }
        }

        // Paint the band behind folded rows, under everything else.
        //
        // Its own walk rather than a branch inside the active-line loop below:
        // that one only runs when the line number gutter is on, and folding
        // does not need the gutter.
        if state.mode.fold_highlight() && !state.folded_rows.is_empty() {
            let color = cx.theme().selection.opacity(0.5);
            let mut fold_y = invisible_top_padding;
            for row in visible_range.clone() {
                let Some(line) = prepaint.last_layout.line(row) else {
                    continue;
                };
                let height = line_height * line.wrapped_lines.len() as f32;
                if state.is_folded(row) && height > px(0.) {
                    window.paint_quad(fill(
                        Bounds::new(
                            point(input_bounds.origin.x, origin.y + fold_y),
                            size(bounds.size.width, height),
                        ),
                        color,
                    ));
                }
                fold_y += height;
            }
        }

        let active_line_color = cx.theme().highlight_theme.style.editor_active_line;
        // How the caret's row is marked out (dopamine #255 / ADR-0090).
        // `focused` was worked out at the top of `paint`.
        let line_highlight = state.mode.line_highlight();
        let highlight_row = (!state.mode.line_highlight_focused_only() || focused)
            .then_some(prepaint.current_row)
            .flatten();

        // Paint the band behind the caret's row.
        //
        // **Walked over the laid-out lines, not the line numbers.** It used to
        // hang off `prepaint.line_numbers`, so turning the gutter off took the
        // band with it -- the two have nothing to do with each other.
        if line_highlight.highlights_line() {
            let mut offset_y = invisible_top_padding;
            for (ix, line) in prepaint.last_layout.lines.iter().enumerate() {
                let row = visible_range.start + ix;
                let height = line.size(line_height).height;
                if Some(row) == highlight_row {
                    if let Some(bg_color) = active_line_color {
                        // The gutter has its own band; this one starts where
                        // the text does so `Line` and `Gutter` can be told
                        // apart.
                        let x = input_bounds.origin.x + prepaint.last_layout.line_number_width;
                        window.paint_quad(fill(
                            Bounds::new(
                                point(x, origin.y + offset_y),
                                size(bounds.size.width, height),
                            ),
                            bg_color,
                        ));
                    }
                }
                offset_y += height;
            }
        }

        // Paint the rulers under the indent guides, and fainter than them:
        // the two cross each other, and the ruler is the quieter mark.
        if let Some(path) = prepaint.rulers_path.take() {
            window.paint_path(path, cx.theme().border.opacity(0.6));
        }

        // Paint indent guides
        if let Some(path) = prepaint.indent_guides_path.take() {
            window.paint_path(path, cx.theme().border.opacity(0.85));
        }

        // Paint bracket pair guides, in the colour of the pair they belong to
        for (path, color) in prepaint.bracket_guide_paths.iter() {
            window.paint_path(path.clone(), *color);
        }

        // Paint selections
        if window.is_window_active() {
            let secondary_selection = cx.theme().selection.saturation(0.1);
            // The selection's twins, in the same faint colour the inactive
            // search matches use -- they mean the same thing to the reader.
            for path in prepaint.selection_highlight_paths.iter() {
                window.paint_path(path.clone(), secondary_selection);
            }
            for (path, is_active) in prepaint.search_match_paths.iter() {
                window.paint_path(path.clone(), secondary_selection);

                if *is_active {
                    window.paint_path(path.clone(), cx.theme().selection);
                }
            }

            if let Some(path) = prepaint.selection_path.take() {
                window.paint_path(path, cx.theme().selection);
            }

            // Paint the matching bracket's outline. Drawn after the selection
            // so it stays readable inside one.
            for path in prepaint.bracket_match_paths.iter() {
                window.paint_path(path.clone(), cx.theme().selection);
            }

            // Paint hover highlight
            if let Some(path) = prepaint.hover_highlight_path.take() {
                window.paint_path(path, secondary_selection);
            }
        }

        // Paint the server's occurrence highlights (#253).
        for (path, color) in prepaint.document_highlight_paths.iter() {
            window.paint_path(path.clone(), *color);
        }

        // Paint document colors
        for (path, color) in prepaint.document_color_paths.iter() {
            window.paint_path(path.clone(), *color);
        }

        // Paint text with inline completion ghost line support
        let mut offset_y = mask_offset_y + invisible_top_padding;
        let ghost_lines = &prepaint.ghost_lines;
        let has_ghost_lines = !ghost_lines.is_empty();

        for (ix, line) in prepaint.last_layout.lines.iter().enumerate() {
            let row = visible_range.start + ix;
            let p = point(
                origin.x + prepaint.last_layout.line_number_width,
                origin.y + offset_y,
            );

            // Paint the actual line
            _ = line.paint(p, line_height, window, cx);

            // `…` after a folded header, on its **last** wrapped line -- not at
            // `longest_width`, which would float mid-paragraph when the header
            // itself wraps.
            if let Some((_, marker)) = prepaint.fold_markers.iter().find(|(mix, _)| *mix == ix) {
                let last = line.wrapped_lines.last();
                let marker_p = point(
                    p.x + last.map(|l| l.width).unwrap_or(px(0.)) + px(4.),
                    p.y + line_height * (line.wrapped_lines.len().saturating_sub(1)) as f32,
                );
                _ = marker.paint(marker_p, line_height, window, cx);
            }

            // The inlay hints for this row, after the end of it -- and after
            // the fold badge when the row has both. Nothing reads these glyphs
            // for offsets, so the code itself does not move (see `inlay.rs`).
            if let Some((_, hint)) = prepaint.inlay_hints.iter().find(|(hix, _)| *hix == ix) {
                let last = line.wrapped_lines.last();
                let badge = prepaint
                    .fold_markers
                    .iter()
                    .find(|(mix, _)| *mix == ix)
                    .map_or(px(0.), |(_, m)| m.width + px(4.));
                let hint_p = point(
                    p.x + last.map_or(px(0.), |l| l.width) + badge + INLAY_GAP,
                    p.y + line_height * (line.wrapped_lines.len().saturating_sub(1)) as f32,
                );
                if inlay_chip {
                    window.paint_quad(
                        gpui::fill(
                            Bounds::new(
                                point(hint_p.x - INLAY_CHIP_PAD, hint_p.y + px(1.)),
                                size(hint.width + INLAY_CHIP_PAD * 2., line_height - px(2.)),
                            ),
                            // **`secondary` では見えない** —— dopamine では
                            // それも編集面と同じ色に落ちている。地色に本文の
                            // 色を薄く混ぜると、どの配色でも 1 段浮く
                            // （`hint_background` と同じ作り）。
                            cx.theme()
                                .editor_background()
                                .blend(cx.theme().foreground.alpha(0.10)),
                        )
                        .corner_radii(px(3.)),
                    );
                }
                _ = hint.paint(hint_p, line_height, window, cx);
            }

            // The row's diagnostic, after the code and after the hints
            // (#256 / ADR-0089). Like the hints, these glyphs are nobody's
            // byte offsets -- the squiggle under the code is what marks the
            // place; this only says what it says.
            if let Some((_, diag)) = prepaint
                .inline_diagnostics
                .iter()
                .find(|(dix, _)| *dix == ix)
            {
                let last = line.wrapped_lines.last();
                let badge = prepaint
                    .fold_markers
                    .iter()
                    .find(|(mix, _)| *mix == ix)
                    .map_or(px(0.), |(_, m)| m.width + px(4.));
                let hint = prepaint
                    .inlay_hints
                    .iter()
                    .find(|(hix, _)| *hix == ix)
                    .map_or(px(0.), |(_, h)| h.width + INLAY_GAP);
                // At least `min_column`, so short rows line up instead of
                // each starting wherever their code happens to end.
                let least = p.x + column_advance * inline_diagnostic_min_column;
                let after_code = p.x
                    + last.map_or(px(0.), |l| l.width)
                    + badge
                    + hint
                    + column_advance * inline_diagnostic_padding;
                let diag_p = point(
                    after_code.max(least),
                    p.y + line_height * (line.wrapped_lines.len().saturating_sub(1)) as f32,
                );
                _ = diag.paint(diag_p, line_height, window, cx);
            }

            offset_y += line.size(line_height).height;

            // After the cursor row, paint ghost lines (which shifts subsequent content down)
            if has_ghost_lines && Some(row) == prepaint.current_row {
                let ghost_x = origin.x + prepaint.last_layout.line_number_width;

                for ghost_line in ghost_lines {
                    let ghost_p = point(ghost_x, origin.y + offset_y);

                    // Paint semi-transparent background for ghost line
                    let ghost_bounds = Bounds::new(
                        ghost_p,
                        size(
                            bounds.size.width - prepaint.last_layout.line_number_width,
                            line_height,
                        ),
                    );
                    window.paint_quad(fill(ghost_bounds, cx.theme().editor_background()));

                    // Paint ghost line text
                    _ = ghost_line.paint(ghost_p, line_height, window, cx);
                    offset_y += line_height;
                }
            }
        }

        // Paint whitespace marks on top of the glyphs they belong to.
        Self::paint_whitespaces(&prepaint.whitespaces, line_height, window, cx);

        // Paint blinking cursor
        //
        // `Blink` is already on/off by the time it gets here; the fades are
        // shaped from the phase, and only those need a frame scheduled.
        if focused && show_cursor {
            if let Some(target) = prepaint.cursor_bounds {
                // Where the caret is *drawn* -- part way along a slide, or the
                // target itself. Interpolated in text coordinates; the scroll
                // offset goes on after.
                let line_height = prepaint.last_layout.line_height;
                let (quad, sliding) = self.state.update(cx, |state, _| {
                    state.advance_caret_slide(target, line_height)
                });
                let cursor_bounds = PrepaintState::with_scroll(quad, prepaint.cursor_scroll_offset);

                let state = self.state.read(cx);
                let blinking = state.mode.cursor_blinking();
                let style = state.mode.cursor_style();
                let phase = state.blink_cursor.read(cx).phase();
                let (opacity, height) = caret::appearance(blinking, phase);
                let quad = caret::scale_height(cursor_bounds, height);
                let color = cx.theme().caret.opacity(opacity);

                if style.is_outline() {
                    // Leave the glyph readable: draw the box, not a fill.
                    let mut builder = gpui::PathBuilder::stroke(px(1.));
                    builder.move_to(quad.origin);
                    builder.line_to(point(quad.right(), quad.top()));
                    builder.line_to(point(quad.right(), quad.bottom()));
                    builder.line_to(point(quad.left(), quad.bottom()));
                    builder.line_to(quad.origin);
                    if let Ok(path) = builder.build() {
                        window.paint_path(path, color);
                    }
                } else if opacity > 0. {
                    window.paint_quad(fill(quad, color));
                }

                // A slide needs frames for the same reason the fades do.
                // Every caret move pauses the blink (`pause_blink_cursor`), so
                // the caret is solid while it travels without any extra work.
                if sliding || blinking.needs_animation() {
                    window.request_animation_frame();
                }
            }
        }

        // Paint line numbers
        let mut offset_y = px(0.);
        if let Some(line_numbers) = prepaint.line_numbers.as_ref() {
            offset_y += invisible_top_padding;

            window.paint_quad(fill(
                Bounds {
                    origin: input_bounds.origin,
                    size: size(
                        prepaint.last_layout.line_number_width - LINE_NUMBER_RIGHT_MARGIN,
                        input_bounds.size.height + prepaint.ghost_lines_height,
                    ),
                },
                cx.theme().editor_background(),
            ));

            // Each item is the normal lines.
            for (ix, lines) in line_numbers.iter().enumerate() {
                let row = visible_range.start + ix;

                let p = point(input_bounds.origin.x, origin.y + offset_y);
                let height = line_height * lines.len() as f32;
                // paint active line number background
                if line_highlight.highlights_gutter() && Some(row) == highlight_row {
                    if let Some(bg_color) = active_line_color {
                        window.paint_quad(fill(
                            Bounds::new(p, size(prepaint.last_layout.line_number_width, height)),
                            bg_color,
                        ));
                    }
                }

                if let Some(chevron) = prepaint.fold_chevrons.iter().find(|c| c.ix == ix) {
                    let x = fold_chevron_x(
                        input_bounds.origin.x,
                        prepaint.last_layout.line_number_width,
                    );
                    _ = chevron.line.paint(point(x, p.y), line_height, window, cx);
                }

                for line in lines {
                    _ = line.paint(p, line_height, window, cx);
                    offset_y += line_height;
                }

                // Add ghost line height after cursor row for line numbers alignment
                if !prepaint.ghost_lines.is_empty() && prepaint.current_row.is_some() {
                    offset_y += prepaint.ghost_lines_height;
                }
            }
        }

        self.state.update(cx, |state, cx| {
            state.last_layout = Some(prepaint.last_layout.clone());
            state.last_bounds = Some(bounds);
            state.last_cursor = Some(state.cursor());
            state.set_input_bounds(input_bounds, cx);
            state.last_selected_range = Some(selected_range);
            state.scroll_size = prepaint.scroll_size;
            state.update_scroll_offset(Some(prepaint.cursor_scroll_offset), cx);
            state.deferred_scroll_offset = None;

            cx.notify();
        });

        if let Some(hitbox) = prepaint.hover_definition_hitbox.as_ref() {
            window.set_cursor_style(gpui::CursorStyle::PointingHand, &hitbox);
        }

        if let Some(hitbox) = prepaint.link_hitbox.as_ref() {
            window.set_cursor_style(gpui::CursorStyle::PointingHand, hitbox);
        }

        if let Some(hitbox) = prepaint.fold_gutter_hitbox.as_ref() {
            window.set_cursor_style(gpui::CursorStyle::PointingHand, hitbox);
        }

        // Paint inline completion first line suffix (after cursor on same line)
        if focused {
            if let Some(first_line) = &prepaint.ghost_first_line {
                if let Some(cursor_bounds) = prepaint.cursor_bounds_with_scroll() {
                    // A block or underline caret is as wide as the glyph it
                    // covers; the ghost text still starts at the caret, so
                    // clamp to a bar's width here.
                    let first_line_x = cursor_bounds.origin.x
                        + cursor_bounds.size.width.min(caret::DEFAULT_CURSOR_WIDTH);
                    let p = point(first_line_x, cursor_bounds.origin.y);

                    // Paint background to cover any existing text
                    let bg_bounds = Bounds::new(p, size(first_line.width + px(4.), line_height));
                    window.paint_quad(fill(bg_bounds, cx.theme().editor_background()));

                    // Paint first line completion text
                    _ = first_line.paint(p, line_height, window, cx);
                }
            }
        }

        self.paint_mouse_listeners(window, cx);
    }
}

/// Get the runs for the given range.
///
/// The range is the byte range of the wrapped line.
pub(super) fn runs_for_range(
    runs: &[TextRun],
    line_offset: usize,
    range: &Range<usize>,
) -> Vec<TextRun> {
    let mut result = vec![];
    let range = (line_offset + range.start)..(line_offset + range.end);
    let mut cursor = 0;

    for run in runs {
        let run_start = cursor;
        let run_end = cursor + run.len;

        if run_end <= range.start {
            cursor = run_end;
            continue;
        }

        if run_start >= range.end {
            break;
        }

        let start = range.start.max(run_start) - run_start;
        let end = range.end.min(run_end) - run_start;
        let len = end - start;

        if len > 0 {
            result.push(TextRun { len, ..run.clone() });
        }

        cursor = run_end;
    }

    result
}

fn split_runs_by_bg_segments(
    start_offset: usize,
    runs: &[TextRun],
    bg_segments: &[(Range<usize>, Hsla)],
) -> Vec<TextRun> {
    let mut result = vec![];

    let mut cursor = start_offset;
    for run in runs {
        let mut run_start = cursor;
        let run_end = cursor + run.len;

        for (bg_range, bg_color) in bg_segments {
            if run_end <= bg_range.start || run_start >= bg_range.end {
                continue;
            }

            // Overlap exists
            if run_start < bg_range.start {
                // Add the part before the background range
                result.push(TextRun {
                    len: bg_range.start - run_start,
                    ..run.clone()
                });
            }

            // Add the overlapping part with background color
            let overlap_start = run_start.max(bg_range.start);
            let overlap_end = run_end.min(bg_range.end);
            let text_color = if bg_color.l >= 0.5 {
                gpui::black()
            } else {
                gpui::white()
            };

            let run_len = overlap_end.saturating_sub(overlap_start);
            if run_len > 0 {
                result.push(TextRun {
                    len: run_len,
                    color: text_color,
                    ..run.clone()
                });

                cursor = bg_range.end;
                run_start = cursor;
            }
        }

        if run_end > cursor {
            // Add the part after the background range
            result.push(TextRun {
                len: run_end - cursor,
                ..run.clone()
            });
        }

        cursor = run_end;
    }

    result
}

#[cfg(test)]
mod tests {
    /// The caret margin has to clear the scrollbar, or the caret at the end of
    /// a horizontally scrolled line is drawn underneath it (dopamine #324).
    ///
    /// The scrollbar is drawn after the text element, so there is no painting
    /// order to fix -- the caret simply must not be parked there.
    #[test]
    fn the_right_margin_clears_the_scrollbar() {
        assert_eq!(
            gpui::px(super::WIDTH_OF_SCROLLBAR),
            crate::scroll::Scrollbar::width()
        );
        assert!(
            super::RIGHT_MARGIN > crate::scroll::Scrollbar::width(),
            "the caret would sit under the scrollbar"
        );
    }

    use super::*;

    #[test]
    fn test_runs_for_range() {
        let run = TextRun {
            len: 0,
            font: gpui::font(".SystemUIFont"),
            color: gpui::black(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        // use hello this-is-test
        let runs = vec![
            // use
            TextRun {
                len: 3,
                ..run.clone()
            },
            // \s
            TextRun {
                len: 1,
                ..run.clone()
            },
            // hello
            TextRun {
                len: 5,
                ..run.clone()
            },
            // \s
            TextRun {
                len: 1,
                ..run.clone()
            },
            // this-is-test
            TextRun {
                len: 12,
                ..run.clone()
            },
        ];

        #[track_caller]
        fn assert_runs(actual: Vec<TextRun>, expected: &[usize]) {
            let left = actual.iter().map(|run| run.len).collect::<Vec<_>>();
            assert_eq!(left, expected);
        }

        assert_runs(runs_for_range(&runs, 0, &(0..0)), &[]);
        assert_runs(runs_for_range(&runs, 0, &(0..100)), &[3, 1, 5, 1, 12]);

        assert_runs(runs_for_range(&runs, 0, &(0..6)), &[3, 1, 2]);
        assert_runs(runs_for_range(&runs, 0, &(1..6)), &[2, 1, 2]);
        assert_runs(runs_for_range(&runs, 0, &(3..10)), &[1, 5, 1]);
        assert_runs(runs_for_range(&runs, 0, &(5..8)), &[3]);
        assert_runs(runs_for_range(&runs, 3, &(0..3)), &[1, 2]);
        assert_runs(runs_for_range(&runs, 3, &(2..10)), &[4, 1, 3]);
        assert_runs(runs_for_range(&runs, 9, &(0..8)), &[1, 7]);
    }

    #[test]
    fn test_split_runs_by_bg_segments() {
        let run = TextRun {
            len: 0,
            font: gpui::font(".SystemUIFont"),
            color: gpui::blue(),
            background_color: None,
            underline: None,
            strikethrough: None,
        };

        let runs = vec![
            TextRun {
                len: 5,
                ..run.clone()
            },
            TextRun {
                len: 7,
                ..run.clone()
            },
            TextRun {
                len: 24,
                ..run.clone()
            },
        ];

        let bg_segments = vec![(8..12, gpui::red()), (12..18, gpui::blue())];
        let result = split_runs_by_bg_segments(5, &runs, &bg_segments);
        assert_eq!(
            result.iter().map(|run| run.len).collect::<Vec<_>>(),
            vec![3, 2, 2, 5, 1, 23]
        );
        assert_eq!(result[0].color, gpui::blue());
        assert_eq!(result[1].color, gpui::black());
        assert_eq!(result[2].color, gpui::black());
        assert_eq!(result[3].color, gpui::black());
        assert_eq!(result[4].color, gpui::black());
        assert_eq!(result[5].color, gpui::blue());
    }
}

