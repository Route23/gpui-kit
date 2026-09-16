//! The headers pinned above the text while you scroll inside a block
//! (#252, VS Code's `editor.stickyScroll`, Zed's `sticky_scroll`).
//!
//! **Which rows** is decided here, from indentation alone, so it can be
//! tested without an editor; **where they are drawn** is `element.rs`, in the
//! same layer the inline-completion ghost lines use.
//!
//! The syntax tree is not consulted. The same indentation walk that decides
//! what folds (`super::fold`) decides what is an ancestor, and a language
//! server's `foldingRange` can stand in for it when one has answered.

use std::ops::Range;

/// One indented line, as this module needs to see it.
///
/// Taking a slice of `(row, indent)` instead of the rope keeps the walk
/// testable and keeps the tab arithmetic in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Indented {
    pub row: usize,
    /// Indent in columns, or `None` for a blank line.
    pub indent: Option<usize>,
}

/// The rows whose block encloses `top`, outermost first.
///
/// Walks **up** from `top`, keeping a row every time the indent gets
/// shallower: that row is the head of a block `top` is inside. Blank lines
/// are skipped -- they belong to no block. At most `max` rows come back, the
/// innermost ones dropped first, because the outermost header is the one that
/// says where you are.
pub(super) fn ancestors_by_indent(lines: &[Indented], top: usize, max: usize) -> Vec<usize> {
    if max == 0 || top == 0 || top >= lines.len() {
        return vec![];
    }
    let Some(mut depth) = lines[top].indent else {
        return vec![];
    };
    let mut out = vec![];
    for line in lines[..top].iter().rev() {
        let Some(indent) = line.indent else { continue };
        if indent < depth {
            out.push(line.row);
            depth = indent;
            if depth == 0 {
                break;
            }
        }
    }
    out.reverse();
    if out.len() > max {
        out.truncate(max);
    }
    out
}

/// The rows whose supplied fold range covers `top`, outermost first.
///
/// Used when a language server has answered `textDocument/foldingRange`:
/// **its idea of a block beats indentation**, because it knows that a
/// multi-line argument list is not a scope.
pub(super) fn ancestors_by_ranges(
    ranges: &[(usize, Range<usize>)],
    top: usize,
    max: usize,
) -> Vec<usize> {
    if max == 0 {
        return vec![];
    }
    let mut rows: Vec<usize> = ranges
        .iter()
        .filter(|(head, range)| *head < top && range.contains(&top))
        .map(|(head, _)| *head)
        .collect();
    rows.sort_unstable();
    rows.dedup();
    if rows.len() > max {
        rows.truncate(max);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(indents: &[Option<usize>]) -> Vec<Indented> {
        indents
            .iter()
            .enumerate()
            .map(|(row, indent)| Indented { row, indent: *indent })
            .collect()
    }

    /// ```text
    /// 0  fn main() {
    /// 1      if a {
    /// 2          for b {
    /// 3              body      <- here
    /// ```
    #[test]
    fn the_headers_are_the_rows_that_step_out() {
        let l = lines(&[Some(0), Some(4), Some(8), Some(12)]);
        assert_eq!(ancestors_by_indent(&l, 3, 10), vec![0, 1, 2]);
    }

    /// A blank line belongs to no block, and must not end the walk.
    #[test]
    fn blank_lines_are_stepped_over() {
        let l = lines(&[Some(0), None, Some(4), Some(8)]);
        assert_eq!(ancestors_by_indent(&l, 3, 10), vec![0, 2]);
    }

    /// The outermost header is the one that says where you are.
    #[test]
    fn the_innermost_headers_are_dropped_first() {
        let l = lines(&[Some(0), Some(4), Some(8), Some(12)]);
        assert_eq!(ancestors_by_indent(&l, 3, 2), vec![0, 1]);
    }

    #[test]
    fn nothing_encloses_the_first_row_or_a_blank_one() {
        let l = lines(&[Some(0), Some(4)]);
        assert_eq!(ancestors_by_indent(&l, 0, 10), Vec::<usize>::new());
        let b = lines(&[Some(0), None]);
        assert_eq!(ancestors_by_indent(&b, 1, 10), Vec::<usize>::new());
        assert_eq!(ancestors_by_indent(&l, 1, 0), Vec::<usize>::new());
    }

    /// A row at the same depth is a sibling, not a parent.
    #[test]
    fn a_sibling_is_not_a_header() {
        let l = lines(&[Some(0), Some(4), Some(4), Some(4)]);
        assert_eq!(ancestors_by_indent(&l, 3, 10), vec![0]);
    }

    #[test]
    fn supplied_ranges_win_when_there_are_any() {
        let ranges = vec![(0usize, 1..20usize), (5, 6..9), (30, 31..40)];
        assert_eq!(ancestors_by_ranges(&ranges, 7, 10), vec![0, 5]);
        assert_eq!(ancestors_by_ranges(&ranges, 7, 1), vec![0]);
        assert_eq!(ancestors_by_ranges(&ranges, 25, 10), Vec::<usize>::new());
    }
}

use gpui::{
    App, Bounds, Hsla, Pixels, Point, ShapedLine, TextRun, Window, fill, point, px, size,
};

use crate::{ActiveTheme as _, RopeExt as _};

/// Where the pinned headers go.
#[derive(Clone, Copy)]
pub(super) struct StickyGeometry {
    /// **The viewport's top-left, not the element's.** `bounds.origin` in
    /// `paint` has already been moved by the scroll offset -- anchoring to it
    /// puts the headers as far above the screen as the reader has scrolled.
    pub origin: Point<Pixels>,
    pub width: Pixels,
    pub line_number_width: Pixels,
    pub line_height: Pixels,
    pub scroll_x: Pixels,
    pub follow_scroll: bool,
}

/// A header shaped and ready to paint.
pub(super) struct StickyLine {
    /// The row this header is, so its own number goes in the gutter.
    pub row: usize,
    pub line: ShapedLine,
}

impl super::InputState {
    /// The rows to pin above the text, outermost first (#252).
    pub(super) fn sticky_rows(&self, top: usize) -> Vec<usize> {
        let (on, max, model, _) = self.mode.sticky_scroll();
        if !on || max == 0 {
            return vec![];
        }
        // A language server's ranges beat indentation when there are any:
        // it knows a multi-line argument list is not a scope.
        if model == super::mode::StickyModel::Folding {
            if let Some(folds) = self.supplied_folds.as_ref() {
                let ranges: Vec<(usize, Range<usize>)> = folds
                    .iter()
                    .map(|f| (f.start_row, f.start_row + 1..f.end_row + 1))
                    .collect();
                if !ranges.is_empty() {
                    return ancestors_by_ranges(&ranges, top, max);
                }
            }
        }
        // Only the rows above `top` matter, and only their indent.
        let tab = self.mode.tab_size();
        let lines: Vec<Indented> = (0..=top.min(self.text.lines_len().saturating_sub(1)))
            .map(|row| {
                let line = self.text.slice_line(row);
                let blank = line.chars().all(char::is_whitespace);
                Indented {
                    row,
                    indent: (!blank).then(|| tab.indent_count(&line)),
                }
            })
            .collect();
        ancestors_by_indent(&lines, top, max)
    }
}

impl super::element::TextElement {
    /// Shape the pinned headers.
    pub(super) fn layout_sticky(
        &self,
        state: &super::InputState,
        last_layout: &super::state::LastLayout,
        text_style: &gpui::TextStyle,
        window: &mut Window,
    ) -> Vec<StickyLine> {
        let rows = state.sticky_rows(last_layout.visible_range.start);
        if rows.is_empty() {
            return vec![];
        }
        let font_size = text_style.font_size.to_pixels(window.rem_size());
        rows.into_iter()
            .map(|row| {
                let text: gpui::SharedString = state
                    .text
                    .slice_line(row)
                    .to_string()
                    .trim_end()
                    .to_string()
                    .into();
                let run = TextRun {
                    len: text.len(),
                    font: text_style.font(),
                    color: text_style.color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let line = window
                    .text_system()
                    .shape_line(text, font_size, &[run], None);
                StickyLine { row, line }
            })
            .collect()
    }

    /// Paint the pinned headers over the text.
    pub(super) fn paint_sticky(
        lines: &[StickyLine],
        geom: StickyGeometry,
        window: &mut Window,
        cx: &mut App,
    ) {
        let StickyGeometry {
            origin,
            width,
            line_number_width,
            line_height,
            scroll_x,
            follow_scroll,
        } = geom;
        if lines.is_empty() {
            return;
        }
        // **Two fills.** The editor's own background first, because the band
        // colour is translucent and the rows underneath -- line numbers
        // included -- would otherwise read straight through the header.
        let under: Hsla = cx.theme().background;
        let bg: Hsla = cx.theme().secondary;
        let number_color = cx.theme().muted_foreground;
        let font_size = window.text_style().font_size.to_pixels(window.rem_size());
        let font = window.text_style().font();
        let mut y = origin.y;
        for sticky in lines {
            let band = Bounds::new(point(origin.x, y), size(width, line_height));
            window.paint_quad(fill(band, under));
            window.paint_quad(fill(band, bg));
            // The header's **own** number, not the one it is covering.
            if line_number_width > px(0.) {
                let text: gpui::SharedString = (sticky.row + 1).to_string().into();
                let run = TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: number_color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let shaped = window
                    .text_system()
                    .shape_line(text, font_size, &[run], None);
                // Right-aligned in the gutter, the way the real numbers are.
                let nx = origin.x + line_number_width - shaped.width - px(8.);
                let _ = shaped.paint(point(nx.max(origin.x), y), line_height, window, cx);
            }
            let x = origin.x + line_number_width + if follow_scroll { scroll_x } else { px(0.) };
            let _ = sticky.line.paint(point(x, y), line_height, window, cx);
            y += line_height;
        }
        // A hairline under the stack, so the pinned rows do not read as part
        // of the text they are covering.
        window.paint_quad(fill(
            Bounds::new(point(origin.x, y), size(width, px(1.))),
            cx.theme().border,
        ));
    }
}
