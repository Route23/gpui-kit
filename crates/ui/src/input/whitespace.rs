//! Drawing whitespace as visible marks (VS Code's `editor.renderWhitespace`).
//!
//! The decision of *which* byte offsets get a mark is a pure function
//! ([`marks_for_line`]) so it can be unit tested; the pixel positions come from
//! the already-shaped lines, the same way [`super::indent`] draws indent guides.

use std::ops::Range;

use gpui::{Bounds, Pixels, Point, Window, point, px};

use crate::{
    ActiveTheme as _, RopeExt,
    input::{InputState, LastLayout, element::TextElement, mode::RenderWhitespace},
};

/// A drawn whitespace character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WsKind {
    /// A space: drawn as a small centered dot.
    Space,
    /// A tab: drawn as an arrow spanning the tab's own advance.
    Tab,
}

/// One mark to draw: the byte offset within the line, and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mark {
    pub(super) offset: usize,
    pub(super) kind: WsKind,
}

/// Which whitespace characters of `line` get a mark.
///
/// - `line` is one buffer row **without** its `\n` (a trailing `\r` is treated
///   as part of the line ending, not as whitespace to mark).
/// - `selection` is the selection's byte range **local to this line**, and is
///   only consulted by [`RenderWhitespace::Selection`].
pub(super) fn marks_for_line(
    line: &str,
    mode: RenderWhitespace,
    selection: Option<Range<usize>>,
) -> Vec<Mark> {
    if !mode.is_visible() {
        return vec![];
    }

    // `\r` belongs to the line ending. Marking it would put a dot past the end
    // of every line of a CRLF file.
    let end = line.strip_suffix('\r').map(str::len).unwrap_or(line.len());

    // Where the trailing whitespace run starts.
    let trailing_start = line[..end]
        .rfind(|c: char| c != ' ' && c != '\t')
        .map(|ix| ix + line[ix..].chars().next().map(char::len_utf8).unwrap_or(1))
        .unwrap_or(0);

    let bytes = line.as_bytes();
    let mut marks = vec![];
    for (offset, ch) in line[..end].char_indices() {
        let kind = match ch {
            ' ' => WsKind::Space,
            '\t' => WsKind::Tab,
            _ => continue,
        };

        let keep = match mode {
            RenderWhitespace::None => false,
            RenderWhitespace::All => true,
            RenderWhitespace::Trailing => offset >= trailing_start,
            RenderWhitespace::Selection => selection
                .as_ref()
                .is_some_and(|s| offset >= s.start && offset < s.end),
            // Everything except a *single* space between two words. Tabs are
            // always drawn; so are leading, trailing and repeated spaces.
            RenderWhitespace::Boundary => {
                kind == WsKind::Tab
                    || offset >= trailing_start
                    || offset == 0
                    || bytes.get(offset - 1) == Some(&b' ')
                    || bytes.get(offset + 1) == Some(&b' ')
            }
        };

        if keep {
            marks.push(Mark { offset, kind });
        }
    }

    marks
}

/// A mark placed in the pane, ready to paint.
pub(super) struct PlacedMark {
    pub(super) origin: Point<Pixels>,
    /// The advance of the character, used to size the tab arrow.
    pub(super) width: Pixels,
    pub(super) kind: WsKind,
}

impl TextElement {
    /// Place a whitespace mark on every character [`marks_for_line`] selects.
    ///
    /// Returns positions relative to the window, already offset by the gutter
    /// and by `bounds.origin` — the same contract as
    /// [`TextElement::layout_indent_guides`].
    pub(super) fn layout_whitespaces(
        &self,
        state: &InputState,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
    ) -> Vec<PlacedMark> {
        let mode = state.mode.render_whitespace();
        if !mode.is_visible() {
            return vec![];
        }

        let selection: Range<usize> = state.selected_range.into();
        if mode.follows_selection() && selection.is_empty() {
            return vec![];
        }

        let line_height = last_layout.line_height;
        let mut placed = vec![];
        let mut offset_y = last_layout.visible_top;
        // Byte offset of the first visible row, walked forward per row so the
        // selection can be cut down to a line-local range.
        let mut row_offset = last_layout.visible_range_offset.start;

        for row in last_layout.visible_range.clone() {
            let line = state.text.slice_line(row).to_string();
            let Some(line_layout) = last_layout.line(row) else {
                row_offset += line.len() + 1;
                continue;
            };

            let local_selection = mode.follows_selection().then(|| {
                selection.start.saturating_sub(row_offset)
                    ..selection.end.saturating_sub(row_offset).min(line.len())
            });

            for mark in marks_for_line(&line, mode, local_selection) {
                let Some(pos) = line_layout.position_for_index(mark.offset, line_height) else {
                    continue;
                };
                // A tab is shaped as a single glyph with whatever advance the
                // font gives it — there are no tab stops here, so the width has
                // to be measured rather than derived from `tab_size`.
                let next = line_layout
                    .position_for_index(mark.offset + 1, line_height)
                    .filter(|next| next.y == pos.y);
                let width = next.map(|next| next.x - pos.x).unwrap_or(px(0.));

                placed.push(PlacedMark {
                    origin: bounds.origin
                        + point(pos.x + last_layout.line_number_width, pos.y + offset_y),
                    width,
                    kind: mark.kind,
                });
            }

            offset_y += line_layout.wrapped_lines.len() as f32 * line_height;
            row_offset += line.len() + 1;
        }

        placed
    }

    /// Paint the marks collected by [`TextElement::layout_whitespaces`].
    pub(super) fn paint_whitespaces(
        marks: &[PlacedMark],
        line_height: Pixels,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        if marks.is_empty() {
            return;
        }

        let color = cx.theme().muted_foreground.opacity(0.4);
        let middle = line_height / 2.;

        for mark in marks {
            match mark.kind {
                WsKind::Space => {
                    let size = px(2.);
                    let x = mark.origin.x + (mark.width - size) / 2.;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(x, mark.origin.y + middle - size / 2.),
                            gpui::size(size, size),
                        ),
                        color,
                    ));
                }
                WsKind::Tab => {
                    // A horizontal bar with a short head, drawn as two quads so
                    // no glyph shaping is needed per tab.
                    let thickness = px(1.);
                    let inset = px(2.);
                    let len = (mark.width - inset * 2.).max(px(3.));
                    let y = mark.origin.y + middle;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(mark.origin.x + inset, y),
                            gpui::size(len, thickness),
                        ),
                        color,
                    ));
                    let head = px(3.).min(len);
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(mark.origin.x + inset + len - head, y - head + thickness),
                            gpui::size(thickness, head * 2. - thickness),
                        ),
                        color,
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mark, WsKind, marks_for_line};
    use crate::input::mode::RenderWhitespace;

    #[track_caller]
    fn offsets(line: &str, mode: RenderWhitespace) -> Vec<usize> {
        marks_for_line(line, mode, None)
            .into_iter()
            .map(|m| m.offset)
            .collect()
    }

    #[test]
    fn none_draws_nothing() {
        assert!(offsets("  a  b  ", RenderWhitespace::None).is_empty());
    }

    #[test]
    fn all_draws_every_space_and_tab() {
        assert_eq!(offsets("  a b", RenderWhitespace::All), vec![0, 1, 3]);
        assert_eq!(
            marks_for_line("\ta", RenderWhitespace::All, None),
            vec![Mark {
                offset: 0,
                kind: WsKind::Tab
            }]
        );
    }

    #[test]
    fn trailing_draws_only_the_run_after_the_last_word() {
        assert_eq!(offsets("  a b  ", RenderWhitespace::Trailing), vec![5, 6]);
        // A line that is nothing but whitespace is entirely trailing.
        assert_eq!(offsets("   ", RenderWhitespace::Trailing), vec![0, 1, 2]);
        assert!(offsets("ab", RenderWhitespace::Trailing).is_empty());
    }

    #[test]
    fn boundary_skips_a_single_space_between_words() {
        // "  a b  c" -> leading 2, the single space at 3 is skipped, the
        // trailing-ish double space at 5,6 is drawn.
        assert_eq!(offsets("  a b  c", RenderWhitespace::Boundary), vec![0, 1, 5, 6]);
        // Tabs are always drawn, even between words.
        assert_eq!(offsets("a\tb", RenderWhitespace::Boundary), vec![1]);
        // Trailing single space is drawn.
        assert_eq!(offsets("a b ", RenderWhitespace::Boundary), vec![3]);
    }

    #[test]
    fn selection_limits_the_marks_to_the_range() {
        let marks = marks_for_line("a b c d", RenderWhitespace::Selection, Some(1..4));
        assert_eq!(marks.iter().map(|m| m.offset).collect::<Vec<_>>(), vec![1, 3]);
        // No selection on this line -> nothing.
        assert!(marks_for_line("a b", RenderWhitespace::Selection, None).is_empty());
    }

    #[test]
    fn carriage_return_is_not_marked() {
        // `\r` belongs to the line ending; marking it would put a dot past the
        // end of every line of a CRLF file.
        assert_eq!(offsets("a \r", RenderWhitespace::All), vec![1]);
        assert_eq!(offsets("a \r", RenderWhitespace::Trailing), vec![1]);
    }
}
