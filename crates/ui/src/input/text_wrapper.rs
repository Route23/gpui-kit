use std::ops::Range;

use gpui::{App, Font, LineFragment, Pixels, Point, ShapedLine, Size, Window, point, px, size};
use ropey::Rope;
use smallvec::SmallVec;

use crate::input::RopeExt;

/// A line with soft wrapped lines info.
#[derive(Debug, Clone)]
pub(super) struct LineItem {
    /// The original line text, without end `\n`.
    line: Rope,
    /// The soft wrapped lines relative byte range (0..line.len) of this line (Include first line).
    ///
    /// Not contains the line end `\n`.
    pub(super) wrapped_lines: Vec<Range<usize>>,
    /// Hidden by a fold.
    ///
    /// The wrap info is kept, so unfolding needs no re-wrap (which would need
    /// the text system, and therefore a `Window`).
    pub(super) hidden: bool,
    /// Rows inserted **below** this one that are not text: the ghost lines of a
    /// multi-line inline completion today, code lenses later.
    ///
    /// Kept on the row rather than added by whoever paints them, so that every
    /// y-walk which already asks a row for its height follows along -- the hit
    /// tests included. Painting them is still the caller's business.
    pub(super) extra_rows: usize,
}

impl LineItem {
    /// Get the bytes length of this line.
    #[inline]
    pub(super) fn len(&self) -> usize {
        self.line.len()
    }

    /// Get number of soft wrapped lines of this line (include the first line).
    ///
    /// A row hidden by a fold occupies **no** visual lines, which is what makes
    /// every height-accumulating loop skip it for free.
    #[inline]
    pub(super) fn lines_len(&self) -> usize {
        if self.hidden {
            return 0;
        }
        self.wrapped_lines.len()
    }

    /// Rows inserted below this one, as seen on screen.
    ///
    /// A folded row shows nothing, so what was inserted under it takes no room
    /// either -- same rule as [`LineItem::lines_len`].
    #[inline]
    pub(super) fn extra_rows(&self) -> usize {
        if self.hidden {
            return 0;
        }
        self.extra_rows
    }

    /// Get the height of this line item with given line height.
    ///
    /// **Text rows plus anything inserted under them.** Every y-walk that goes
    /// through here -- the visible range, the caret hit test, the fold gutter
    /// hit test -- keeps up with an insertion for free.
    pub(super) fn height(&self, line_height: Pixels) -> Pixels {
        (self.lines_len() + self.extra_rows()) as f32 * line_height
    }
}

#[derive(Debug, Default)]
pub(super) struct LongestRow {
    /// The 0-based row index.
    pub row: usize,
    /// The bytes length of the longest line.
    pub len: usize,
}

/// Used to prepare the text with soft wrap to be get lines to displayed in the Editor.
///
/// After use lines to calculate the scroll size of the Editor.
pub(super) struct TextWrapper {
    text: Rope,
    /// Total wrapped lines (Inlucde the first line), value is start and end index of the line.
    soft_lines: usize,
    font: Font,
    font_size: Pixels,
    /// If is none, it means the text is not wrapped
    wrap_width: Option<Pixels>,
    /// The longest (row, bytes len) in characters, used to calculate the horizontal scroll width.
    pub(super) longest_row: LongestRow,
    /// The lines by split \n
    pub(super) lines: Vec<LineItem>,
    /// Rows hidden by folding.
    ///
    /// Kept here (rather than only as flags on the lines) because `_update`
    /// splices `lines` and `update_all` rebuilds it outright -- `set_wrap_width`
    /// and `set_font` both go through the latter, and would otherwise drop
    /// every flag mid-frame.
    hidden_rows: Vec<usize>,
    /// Rows inserted below a row, as `(row, rows)`.
    ///
    /// Kept here for the same reason as `hidden_rows`: `_update` splices
    /// `lines` and `update_all` rebuilds it outright, so flags living only on
    /// the lines would be dropped mid-frame.
    extra_rows: Vec<(usize, usize)>,

    _initialized: bool,
}

#[allow(unused)]
impl TextWrapper {
    pub(super) fn new(font: Font, font_size: Pixels, wrap_width: Option<Pixels>) -> Self {
        Self {
            text: Rope::new(),
            font,
            font_size,
            wrap_width,
            soft_lines: 0,
            longest_row: LongestRow::default(),
            lines: Vec::new(),
            hidden_rows: Vec::new(),
            extra_rows: Vec::new(),
            _initialized: false,
        }
    }

    #[inline]
    pub(super) fn set_default_text(&mut self, text: &Rope) {
        self.text = text.clone();
    }

    /// Get the total number of lines including wrapped lines.
    #[inline]
    pub(super) fn len(&self) -> usize {
        self.soft_lines
    }

    /// Get the line item by row index.
    #[inline]
    pub(super) fn line(&self, row: usize) -> Option<&LineItem> {
        self.lines.iter().skip(row).next()
    }

    /// Replace the set of rows hidden by folding.
    ///
    /// Clear-then-set, never incremental: `_update` splices `lines` and keeps
    /// the flags of the rows it did not touch, so a row shift would otherwise
    /// leave stale flags behind.
    pub(super) fn set_hidden_rows(&mut self, rows: Vec<usize>) {
        self.hidden_rows = rows;
        self.apply_hidden_rows();
    }

    /// Replace the set of rows inserted below a row, as `(row, rows)`.
    ///
    /// Clear-then-set like [`TextWrapper::set_hidden_rows`], and for the same
    /// reason. Passing an empty list is how an insertion goes away.
    pub(super) fn set_extra_rows(&mut self, rows: Vec<(usize, usize)>) {
        if self.extra_rows == rows {
            return;
        }
        self.extra_rows = rows;
        self.apply_extra_rows();
    }

    fn apply_extra_rows(&mut self) {
        for line in self.lines.iter_mut() {
            line.extra_rows = 0;
        }
        for &(row, rows) in &self.extra_rows {
            if let Some(line) = self.lines.get_mut(row) {
                line.extra_rows = rows;
            }
        }
    }

    /// The height of every row, insertions included.
    ///
    /// **The one truth for the scroll size** -- whoever paints an insertion
    /// must not add its height a second time.
    pub(super) fn total_height(&self, line_height: Pixels) -> Pixels {
        self.lines
            .iter()
            .map(|line| line.height(line_height))
            .fold(px(0.), |acc, h| acc + h)
    }

    fn apply_hidden_rows(&mut self) {
        for line in self.lines.iter_mut() {
            line.hidden = false;
        }
        for &row in &self.hidden_rows {
            if let Some(line) = self.lines.get_mut(row) {
                line.hidden = true;
            }
        }
        self.soft_lines = self.lines.iter().map(|l| l.lines_len()).sum();
    }

    pub(super) fn set_wrap_width(&mut self, wrap_width: Option<Pixels>, cx: &mut App) {
        if wrap_width == self.wrap_width {
            return;
        }

        self.wrap_width = wrap_width;
        self.update_all(&self.text.clone(), cx);
    }

    pub(super) fn set_font(&mut self, font: Font, font_size: Pixels, cx: &mut App) {
        if self.font.eq(&font) && self.font_size == font_size {
            return;
        }

        self.font = font;
        self.font_size = font_size;
        self.update_all(&self.text.clone(), cx);
    }

    pub(super) fn prepare_if_need(&mut self, text: &Rope, cx: &mut App) {
        if self._initialized {
            return;
        }
        self._initialized = true;
        self.update_all(text, cx);
    }

    /// Update the text wrapper and recalculate the wrapped lines.
    ///
    /// If the `text` is the same as the current text, do nothing.
    ///
    /// - `changed_text`: The text [`Rope`] that has changed.
    /// - `range`: The `selected_range` before change.
    /// - `new_text`: The inserted text.
    /// - `force`: Whether to force the update, if false, the update will be skipped if the text is the same.
    /// - `cx`: The application context.
    pub(super) fn update(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        cx: &mut App,
    ) {
        let mut line_wrapper = cx
            .text_system()
            .line_wrapper(self.font.clone(), self.font_size);
        self._update(
            changed_text,
            range,
            new_text,
            &mut |line_str, wrap_width| {
                line_wrapper
                    .wrap_line(&[LineFragment::text(line_str)], wrap_width)
                    .collect()
            },
        );
    }

    fn _update<F>(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_text: &Rope,
        wrap_line: &mut F,
    ) where
        F: FnMut(&str, Pixels) -> Vec<gpui::Boundary>,
    {
        // Remove the old changed lines.
        let start_row = self.text.offset_to_point(range.start).row;
        let start_row = start_row.min(self.lines.len().saturating_sub(1));
        let end_row = self.text.offset_to_point(range.end).row;
        let end_row = end_row.min(self.lines.len().saturating_sub(1));
        let rows_range = start_row..=end_row;

        if rows_range.contains(&self.longest_row.row) {
            self.longest_row = LongestRow::default();
        }

        let mut longest_row_ix = self.longest_row.row;
        let mut longest_row_len = self.longest_row.len;

        // To add the new lines.
        let new_start_row = changed_text.offset_to_point(range.start).row;
        let new_start_offset = changed_text.line_start_offset(new_start_row);
        let new_end_row = changed_text
            .offset_to_point(range.start + new_text.len())
            .row;
        let new_end_offset = changed_text.line_end_offset(new_end_row);
        let new_range = new_start_offset..new_end_offset;

        let mut new_lines = vec![];
        let wrap_width = self.wrap_width;

        // line not contains `\n`.
        for (ix, line) in Rope::from(changed_text.slice(new_range))
            .iter_lines()
            .enumerate()
        {
            let line_str = line.to_string();
            let mut wrapped_lines = vec![];
            let mut prev_boundary_ix = 0;

            if line_str.len() > longest_row_len {
                longest_row_ix = new_start_row + ix;
                longest_row_len = line_str.len();
            }

            // If wrap_width is Pixels::MAX, skip wrapping to disable word wrap
            if let Some(wrap_width) = wrap_width {
                // Here only have wrapped line, if there is no wrap meet, the `line_wraps` result will empty.
                for boundary in wrap_line(&line_str, wrap_width) {
                    wrapped_lines.push(prev_boundary_ix..boundary.ix);
                    prev_boundary_ix = boundary.ix;
                }
            }

            // Reset of the line
            if !line_str[prev_boundary_ix..].is_empty() || prev_boundary_ix == 0 {
                wrapped_lines.push(prev_boundary_ix..line.len());
            }

            new_lines.push(LineItem {
                line: Rope::from(line),
                wrapped_lines,
                hidden: false,
                extra_rows: 0,
            });
        }

        if self.lines.len() == 0 {
            self.lines = new_lines;
        } else {
            self.lines.splice(rows_range, new_lines);
        }

        self.text = changed_text.clone();
        self.apply_hidden_rows();
        self.apply_extra_rows();
        self.longest_row = LongestRow {
            row: longest_row_ix,
            len: longest_row_len,
        }
    }

    /// Update the text wrapper and recalculate the wrapped lines.
    ///
    /// If the `text` is the same as the current text, do nothing.
    fn update_all(&mut self, text: &Rope, cx: &mut App) {
        self.update(text, &(0..text.len()), &text, cx);
    }

    /// Return display point (with soft wrap) from the given byte offset in the text.
    ///
    /// Panics if the `offset` is out of bounds.
    pub(crate) fn offset_to_display_point(&self, offset: usize) -> DisplayPoint {
        let row = self.text.offset_to_point(offset).row;
        let start = self.text.line_start_offset(row);
        let line = &self.lines[row];

        let mut wrapped_row = self
            .lines
            .iter()
            .take(row)
            .map(|l| l.lines_len())
            .sum::<usize>();

        let local_offset = offset.saturating_sub(start);
        for (ix, range) in line.wrapped_lines.iter().enumerate() {
            if range.contains(&local_offset) {
                return DisplayPoint::new(
                    wrapped_row + ix,
                    ix,
                    local_offset.saturating_sub(range.start),
                );
            }
        }

        // Otherwise return the eof of the line.
        let last_range = line.wrapped_lines.last().unwrap_or(&(0..0));
        let ix = line.lines_len().saturating_sub(1);
        return DisplayPoint::new(wrapped_row + ix, ix, last_range.len());
    }

    /// Return byte offset in the text from the given display point (with soft wrap).
    ///
    /// Panics if the `point.row` is out of bounds.
    pub(crate) fn display_point_to_offset(&self, point: DisplayPoint) -> usize {
        let mut wrapped_row = 0;
        for (row, line) in self.lines.iter().enumerate() {
            if wrapped_row + line.lines_len() > point.row {
                let line_start = self.text.line_start_offset(row);
                let local_row = point.row.saturating_sub(wrapped_row);
                if let Some(range) = line.wrapped_lines.get(local_row) {
                    return line_start + (range.start + point.column).min(range.end);
                } else {
                    // If not found, return the end of the line.
                    return line_start + line.len();
                }
            }

            wrapped_row += line.lines_len();
        }

        return self.text.len();
    }

    pub(crate) fn display_point_to_point(&self, point: DisplayPoint) -> tree_sitter::Point {
        let offset = self.display_point_to_offset(point);
        self.text.offset_to_point(offset)
    }

    pub(crate) fn point_to_display_point(&self, point: tree_sitter::Point) -> DisplayPoint {
        let offset = self.text.point_to_offset(point);
        self.offset_to_display_point(offset)
    }
}

/// The actually display point in the text.
///
/// This is usually used to describe the
/// position in the text with `soft-wrap` mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayPoint {
    /// The 0-based soft wrapped row index in the text.
    pub row: usize,
    /// The 0-based row index in local line (include first line).
    ///
    /// This value only valid when return from [`TextWrapper::offset_to_display_point`], otherwise it will be ignored.
    pub local_row: usize,
    /// The 0-based column byte index in the display line (with soft wrap).
    pub column: usize,
}

impl DisplayPoint {
    pub fn new(row: usize, local_row: usize, column: usize) -> Self {
        Self {
            row,
            local_row,
            column,
        }
    }
}

/// The layout info of a line with soft wrapped lines.
pub(crate) struct LineLayout {
    /// Total bytes length of this line.
    len: usize,
    /// The soft wrapped lines of this line (Include the first line).
    pub(crate) wrapped_lines: SmallVec<[ShapedLine; 1]>,
    pub(crate) longest_width: Pixels,
}

impl LineLayout {
    /// A layout for a row hidden by a fold: no visual lines (so it takes no
    /// height and reports no positions), but an **honest byte length**.
    ///
    /// The length matters more than it looks: every offset-walking loop steps
    /// through the visible rows with `prev_lines_offset += line.len() + 1`, so a
    /// folded row reporting 0 would put a gap in the byte stream and misplace
    /// every click, selection and IME rect below the fold.
    pub(crate) fn folded(len: usize) -> Self {
        Self {
            len,
            longest_width: px(0.),
            wrapped_lines: SmallVec::new(),
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            len: 0,
            longest_width: px(0.),
            wrapped_lines: SmallVec::new(),
        }
    }

    pub(crate) fn lines(mut self, wrapped_lines: SmallVec<[ShapedLine; 1]>) -> Self {
        self.set_wrapped_lines(wrapped_lines);
        self
    }

    pub(crate) fn set_wrapped_lines(&mut self, wrapped_lines: SmallVec<[ShapedLine; 1]>) {
        self.len = wrapped_lines.iter().map(|l| l.len).sum();
        let width = wrapped_lines
            .iter()
            .map(|l| l.width)
            .max()
            .unwrap_or_default();
        self.longest_width = width;
        self.wrapped_lines = wrapped_lines;
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// Get the position (x, y) for the given index in this line layout.
    ///
    /// - The `offset` is a local byte index in this line layout.
    /// - The return value is relative to the top-left corner of this line layout, start from (0, 0)
    pub(crate) fn position_for_index(
        &self,
        offset: usize,
        line_height: Pixels,
    ) -> Option<Point<Pixels>> {
        let mut acc_len = 0;
        let mut offset_y = px(0.);

        for (i, line) in self.wrapped_lines.iter().enumerate() {
            let is_last = i + 1 == self.wrapped_lines.len();
            let line_len = if is_last { line.len + 1 } else { line.len };

            let range = acc_len..(acc_len + line_len);
            if range.contains(&offset) {
                let x = line.x_for_index(offset.saturating_sub(acc_len));
                return Some(point(x, offset_y));
            }
            acc_len += line_len;
            offset_y += line_height;
        }

        None
    }

    /// Get the closest index for the given x in this line layout.
    pub(super) fn closest_index_for_x(&self, x: Pixels) -> usize {
        let mut acc_len = 0;
        for (i, line) in self.wrapped_lines.iter().enumerate() {
            let is_last = i + 1 == self.wrapped_lines.len();
            if x <= line.width {
                let mut ix = line.closest_index_for_x(x);
                if !is_last && ix == line.text.len() {
                    // For soft wrap line, we can't put the cursor at the end of the line.
                    let c_len = line.text.chars().last().map(|c| c.len_utf8()).unwrap_or(0);
                    ix = ix.saturating_sub(c_len);
                }

                return acc_len + ix;
            }
            acc_len += line.text.len();
        }

        acc_len
    }

    /// Get the index for the given position (x, y) in this line layout.
    ///
    /// The `pos` is relative to the top-left corner of this line layout, start from (0, 0)
    /// The return value is a local byte index in this line layout, start from 0.
    pub(super) fn closest_index_for_position(
        &self,
        pos: Point<Pixels>,
        line_height: Pixels,
    ) -> Option<usize> {
        let mut offset = 0;
        let mut line_top = px(0.);
        for (i, line) in self.wrapped_lines.iter().enumerate() {
            let is_last = i + 1 == self.wrapped_lines.len();
            let line_bottom = line_top + line_height;
            if pos.y >= line_top && pos.y < line_bottom {
                let mut ix = line.closest_index_for_x(pos.x);
                if !is_last && ix == line.text.len() {
                    // For soft wrap line, we can't put the cursor at the end of the line.
                    let c_len = line.text.chars().last().map(|c| c.len_utf8()).unwrap_or(0);
                    ix = ix.saturating_sub(c_len);
                }
                return Some(offset + ix);
            }

            offset += line.text.len();
            line_top = line_bottom;
        }

        None
    }

    pub(super) fn index_for_position(
        &self,
        pos: Point<Pixels>,
        line_height: Pixels,
    ) -> Option<usize> {
        let mut offset = 0;
        let mut line_top = px(0.);
        for line in self.wrapped_lines.iter() {
            let line_bottom = line_top + line_height;
            if pos.y >= line_top && pos.y < line_bottom {
                let ix = line.index_for_x(pos.x)?;
                return Some(offset + ix);
            }

            offset += line.text.len();
            line_top = line_bottom;
        }

        None
    }

    pub(super) fn size(&self, line_height: Pixels) -> Size<Pixels> {
        size(self.longest_width, self.wrapped_lines.len() * line_height)
    }

    pub(super) fn paint(
        &self,
        pos: Point<Pixels>,
        line_height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        for (ix, line) in self.wrapped_lines.iter().enumerate() {
            _ = line.paint(
                pos + point(px(0.), ix * line_height),
                line_height,
                window,
                cx,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Boundary, FontFeatures, FontStyle, FontWeight, px};

    fn test_font() -> gpui::Font {
        gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        }
    }

    fn no_wrap(_line: &str, _wrap_width: Pixels) -> Vec<Boundary> {
        vec![]
    }

    /// Four one-line rows, no soft wrap.
    fn four_rows() -> (Rope, TextWrapper) {
        let text = Rope::from("one\ntwo\nthree\nfour");
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        wrapper._update(&text, &(0..text.len()), &text, &mut no_wrap);
        (text, wrapper)
    }

    /// A row carries what was inserted under it, and the total follows.
    #[test]
    fn inserted_rows_add_to_the_height() {
        let (_text, mut wrapper) = four_rows();
        let h = px(20.);
        assert_eq!(wrapper.total_height(h), px(80.));

        wrapper.set_extra_rows(vec![(1, 2)]);
        assert_eq!(wrapper.lines[1].height(h), px(60.));
        // Only that row: the others are untouched.
        assert_eq!(wrapper.lines[0].height(h), px(20.));
        assert_eq!(wrapper.total_height(h), px(120.));

        // An empty list is how an insertion goes away.
        wrapper.set_extra_rows(vec![]);
        assert_eq!(wrapper.total_height(h), px(80.));
    }

    /// Nothing shows under a folded row, so nothing takes room there either.
    #[test]
    fn a_folded_row_hides_what_was_inserted_under_it() {
        let (_text, mut wrapper) = four_rows();
        let h = px(20.);
        wrapper.set_extra_rows(vec![(1, 2)]);
        wrapper.set_hidden_rows(vec![1]);

        assert_eq!(wrapper.lines[1].extra_rows(), 0);
        assert_eq!(wrapper.lines[1].height(h), px(0.));
        assert_eq!(wrapper.total_height(h), px(60.));

        // Unfolding brings both the row and its insertion back.
        wrapper.set_hidden_rows(vec![]);
        assert_eq!(wrapper.lines[1].height(h), px(60.));
    }

    /// `_update` splices `lines`; the flags have to be put back afterwards.
    #[test]
    fn inserted_rows_survive_a_rewrap() {
        let (mut text, mut wrapper) = four_rows();
        let h = px(20.);
        wrapper.set_extra_rows(vec![(1, 3)]);

        let range = text.len()..text.len();
        text.replace(range.clone(), "\nfive");
        wrapper._update(&text, &range, &Rope::from("\nfive"), &mut no_wrap);

        assert_eq!(wrapper.lines.len(), 5);
        assert_eq!(wrapper.lines[1].extra_rows(), 3);
        assert_eq!(wrapper.total_height(h), px(160.));
    }

    /// A row the text no longer has is dropped, not carried on the next one.
    #[test]
    fn an_insertion_past_the_end_is_dropped() {
        let (_text, mut wrapper) = four_rows();
        wrapper.set_extra_rows(vec![(9, 2)]);
        assert_eq!(wrapper.total_height(px(20.)), px(80.));
    }

    #[test]
    fn test_update() {
        let font = gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        };

        let mut wrapper = TextWrapper::new(font, px(14.), None);
        let mut text = Rope::from(
            "Hello, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。",
        );

        fn fake_wrap_line(_line: &str, _wrap_width: Pixels) -> Vec<Boundary> {
            vec![]
        }

        #[track_caller]
        fn assert_wrapper_lines(text: &Rope, wrapper: &TextWrapper, expected_lines: &[&[&str]]) {
            let mut actual_lines = vec![];
            let mut offset = 0;
            for line in wrapper.lines.iter() {
                actual_lines.push(
                    line.wrapped_lines
                        .iter()
                        .map(|range| text.slice(offset + range.start..offset + range.end))
                        .collect::<Vec<_>>(),
                );
                // +1 \n
                offset += line.len() + 1;
            }
            assert_eq!(actual_lines, expected_lines);
        }

        wrapper._update(&text, &(0..text.len()), &text, &mut fake_wrap_line);
        assert_eq!(wrapper.lines.len(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["Hello, 世界!\r"],
                &["This is second line."],
                &["This is third line."],
                &["这里是第 4 行。"],
            ],
        );

        // Add a new text to end
        let range = text.len()..text.len();
        let new_text = "New text";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "Hello, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。New text"
        );
        assert_eq!(wrapper.lines.len(), 4);
        assert_eq!(wrapper.lines.len(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["Hello, 世界!\r"],
                &["This is second line."],
                &["This is third line."],
                &["这里是第 4 行。New text"],
            ],
        );

        // Replace first line `Hello` to `AAA`
        let range = 0..5;
        let new_text = "AAA";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "AAA, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。New text"
        );
        assert_eq!(wrapper.lines.len(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["AAA, 世界!\r"],
                &["This is second line."],
                &["This is third line."],
                &["这里是第 4 行。New text"],
            ],
        );

        // Remove the second line
        let start_offset = text.line_start_offset(1);
        let end_offset = text.line_end_offset(1);
        let range = start_offset..end_offset + 1;
        text.replace(range.clone(), "");
        wrapper._update(&text, &range, &Rope::from(""), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "AAA, 世界!\r\nThis is third line.\n这里是第 4 行。New text"
        );
        assert_eq!(wrapper.lines.len(), 3);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["AAA, 世界!\r"],
                &["This is third line."],
                &["这里是第 4 行。New text"],
            ],
        );

        // Replace the first 2 lines to "This is a new line."
        let range = text.line_start_offset(0)..text.line_end_offset(1) + 1;
        let new_text = "This is a new line.\nThis is new line 2.\n";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "This is a new line.\nThis is new line 2.\n这里是第 4 行。New text"
        );
        assert_eq!(wrapper.lines.len(), 3);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["This is a new line."],
                &["This is new line 2."],
                &["这里是第 4 行。New text"],
            ],
        );

        // Add a new line at the end
        let range = text.len()..text.len();
        let new_text = "\nThis is a new line at the end.";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "This is a new line.\nThis is new line 2.\n这里是第 4 行。New text\nThis is a new line at the end."
        );
        assert_eq!(wrapper.lines.len(), 4);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["This is a new line."],
                &["This is new line 2."],
                &["这里是第 4 行。New text"],
                &["This is a new line at the end."],
            ],
        );

        // Add a new line at the beginning
        let range = 0..0;
        let new_text = "This is a new line at the beginning.\n";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "This is a new line at the beginning.\nThis is a new line.\nThis is new line 2.\n这里是第 4 行。New text\nThis is a new line at the end."
        );
        assert_eq!(wrapper.lines.len(), 5);
        assert_wrapper_lines(
            &text,
            &wrapper,
            &[
                &["This is a new line at the beginning."],
                &["This is a new line."],
                &["This is new line 2."],
                &["这里是第 4 行。New text"],
                &["This is a new line at the end."],
            ],
        );

        // Remove all to at least one line in `lines`.
        let range = 0..text.len();
        let new_text = "";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &Rope::from(new_text), &mut fake_wrap_line);
        assert_eq!(text.to_string(), "");
        assert_eq!(wrapper.lines.len(), 1);
        assert_eq!(wrapper.lines[0].wrapped_lines, vec![0..0]);

        // Test update_all
        let range = 0..text.len();
        let new_text = "This is a full text.\nThis is a second line.";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, &text, &mut fake_wrap_line);
        assert_eq!(
            text.to_string(),
            "This is a full text.\nThis is a second line."
        );
        assert_eq!(wrapper.lines.len(), 2);
    }

    #[test]
    fn test_hidden_rows() {
        fn fake_wrap_line(_line: &str, _wrap_width: Pixels) -> Vec<Boundary> {
            vec![]
        }

        let font = gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        };
        let mut wrapper = TextWrapper::new(font, px(14.), None);
        let text = Rope::from("a\nb\nc\nd\n");
        wrapper._update(&text, &(0..text.len()), &text, &mut fake_wrap_line);
        assert_eq!(wrapper.len(), 5, "4 lines plus the row after the last \\n");

        // Hiding rows 1 and 2 drops them from the visual line count and from
        // their own height, without touching the rows around them.
        wrapper.set_hidden_rows(vec![1, 2]);
        assert_eq!(wrapper.len(), 3);
        assert_eq!(wrapper.lines[0].height(px(20.)), px(20.));
        assert_eq!(wrapper.lines[1].height(px(20.)), px(0.));
        assert_eq!(wrapper.lines[2].height(px(20.)), px(0.));
        assert_eq!(wrapper.lines[3].height(px(20.)), px(20.));
        // The wrap info survives, so unfolding needs no re-wrap.
        assert_eq!(wrapper.lines[1].wrapped_lines, vec![0..1]);

        // **Vertical cursor movement rides on this**: a display point never
        // lands on a hidden row, so `move_vertical` steps over a fold with no
        // code of its own.
        let d_offset = text.line_start_offset(3);
        let after_a = wrapper.offset_to_display_point(text.line_start_offset(0));
        let mut next = after_a;
        next.row += 1;
        assert_eq!(
            wrapper.display_point_to_offset(next),
            d_offset,
            "one row down from `a` is `d`, skipping the folded `b` and `c`"
        );

        // Clearing brings them back.
        wrapper.set_hidden_rows(vec![]);
        assert_eq!(wrapper.len(), 5);
        assert_eq!(wrapper.lines[1].height(px(20.)), px(20.));
    }

    #[test]
    fn test_hidden_rows_survive_an_edit() {
        fn fake_wrap_line(_line: &str, _wrap_width: Pixels) -> Vec<Boundary> {
            vec![]
        }

        let font = gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        };
        let mut wrapper = TextWrapper::new(font, px(14.), None);
        let mut text = Rope::from("a\nb\nc\nd\n");
        wrapper._update(&text, &(0..text.len()), &text, &mut fake_wrap_line);
        wrapper.set_hidden_rows(vec![2]);
        assert_eq!(wrapper.len(), 4);

        // `_update` splices `lines`; the flags have to be re-applied or the
        // spliced-in rows come back with stale ones.
        let range = 0..1;
        text.replace(range.clone(), "AA");
        wrapper._update(&text, &range, &Rope::from("AA"), &mut fake_wrap_line);
        assert_eq!(wrapper.len(), 4, "row 2 is still hidden after the edit");
        assert_eq!(wrapper.lines[2].height(px(20.)), px(0.));
    }

    #[test]
    fn test_line_layout() {
        let mut line_layout = LineLayout::new();

        let line1 = ShapedLine::default().with_len(100);
        let line2 = ShapedLine::default().with_len(50);
        let wrapped_lines = smallvec::smallvec![line1, line2];
        line_layout.set_wrapped_lines(wrapped_lines);
        assert_eq!(line_layout.len(), 150);
        assert_eq!(line_layout.wrapped_lines.len(), 2);
    }

    #[test]
    fn test_offset_to_display_point() {
        let font = gpui::Font {
            family: "Arial".into(),
            weight: FontWeight::default(),
            style: FontStyle::Normal,
            features: FontFeatures::default(),
            fallbacks: None,
        };

        let mut wrapper = TextWrapper::new(font, px(14.), None);
        wrapper.text = Rope::from(
            "Hello, 世界!\r\nThis is second line.\nThis is third line.\n这里是第 4 行。",
        );
        wrapper.lines = vec![
            // range: 0..15
            LineItem {
                line: Rope::from("Hello, 世界!\r"),
                wrapped_lines: vec![0..15],
                hidden: false,
                extra_rows: 0,
            },
            // range: 16..36
            LineItem {
                line: Rope::from("This is second line."),
                wrapped_lines: vec![0..10, 10..20],
                hidden: false,
                extra_rows: 0,
            },
            // range: 37..56
            LineItem {
                line: Rope::from("This is third line."),
                wrapped_lines: vec![0..9, 9..15, 15..20],
                hidden: false,
                extra_rows: 0,
            },
            // range: 57..79
            LineItem {
                line: Rope::from("这里是第 4 行。"),
                wrapped_lines: vec![0..22],
                hidden: false,
                extra_rows: 0,
            },
        ];

        assert_eq!(
            wrapper.offset_to_display_point(12),
            DisplayPoint::new(0, 0, 12)
        );
        assert_eq!(
            wrapper.offset_to_display_point(15),
            DisplayPoint::new(0, 0, 15)
        );

        assert_eq!(
            wrapper.offset_to_display_point(16),
            DisplayPoint::new(1, 0, 0)
        );
        assert_eq!(
            wrapper.offset_to_display_point(21),
            DisplayPoint::new(1, 0, 5)
        );
        assert_eq!(
            wrapper.offset_to_display_point(27),
            DisplayPoint::new(2, 1, 1)
        );
        assert_eq!(
            wrapper.offset_to_display_point(37),
            DisplayPoint::new(3, 0, 0)
        );
        assert_eq!(
            wrapper.offset_to_display_point(54),
            DisplayPoint::new(5, 2, 2)
        );
        assert_eq!(
            wrapper.offset_to_display_point(59),
            DisplayPoint::new(6, 0, 2)
        );

        assert_eq!(
            wrapper.display_point_to_offset(DisplayPoint::new(6, 0, 2)),
            59
        );
        assert_eq!(
            wrapper.display_point_to_offset(DisplayPoint::new(5, 2, 2)),
            54
        );
        assert_eq!(
            wrapper.display_point_to_offset(DisplayPoint::new(3, 0, 0)),
            37
        );
        assert_eq!(
            wrapper.display_point_to_offset(DisplayPoint::new(2, 1, 1)),
            27
        );
        assert_eq!(
            wrapper.display_point_to_offset(DisplayPoint::new(1, 0, 5)),
            21
        );
        assert_eq!(
            wrapper.display_point_to_offset(DisplayPoint::new(1, 0, 0)),
            16
        );
        assert_eq!(
            wrapper.display_point_to_offset(DisplayPoint::new(0, 0, 15)),
            15
        );
    }
}
