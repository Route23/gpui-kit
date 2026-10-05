use std::ops::Range;

use gpui::{App, Font, LineFragment, Pixels, Point, ShapedLine, Size, Window, point, px, size};
use ropey::{Rope, RopeSlice};
use smallvec::SmallVec;

use crate::input::RopeExt;

/// A line with soft wrapped lines info.
#[derive(Debug, Clone)]
pub(super) struct LineItem {
    /// The bytes length of the line, without the end `\n`.
    ///
    /// **Only the length.** This was the line's text, as a `Rope` of its own
    /// that nothing read but for its length -- and a `Rope` owns a leaf of
    /// about 1 KB however short its text is. 100,000 rows kept 120 MB next to
    /// the document they were copied from (dopamine #878).
    len: usize,
    /// The soft wrapped lines relative byte range (0..line.len) of this line (Include first line).
    ///
    /// Not contains the line end `\n`.
    ///
    /// Inline while there is one range, which is every row that does not
    /// wrap: as a `Vec` this was one more allocation a row.
    pub(super) wrapped_lines: SmallVec<[Range<usize>; 1]>,
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
        self.len
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

    /// Whether `lines` has every row of `text`.
    ///
    /// It has once the whole text has gone through [`Self::update`] in one
    /// go. A text that was only put in ([`Self::set_default_text`]) has not,
    /// and [`Self::prepare_if_need`] is what makes up for it.
    ///
    /// **Nothing makes up for a change the table was not told of.** Whoever
    /// writes to the text hands the change to `update`: the element draws a
    /// frame from these rows, and a row that is longer here than in the text
    /// is a slice out of range there.
    _initialized: bool,
    /// How many times every row was laid out, for the tests to count.
    #[cfg(test)]
    rebuilds: usize,
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
            #[cfg(test)]
            rebuilds: 0,
        }
    }

    #[inline]
    pub(super) fn set_default_text(&mut self, text: &Rope) {
        self.text = text.clone();
        // Its rows are not laid out here.
        self._initialized = false;
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

        // The font only decides where a row wraps. Without a wrap width every
        // row is one line whatever it is drawn in, so laying them all out
        // again gave back the table it started from -- and the first frame of
        // an editor always came here: the wrapper is made with the window's
        // font, the editor is drawn in its own (dopamine #878).
        if self.wrap_width.is_none() {
            self.refresh_longest_row();
            return;
        }

        self.update_all(&self.text.clone(), cx);
    }

    /// Look for the longest row again, the way laying every row out does.
    ///
    /// The one thing that could come out differently for rows that do not
    /// wrap: the row remembered as the longest is not kept right through
    /// every edit, and a new font was a moment it got looked for afresh.
    fn refresh_longest_row(&mut self) {
        let mut longest = std::mem::take(&mut self.longest_row);
        // As in `_update`: the search starts over only when the row
        // remembered is one of the rows gone through.
        if longest.row < self.lines.len() {
            longest = LongestRow::default();
        }
        for (row, line) in self.lines.iter().enumerate() {
            if line.len() > longest.len {
                longest = LongestRow {
                    row,
                    len: line.len(),
                };
            }
        }
        self.longest_row = longest;
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
    /// - `new_len`: The bytes length of the inserted text.
    /// - `force`: Whether to force the update, if false, the update will be skipped if the text is the same.
    /// - `cx`: The application context.
    pub(super) fn update(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_len: usize,
        cx: &mut App,
    ) {
        let mut line_wrapper = cx
            .text_system()
            .line_wrapper(self.font.clone(), self.font_size);
        self._update(changed_text, range, new_len, &mut |line_str, wrap_width| {
            line_wrapper
                .wrap_line(&[LineFragment::text(line_str)], wrap_width)
                .collect()
        });
    }

    fn _update<F>(
        &mut self,
        changed_text: &Rope,
        range: &Range<usize>,
        new_len: usize,
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
        let new_end_row = changed_text.offset_to_point(range.start + new_len).row;
        let new_end_offset = changed_text.line_end_offset(new_end_row);
        let new_range = new_start_offset..new_end_offset;
        // Every row of the text is about to be laid out.
        let whole = new_start_row == 0 && new_end_row + 1 == changed_text.lines_len();
        if whole {
            // None of the old rows is kept, so they go first: the new table
            // used to be made next to the old one, two of them for a while.
            self.lines = Vec::new();
        }

        let new_rows = new_end_row.saturating_sub(new_start_row) + 1;
        let mut new_lines = Vec::with_capacity(new_rows);
        let wrap_width = self.wrap_width;

        // line not contains `\n`.
        //
        // Read where they are. The range used to be copied into a `Rope`, and
        // each row of that into a `String`, to measure rows that are not kept.
        for_each_row(changed_text.slice(new_range), |ix, line_str| {
            let mut wrapped_lines = SmallVec::new();
            let mut prev_boundary_ix = 0;

            if line_str.len() > longest_row_len {
                longest_row_ix = new_start_row + ix;
                longest_row_len = line_str.len();
            }

            // If wrap_width is Pixels::MAX, skip wrapping to disable word wrap
            if let Some(wrap_width) = wrap_width {
                // Here only have wrapped line, if there is no wrap meet, the `line_wraps` result will empty.
                for boundary in wrap_line(line_str, wrap_width) {
                    wrapped_lines.push(prev_boundary_ix..boundary.ix);
                    prev_boundary_ix = boundary.ix;
                }
            }

            // Reset of the line
            if !line_str[prev_boundary_ix..].is_empty() || prev_boundary_ix == 0 {
                wrapped_lines.push(prev_boundary_ix..line_str.len());
            }

            new_lines.push(LineItem {
                len: line_str.len(),
                wrapped_lines,
                hidden: false,
                extra_rows: 0,
            });
        });
        debug_assert_eq!(new_lines.len(), new_rows);

        if whole {
            // The table is these rows. From here on an edit keeps it whole,
            // so `prepare_if_need` has nothing left to do: opening a file
            // laid every row out when the text was set, and then once more
            // for the first frame (dopamine #878).
            self.lines = new_lines;
            self._initialized = true;
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        } else if self.lines.len() == 0 {
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
        self.update(text, &(0..text.len()), text.len(), cx);
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

/// Hand every row of `text` to `f`, in order: its index, and its text without
/// the `\n` (a `\r` stays, as in [`RopeExt::slice_line`]).
///
/// The text is read where it is. A row is only copied when the rope keeps it
/// in more than one piece -- or when it is the last, which no `\n` ends --
/// and then into a buffer that is used again.
fn for_each_row(text: RopeSlice<'_>, mut f: impl FnMut(usize, &str)) {
    let mut row = 0;
    // The start of a row that ends in a later chunk.
    let mut pending = String::new();
    for chunk in text.chunks() {
        let mut rest = chunk;
        while let Some(end) = rest.find('\n') {
            if pending.is_empty() {
                f(row, &rest[..end]);
            } else {
                pending.push_str(&rest[..end]);
                f(row, &pending);
                pending.clear();
            }
            row += 1;
            rest = &rest[end + 1..];
        }
        pending.push_str(rest);
    }
    // What follows the last `\n` is a row too, also when it is empty.
    f(row, &pending);
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
    ///
    /// **On the heap, not inline.** A `ShapedLine` is 3 KB (it carries 32
    /// decoration runs inline), and a row hidden by a fold has none -- but
    /// still has a `LineLayout`, one per row of the fold, every frame. Inline
    /// storage made a 3,700-row fold cost 12 MB a frame (dopamine #876).
    pub(crate) wrapped_lines: Vec<ShapedLine>,
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
            wrapped_lines: Vec::new(),
        }
    }

    pub(crate) fn new() -> Self {
        Self {
            len: 0,
            longest_width: px(0.),
            wrapped_lines: Vec::new(),
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
        self.wrapped_lines = wrapped_lines.into_vec();
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
    use crate::{
        Root,
        input::{AutoIndent, Enter, Input, InputState, LensRow, Position},
    };
    use gpui::{
        AppContext as _, Boundary, Context, Entity, EntityInputHandler as _, FontFeatures,
        FontStyle, FontWeight, IntoElement, ParentElement as _, Render, Styled as _,
        TestAppContext, VisualTestContext, Window, div, px,
    };
    use smallvec::smallvec;

    /// What a piece of code asks of the heap, for the tests that are about
    /// nothing else: how a table is made does not show in what it says.
    ///
    /// **This is the allocator of the whole test binary** -- a binary has
    /// one. It hands everything on to the system's, which is what the binary
    /// had without it, and counts on the way. The counts are kept for each
    /// thread on its own, because tests run side by side and one must not
    /// count what another asked for.
    mod heap {
        use std::alloc::{GlobalAlloc, Layout, System};
        use std::cell::Cell;

        #[derive(Clone, Copy)]
        struct Count {
            /// Times this thread asked for room.
            asked: usize,
            /// Bytes it holds. Signed: what one thread took, another may
            /// give back.
            held: isize,
            /// The most `held` has been since a measurement began.
            most: isize,
        }

        thread_local! {
            // `const`, and nothing to drop: reaching it never allocates, and
            // it is still there while a thread that is going away frees.
            static COUNT: Cell<Count> = const {
                Cell::new(Count {
                    asked: 0,
                    held: 0,
                    most: 0,
                })
            };
        }

        fn count(change: impl FnOnce(&mut Count)) {
            let _ = COUNT.try_with(|count| {
                let mut now = count.get();
                change(&mut now);
                count.set(now);
            });
        }

        fn took(bytes: usize) {
            count(|count| {
                count.asked = count.asked.wrapping_add(1);
                count.held = count.held.wrapping_add(bytes as isize);
                count.most = count.most.max(count.held);
            });
        }

        fn gave_back(bytes: usize) {
            count(|count| count.held = count.held.wrapping_sub(bytes as isize));
        }

        struct Counting;

        #[global_allocator]
        static HEAP: Counting = Counting;

        unsafe impl GlobalAlloc for Counting {
            unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
                took(layout.size());
                unsafe { System.alloc(layout) }
            }

            unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
                took(layout.size());
                unsafe { System.alloc_zeroed(layout) }
            }

            unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
                gave_back(layout.size());
                unsafe { System.dealloc(ptr, layout) }
            }

            unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
                gave_back(layout.size());
                took(new_size);
                unsafe { System.realloc(ptr, layout, new_size) }
            }
        }

        /// What was asked of the heap while something ran.
        pub(super) struct Asked {
            /// How many times it asked for room.
            pub(super) times: usize,
            /// How far over where it started the bytes it held went, at the
            /// most. Room given back and taken again does not add up.
            pub(super) over: usize,
        }

        /// Run `f` and say what it asked of the heap, on this thread.
        pub(super) fn measure<R>(f: impl FnOnce() -> R) -> (R, Asked) {
            let before = COUNT.get();
            COUNT.set(Count {
                most: before.held,
                ..before
            });
            let out = f();
            let after = COUNT.get();
            // A measurement around this one keeps its own high-water mark.
            COUNT.set(Count {
                most: after.most.max(before.most),
                ..after
            });
            let asked = Asked {
                times: after.asked.wrapping_sub(before.asked),
                over: after.most.saturating_sub(before.held).max(0) as usize,
            };
            (out, asked)
        }
    }

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
        wrapper._update(&text, &(0..text.len()), text.len(), &mut no_wrap);
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
        wrapper._update(&text, &range, "\nfive".len(), &mut no_wrap);

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

        wrapper._update(&text, &(0..text.len()), text.len(), &mut fake_wrap_line);
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
        wrapper._update(&text, &range, new_text.len(), &mut fake_wrap_line);
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
        wrapper._update(&text, &range, new_text.len(), &mut fake_wrap_line);
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
        wrapper._update(&text, &range, 0, &mut fake_wrap_line);
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
        wrapper._update(&text, &range, new_text.len(), &mut fake_wrap_line);
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
        wrapper._update(&text, &range, new_text.len(), &mut fake_wrap_line);
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
        wrapper._update(&text, &range, new_text.len(), &mut fake_wrap_line);
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
        wrapper._update(&text, &range, new_text.len(), &mut fake_wrap_line);
        assert_eq!(text.to_string(), "");
        assert_eq!(wrapper.lines.len(), 1);
        assert_eq!(wrapper.lines[0].wrapped_lines.as_slice(), [0..0]);

        // Test update_all
        let range = 0..text.len();
        let new_text = "This is a full text.\nThis is a second line.";
        text.replace(range.clone(), new_text);
        wrapper._update(&text, &range, text.len(), &mut fake_wrap_line);
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
        wrapper._update(&text, &(0..text.len()), text.len(), &mut fake_wrap_line);
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
        assert_eq!(wrapper.lines[1].wrapped_lines.as_slice(), [0..1]);

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
        wrapper._update(&text, &(0..text.len()), text.len(), &mut fake_wrap_line);
        wrapper.set_hidden_rows(vec![2]);
        assert_eq!(wrapper.len(), 4);

        // `_update` splices `lines`; the flags have to be re-applied or the
        // spliced-in rows come back with stale ones.
        let range = 0..1;
        text.replace(range.clone(), "AA");
        wrapper._update(&text, &range, "AA".len(), &mut fake_wrap_line);
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
                len: "Hello, 世界!\r".len(),
                wrapped_lines: smallvec![0..15],
                hidden: false,
                extra_rows: 0,
            },
            // range: 16..36
            LineItem {
                len: "This is second line.".len(),
                wrapped_lines: smallvec![0..10, 10..20],
                hidden: false,
                extra_rows: 0,
            },
            // range: 37..56
            LineItem {
                len: "This is third line.".len(),
                wrapped_lines: smallvec![0..9, 9..15, 15..20],
                hidden: false,
                extra_rows: 0,
            },
            // range: 57..79
            LineItem {
                len: "这里是第 4 行。".len(),
                wrapped_lines: smallvec![0..22],
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

    // ── a row is a length, laid out once (dopamine #878) ───────────────────

    /// What `for_each_row` hands over.
    fn rows_of(text: RopeSlice<'_>) -> Vec<String> {
        let mut rows = vec![];
        for_each_row(text, |ix, row| {
            assert_eq!(ix, rows.len());
            rows.push(row.to_string());
        });
        rows
    }

    /// The rows the way they were read before: the range copied into a `Rope`
    /// of its own, and each row of that into a `String`.
    fn rows_by_copying(text: RopeSlice<'_>) -> Vec<String> {
        Rope::from(text)
            .iter_lines()
            .map(|line| line.to_string())
            .collect()
    }

    /// (length, where it wraps, visual lines, rows inserted under it) of every row.
    fn rows(wrapper: &TextWrapper) -> Vec<(usize, Vec<Range<usize>>, usize, usize)> {
        wrapper
            .lines
            .iter()
            .map(|line| {
                (
                    line.len(),
                    line.wrapped_lines.to_vec(),
                    line.lines_len(),
                    line.extra_rows(),
                )
            })
            .collect()
    }

    /// Break a row every `n` bytes.
    fn wrap_every(n: usize) -> impl FnMut(&str, Pixels) -> Vec<Boundary> {
        move |line, _| {
            (1..)
                .map(|i| i * n)
                .take_while(|ix| *ix < line.len())
                .map(|ix| Boundary { ix, next_indent: 0 })
                .collect()
        }
    }

    #[test]
    fn rows_read_in_place_are_the_rows_that_were_copied() {
        for text in [
            "",
            "\n",
            "one",
            "one\n",
            "\none",
            "one\n\ntwo",
            "one\r\ntwo\r\n",
            "Hello, 世界!\r\nThis is second line.\n这里是第 4 行。",
        ] {
            let rope = Rope::from(text);
            assert_eq!(
                rows_of(rope.slice(..)),
                rows_by_copying(rope.slice(..)),
                "{text:?}"
            );
        }

        // Long enough to be kept in many pieces: rows that end in the next
        // piece, rows longer than a piece, multi-byte characters at the seams.
        let mut text = String::new();
        for i in 0..4000 {
            match i % 7 {
                0 => {}
                1 => text.push_str(&"x".repeat(i % 97)),
                2 => text.push_str(&"长".repeat(i % 53)),
                3 => text.push_str(&format!("    let v{i} = {i};\r")),
                4 => text.push_str(&"long ".repeat(700)),
                _ => text.push_str(&format!("fn f{i}() {{}}")),
            }
            text.push('\n');
        }
        let mut rope = Rope::from(text.as_str());
        // Edits leave the pieces uneven, which a rope built in one go is not.
        let mut seed = 878_u64;
        for _ in 0..300 {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let at = (seed >> 16) as usize % rope.len();
            let at = rope.clip_offset(at, sum_tree::Bias::Left);
            let piece = ["\n", "é", "\r\n", "tail of a row\nhead of the next"];
            rope.insert(at, piece[(seed >> 8) as usize % piece.len()]);
        }
        assert!(rope.chunks().count() > 100, "kept in many pieces");

        let rows = rows_of(rope.slice(..));
        assert_eq!(rows.len(), rope.lines_len());
        assert_eq!(rows, rows_by_copying(rope.slice(..)));

        // Some rows out of the middle, as an edit hands them over.
        let range = rope.line_start_offset(1000)..rope.line_end_offset(1200);
        assert_eq!(
            rows_of(rope.slice(range.clone())),
            rows_by_copying(rope.slice(range))
        );

        // And the table made of them has a row for each, as long as it is --
        // all of them at once, and after an edit in the middle.
        let lens_of = |wrapper: &TextWrapper| -> Vec<usize> {
            wrapper.lines.iter().map(|line| line.len()).collect()
        };
        let row_lens = |rope: &Rope| -> Vec<usize> {
            (0..rope.lines_len())
                .map(|row| rope.line_len(row))
                .collect()
        };
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        wrapper._update(&rope, &(0..0), rope.len(), &mut no_wrap);
        assert_eq!(lens_of(&wrapper), row_lens(&rope));

        let at = rope.line_start_offset(2000);
        rope.insert(at, "x\ny");
        wrapper._update(&rope, &(at..at), 3, &mut no_wrap);
        assert_eq!(lens_of(&wrapper), row_lens(&rope));
    }

    /// The same rows, and not copies of them: a row that lies in one piece of
    /// the rope is handed over where it is. The test above cannot tell -- it
    /// reads what a row says, and a copy says the same.
    #[test]
    fn a_row_in_one_piece_is_read_where_it_is() {
        let rope = Rope::from("one\ntwo\n\nlast");
        let mut pieces = rope.chunks();
        let piece = pieces.next().unwrap().as_bytes().as_ptr_range();
        assert!(pieces.next().is_none(), "kept in one piece");

        let mut in_place = vec![];
        for_each_row(rope.slice(..), |_, row| {
            in_place.push(piece.contains(&row.as_ptr()));
        });
        // Nothing says the last row is over but the text being over, so it
        // is the one that goes through the buffer.
        assert_eq!(in_place, [true, true, true, false]);

        // In a text kept in many pieces, the rows that are copied are the
        // ones that end in a later piece than they begin in -- no more than
        // one a piece, and there are far fewer pieces than rows.
        let rope = Rope::from("let v = 00000;\n".repeat(10_000));
        let pieces: Vec<_> = rope
            .chunks()
            .map(|piece| piece.as_bytes().as_ptr_range())
            .collect();
        assert!(pieces.len() > 100, "kept in many pieces");
        let (mut rows, mut copied) = (0, 0);
        for_each_row(rope.slice(..), |_, row| {
            rows += 1;
            if !pieces.iter().any(|piece| piece.contains(&row.as_ptr())) {
                copied += 1;
            }
        });
        assert_eq!(rows, 10_001);
        assert!(
            copied <= pieces.len() && pieces.len() * 10 < rows,
            "{copied} of {rows} rows copied, {} pieces",
            pieces.len()
        );
    }

    /// A row keeps what is asked of it -- how long it is, where it wraps, two
    /// marks -- and not a copy of its text.
    #[test]
    fn a_row_keeps_a_length_not_its_text() {
        // The copy was a `Rope` in the row, and about 1 KB behind it.
        assert!(
            std::mem::size_of::<LineItem>() <= 56,
            "{} bytes in a row",
            std::mem::size_of::<LineItem>()
        );

        let text = Rope::from("Hello, 世界!\r\n\nlast");
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        wrapper._update(&text, &(0..0), text.len(), &mut no_wrap);
        assert_eq!(wrapper.lines.len(), 3);
        for (row, line) in wrapper.lines.iter().enumerate() {
            assert_eq!(line.len(), text.line_len(row));
            // One range for a row that does not wrap, kept in the row itself.
            assert_eq!(line.wrapped_lines.as_slice(), [0..line.len()]);
            assert!(!line.wrapped_lines.spilled());
        }
    }

    /// Laying every row out takes one table, not something for each row, and
    /// laying them out again lets the old table go before the new one is
    /// made. Neither shows in what the table says, only in what the heap was
    /// asked for.
    #[test]
    fn every_row_laid_out_is_one_table_on_the_heap() {
        const ROWS: usize = 10_000;
        let text_of = |row: &str| Rope::from(format!("{}last", row.repeat(ROWS - 1)));
        let table = ROWS * std::mem::size_of::<LineItem>();

        let text = text_of("let v = 00000;\n");
        let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
        let ((), first) = heap::measure(|| {
            wrapper._update(&text, &(0..0), text.len(), &mut no_wrap);
        });
        assert_eq!(wrapper.lines.len(), ROWS);
        assert!(first.over >= table, "the table was seen being made");
        // The table, and the buffer the rows that end in a later piece are
        // put together in. It was a `String` for every row, on top of a copy
        // of the text.
        assert!(
            first.times * 100 < ROWS,
            "the heap was asked {} times for {ROWS} rows",
            first.times
        );

        // Another text of as many rows, the way setting a value hands it
        // over: at no time are there two tables.
        let other = text_of("let w = 11111;\n");
        let ((), again) = heap::measure(|| {
            wrapper._update(&other, &(0..text.len()), other.len(), &mut no_wrap);
        });
        assert_eq!(wrapper.lines.len(), ROWS);
        assert!(again.times * 100 < ROWS, "asked {} times", again.times);
        assert!(
            again.over < table / 2,
            "{} bytes over where it started; a table is {table}",
            again.over
        );

        // With rows that wrap, which keep their ranges on the heap, the way
        // a new width lays them out. The first time the ranges are new...
        wrapper.wrap_width = Some(px(100.));
        let mut wrap = wrap_every(4);
        wrapper._update(&other, &(0..other.len()), other.len(), &mut wrap);
        assert!(wrapper.lines[0].wrapped_lines.spilled());
        // ...and after that they go with the table they belong to.
        let ((), wrapped) = heap::measure(|| {
            wrapper._update(&other, &(0..other.len()), other.len(), &mut wrap);
        });
        assert_eq!(wrapper.len(), 4 * (ROWS - 1) + 1);
        assert!(
            wrapped.over < table / 2,
            "{} bytes over where it started; a table is {table}",
            wrapped.over
        );
    }

    /// Soft wrap, a fold and rows inserted under a row at once -- through an
    /// edit, and through every row being laid out again.
    #[test]
    fn wrapped_folded_and_inserted_rows_keep_their_height() {
        let mut text = Rope::from("abcdefghij\nab\n\nabcdefgh\nabcde");
        let mut wrapper = TextWrapper::new(test_font(), px(14.), Some(px(100.)));
        wrapper._update(&text, &(0..0), text.len(), &mut wrap_every(4));
        let h = px(20.);
        let heights = |wrapper: &TextWrapper| -> Vec<Pixels> {
            wrapper.lines.iter().map(|line| line.height(h)).collect()
        };

        assert_eq!(
            rows(&wrapper),
            vec![
                (10, vec![0..4, 4..8, 8..10], 3, 0),
                (2, vec![0..2], 1, 0),
                (0, vec![0..0], 1, 0),
                (8, vec![0..4, 4..8], 2, 0),
                (5, vec![0..4, 4..5], 2, 0),
            ]
        );
        assert_eq!(wrapper.len(), 9);
        assert_eq!(wrapper.total_height(h), px(180.));

        // Row 3 folded, with what is inserted under it; two rows under row 0.
        wrapper.set_hidden_rows(vec![3]);
        wrapper.set_extra_rows(vec![(0, 2), (3, 1)]);
        let marked = [px(100.), px(20.), px(20.), px(0.), px(40.)];
        assert_eq!(heights(&wrapper), marked);
        assert_eq!(wrapper.len(), 7);
        assert_eq!(wrapper.total_height(h), px(180.));

        // An edit in a wrapped row: that row is laid out again, alone.
        let range = 10..10;
        text.replace(range.clone(), "kl");
        wrapper._update(&text, &range, 2, &mut wrap_every(4));
        assert_eq!(wrapper.lines[0].len(), 12);
        assert_eq!(
            wrapper.lines[0].wrapped_lines.as_slice(),
            [0..4, 4..8, 8..12]
        );
        assert_eq!(heights(&wrapper), marked);

        // Every row again, as a new wrap width does it.
        let before = rows(&wrapper);
        wrapper._update(&text, &(0..text.len()), text.len(), &mut wrap_every(4));
        assert_eq!(rows(&wrapper), before);
        assert_eq!(heights(&wrapper), marked);
        assert_eq!(wrapper.len(), 7);
        assert_eq!(wrapper.total_height(h), px(180.));
    }

    /// The whole text goes through `update` when it is set, and that is its
    /// layout: the first frame finds nothing left to do.
    #[gpui::test]
    fn rows_laid_out_with_the_text_are_not_laid_out_again(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text = Rope::from("one\ntwo\nthree");
            let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
            wrapper.update(&text, &(0..0), text.len(), cx);
            assert_eq!(wrapper.rebuilds, 1);

            wrapper.prepare_if_need(&text, cx);
            assert_eq!(wrapper.rebuilds, 1);
            assert_eq!(wrapper.lines.len(), 3);
        });
    }

    /// A text that was only put in (`default_value`) has no rows yet, and an
    /// edit before the first frame lays out the rows it touches, not the
    /// rest. `prepare_if_need` makes up for both, as it did.
    #[gpui::test]
    fn a_text_that_was_only_put_in_is_laid_out_when_asked(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let lens = |wrapper: &TextWrapper| -> Vec<usize> {
                wrapper.lines.iter().map(|line| line.len()).collect()
            };

            let mut text = Rope::from("one\ntwo\nthree");
            let mut wrapper = TextWrapper::new(test_font(), px(14.), None);
            wrapper.set_default_text(&text);
            assert!(wrapper.lines.is_empty());

            let at = text.line_start_offset(1);
            text.replace(at..at, "x");
            wrapper.update(&text, &(at..at), 1, cx);
            assert_eq!(lens(&wrapper), [4], "only the row that was touched");
            assert_eq!(wrapper.rebuilds, 0);

            wrapper.prepare_if_need(&text, cx);
            assert_eq!(wrapper.rebuilds, 1);
            assert_eq!(lens(&wrapper), [3, 4, 5]);

            wrapper.prepare_if_need(&text, cx);
            assert_eq!(wrapper.rebuilds, 1, "laid out now");

            // Another text put in is rows not laid out, again -- and fewer
            // of them, with none of the old ones left behind.
            let text = Rope::from("a\nb");
            wrapper.set_default_text(&text);
            wrapper.prepare_if_need(&text, cx);
            assert_eq!(wrapper.rebuilds, 2);
            assert_eq!(lens(&wrapper), [1, 1]);
        });
    }

    /// Without a wrap width no row depends on the font; with one, every row
    /// does. Either way a fold and the rows inserted under a row stay.
    #[gpui::test]
    fn a_new_font_lays_out_only_rows_that_wrap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let text = Rope::from(format!(
                "{}\nshort\n\n{}",
                "word ".repeat(40),
                "x".repeat(90)
            ));
            // What a wrapper made with this font and width from the start has.
            let fresh = |font_size: Pixels, wrap_width: Option<Pixels>, cx: &mut App| {
                let mut wrapper = TextWrapper::new(test_font(), font_size, wrap_width);
                wrapper.update(&text, &(0..0), text.len(), cx);
                wrapper.set_hidden_rows(vec![1]);
                wrapper.set_extra_rows(vec![(0, 2)]);
                rows(&wrapper)
            };

            let mut wrapper = TextWrapper::new(test_font(), px(16.), None);
            wrapper.update(&text, &(0..0), text.len(), cx);
            wrapper.set_hidden_rows(vec![1]);
            wrapper.set_extra_rows(vec![(0, 2)]);
            assert_eq!(wrapper.rebuilds, 1);
            let one_line_each = rows(&wrapper);
            assert_eq!(one_line_each[0], (200, vec![0..200], 1, 2));
            assert_eq!(one_line_each[1], (5, vec![0..5], 0, 0), "folded");

            wrapper.set_font(test_font(), px(14.), cx);
            assert_eq!(wrapper.rebuilds, 1, "no row wraps, so none was laid out");
            assert_eq!(rows(&wrapper), one_line_each);
            assert_eq!(rows(&wrapper), fresh(px(14.), None, cx));

            // The font was taken all the same: it is the one the rows wrap in
            // once there is a width.
            wrapper.set_wrap_width(Some(px(300.)), cx);
            assert_eq!(wrapper.rebuilds, 2);
            let wrapped = rows(&wrapper);
            assert!(wrapped[0].1.len() > 1, "the long row wraps");
            assert_eq!(wrapped[0].3, 2, "rows inserted under it are still there");
            assert_eq!(wrapped[1].2, 0, "the fold is still closed");
            assert_eq!(wrapped, fresh(px(14.), Some(px(300.)), cx));
            assert_ne!(wrapped, fresh(px(16.), Some(px(300.)), cx));

            // With a width the font decides, and every row is laid out again.
            wrapper.set_font(test_font(), px(28.), cx);
            assert_eq!(wrapper.rebuilds, 3);
            let larger = rows(&wrapper);
            assert!(larger[0].1.len() > wrapped[0].1.len(), "fewer letters fit");
            assert_eq!(larger[0].3, 2);
            assert_eq!(larger[1].2, 0);
            assert_eq!(larger, fresh(px(28.), Some(px(300.)), cx));

            // The same font again is nothing to do.
            wrapper.set_font(test_font(), px(28.), cx);
            assert_eq!(wrapper.rebuilds, 3);
        });
    }

    /// Rows that do not wrap are not laid out for a new font, but the longest
    /// of them is still looked for again, as laying them out did: it is not
    /// kept right through every edit.
    #[gpui::test]
    fn a_new_font_still_looks_for_the_longest_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let mut text = Rope::from("the longest row there is\nshort\na middling row");
            let mut wrapper = TextWrapper::new(test_font(), px(16.), None);
            wrapper.update(&text, &(0..0), text.len(), cx);
            assert_eq!((wrapper.longest_row.row, wrapper.longest_row.len), (0, 24));

            // Cut the longest row short.
            text.replace(0..24, "cut");
            wrapper.update(&text, &(0..24), 3, cx);

            wrapper.set_font(test_font(), px(14.), cx);
            assert_eq!(wrapper.rebuilds, 1);
            assert_eq!((wrapper.longest_row.row, wrapper.longest_row.len), (2, 14));
        });
    }

    /// A window with one editor in it.
    struct Editor {
        input: Entity<InputState>,
    }

    impl Render for Editor {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(Input::new(&self.input).h_full())
        }
    }

    /// Open an editor the way a host opens a file: it is made (`build`), given
    /// what `then` hands it -- its text, a fold to put back, lenses -- and
    /// only then drawn.
    fn open(
        build: impl FnOnce(InputState) -> InputState,
        then: impl FnOnce(&mut InputState, &mut Window, &mut Context<InputState>),
        cx: &mut TestAppContext,
    ) -> (Entity<InputState>, &'static mut VisualTestContext) {
        cx.update(crate::init);
        let mut input = None;
        let window = cx.add_window(|window, cx| {
            let state = cx.new(|cx| build(InputState::new(window, cx)));
            state.update(cx, |state, cx| then(state, window, cx));
            input = Some(state.clone());
            let editor = cx.new(|_| Editor { input: state });
            Root::new(editor, window, cx)
        });
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        cx.run_until_parked();
        (input.unwrap(), cx)
    }

    /// The rows were laid out three times before the first frame was out:
    /// when the text was set, for the editor's font, and because nobody had
    /// noted that they were laid out.
    #[gpui::test]
    fn opening_a_file_lays_the_rows_out_once(cx: &mut TestAppContext) {
        let text = "fn main() {\n    println!(\"hello\");\n}\n".repeat(50);
        let (input, cx) = open(
            |state| state.code_editor("text").soft_wrap(false),
            |state, window, cx| state.set_value(text.clone(), window, cx),
            cx,
        );

        let rem_size = cx.update(|window, _| window.rem_size());
        input.read_with(cx, |state, _| {
            assert!(state.last_layout.is_some(), "a frame was drawn");
            let wrapper = &state.text_wrapper;
            assert_ne!(wrapper.font_size, rem_size, "in the editor's own font");
            assert_eq!(wrapper.lines.len(), 151);
            assert_eq!(wrapper.len(), 151);
            assert_eq!(wrapper.rebuilds, 1);
        });

        // A new text in the same editor is one more layout, not three.
        input.update_in(cx, |state, window, cx| {
            state.set_value("one\ntwo", window, cx);
        });
        cx.run_until_parked();
        input.read_with(cx, |state, _| {
            assert_eq!(state.text_wrapper.lines.len(), 2);
            assert_eq!(state.text_wrapper.rebuilds, 2);
        });
    }

    /// The same for an input that is not an editor: three times it was.
    #[gpui::test]
    fn a_one_line_input_is_laid_out_once_too(cx: &mut TestAppContext) {
        let (line, cx) = open(
            |state| state,
            |state, window, cx| state.set_value("one line", window, cx),
            cx,
        );
        line.read_with(cx, |state, _| {
            assert!(state.last_layout.is_some(), "a frame was drawn");
            assert_eq!(state.text_wrapper.lines.len(), 1);
            assert_eq!(state.text_wrapper.lines[0].len(), 8);
            assert_eq!(state.text_wrapper.rebuilds, 1);
        });
    }

    /// A text that was only put in has no rows until the first frame lays
    /// them out -- once, where it was once for the font and once more.
    #[gpui::test]
    fn a_default_value_is_laid_out_by_the_first_frame(cx: &mut TestAppContext) {
        let (input, cx) = open(
            |state| {
                state
                    .code_editor("text")
                    .soft_wrap(false)
                    .default_value("one\ntwo\nthree")
            },
            |state, _, _| assert!(state.text_wrapper.lines.is_empty()),
            cx,
        );
        input.read_with(cx, |state, _| {
            assert!(state.last_layout.is_some(), "a frame was drawn");
            let lens: Vec<usize> = state.text_wrapper.lines.iter().map(LineItem::len).collect();
            assert_eq!(lens, [3, 3, 5]);
            assert_eq!(state.text_wrapper.rebuilds, 1);
        });
    }

    /// Typing in an opened file lays out the rows it touches and no others,
    /// and the frame that follows finds every row as long as its text (the
    /// element asserts as much for the rows it draws).
    #[gpui::test]
    fn an_edit_in_an_opened_file_lays_out_its_own_rows(cx: &mut TestAppContext) {
        let text = "one\ntwo\nthree\nfour\n".repeat(30);
        let (input, cx) = open(
            |state| state.code_editor("text").soft_wrap(false),
            |state, window, cx| state.set_value(text.clone(), window, cx),
            cx,
        );
        let in_step = |state: &InputState| {
            let wrapper = &state.text_wrapper;
            assert_eq!(wrapper.rebuilds, 1, "no edit laid every row out");
            assert_eq!(wrapper.lines.len(), state.text.lines_len());
            for (row, line) in wrapper.lines.iter().enumerate() {
                assert_eq!(line.len(), state.text.line_len(row), "row {row}");
            }
        };

        // A row grows, and a row is added under it.
        input.update_in(cx, |state, window, cx| {
            state.set_cursor_position(Position::new(1, 3), window, cx);
            state.insert(" and a half\nnearly three", window, cx);
        });
        cx.run_until_parked();
        input.read_with(cx, |state, _| {
            assert_eq!(state.text.lines_len(), 122);
            assert_eq!(state.text_wrapper.lines[1].len(), "two and a half".len());
            in_step(state);
        });

        // Five rows go.
        input.update_in(cx, |state, window, cx| {
            let range = lsp_types::Range::new(Position::new(4, 0), Position::new(9, 0));
            state.replace_text_in_lsp_range(&range, "", window, cx);
        });
        cx.run_until_parked();
        input.read_with(cx, |state, _| {
            assert_eq!(state.text.lines_len(), 117);
            in_step(state);
        });
    }

    /// With soft wrap the width is only known once the editor has been drawn,
    /// so the rows are laid out when the text is set and once more for the
    /// width -- in the editor's font, which they were not laid out for when
    /// it arrived. That used to be four times.
    #[gpui::test]
    fn opening_a_file_with_soft_wrap_lays_the_rows_out_twice(cx: &mut TestAppContext) {
        let text = format!("{}\nshort\n\n{}\n", "word ".repeat(200), "x".repeat(500));
        let (input, cx) = open(
            |state| state.code_editor("text").soft_wrap(true),
            |state, window, cx| state.set_value(text.clone(), window, cx),
            cx,
        );

        let rem_size = cx.update(|window, _| window.rem_size());
        let (font, font_size, wrap_width, wrapped) = input.read_with(cx, |state, _| {
            assert!(state.last_layout.is_some(), "a frame was drawn");
            let wrapper = &state.text_wrapper;
            assert_eq!(wrapper.rebuilds, 2);
            assert_eq!(wrapper.lines.len(), 5);
            assert!(
                wrapper.lines[0].wrapped_lines.len() > 1,
                "the long row wraps"
            );
            assert!(wrapper.len() > 5);
            (
                wrapper.font.clone(),
                wrapper.font_size,
                wrapper.wrap_width,
                rows(wrapper),
            )
        });
        assert!(wrap_width.is_some());
        assert_ne!(font_size, rem_size);

        // What laying every row out for that font and that width gives.
        let text = Rope::from(text);
        let fresh = |font_size: Pixels, cx: &mut App| {
            let mut wrapper = TextWrapper::new(font.clone(), font_size, wrap_width);
            wrapper.update(&text, &(0..0), text.len(), cx);
            rows(&wrapper)
        };
        cx.update(|_, cx| {
            assert_eq!(wrapped, fresh(font_size, cx));
            assert_ne!(
                wrapped,
                fresh(rem_size, cx),
                "not in the font it was made with"
            );
        });
    }

    /// A fold put back before the first frame and a lens above a row, in a
    /// file opened with soft wrap: every row is as tall as laying it out
    /// from scratch makes it.
    #[gpui::test]
    fn a_fold_and_a_lens_keep_their_height_when_a_file_opens(cx: &mut TestAppContext) {
        let text = format!(
            "fn main() {{\n    let a = 1;\n    let b = 2;\n}}\n{}\ntail",
            "word ".repeat(200)
        );
        let (input, cx) = open(
            |state| state.code_editor("text").folding(true).soft_wrap(true),
            |state, window, cx| {
                state.set_value(text.clone(), window, cx);
                state.set_folded_rows(&[0], cx);
                state.set_lens_rows(
                    vec![LensRow {
                        row: 5,
                        items: vec!["Run".into()],
                    }],
                    cx,
                );
            },
            cx,
        );

        let h = px(20.);
        let rem_size = cx.update(|window, _| window.rem_size());
        let (font, font_size, wrap_width, hidden, opened) = input.read_with(cx, |state, _| {
            let wrapper = &state.text_wrapper;
            assert!(state.last_layout.is_some(), "a frame was drawn");
            assert!(state.is_folded(0));
            assert_eq!(wrapper.rebuilds, 2);
            assert_ne!(wrapper.font_size, rem_size, "in the editor's own font");
            (
                wrapper.font.clone(),
                wrapper.font_size,
                wrapper.wrap_width,
                wrapper.hidden_rows.clone(),
                rows(wrapper),
            )
        });
        assert!(hidden.contains(&1) && hidden.contains(&2), "{hidden:?}");

        // Row 0 shows, its body does not; the long row wraps, and carries the
        // row the lens above `tail` sits in.
        assert_eq!(opened[0].2, 1);
        assert_eq!((opened[1].2, opened[2].2), (0, 0));
        assert!(opened[4].2 > 1);
        assert_eq!(opened[4].3, 1);
        assert_eq!((opened[5].2, opened[5].3), (1, 0));

        // Every row as tall as laying it out from scratch makes it, in that
        // font and for that width.
        let lines: usize = opened.iter().map(|row| row.2 + row.3).sum();
        let text = Rope::from(text);
        cx.update(|_, cx| {
            let mut fresh = TextWrapper::new(font, font_size, wrap_width);
            fresh.update(&text, &(0..0), text.len(), cx);
            fresh.set_hidden_rows(hidden);
            fresh.set_extra_rows(vec![(4, 1)]);
            assert_eq!(opened, rows(&fresh));
            assert_eq!(fresh.total_height(h), lines as f32 * h);
        });
        input.read_with(cx, |state, _| {
            assert_eq!(state.text_wrapper.total_height(h), lines as f32 * h);
        });
    }

    // ── the table hears of every change to the text ────────────────────────

    /// The table has a row for every row of the text, as long as it is. The
    /// element draws a frame from the table, and a row that is longer there
    /// than in the text is a slice out of range.
    #[track_caller]
    fn assert_rows_in_step(state: &InputState) {
        let wrapper = &state.text_wrapper;
        assert_eq!(wrapper.lines.len(), state.text.lines_len());
        for (row, line) in wrapper.lines.iter().enumerate() {
            assert_eq!(line.len(), state.text.line_len(row), "row {row}");
        }
    }

    /// An editor that indents a new row the way `how` says, and takes the
    /// indent back when the caret leaves the row if `trim_auto` is set.
    fn indenting(state: InputState, how: AutoIndent, trim_auto: bool) -> InputState {
        state
            .code_editor("text")
            .soft_wrap(false)
            .auto_indent(how, false, false, trim_auto, false)
    }

    /// Indent that Enter wrote and nobody typed after is taken back when the
    /// caret leaves the row (`trim_auto_whitespace`). That is a change to the
    /// text which does not go through `replace_text_in_range`, and the table
    /// was not told of it.
    #[gpui::test]
    fn indent_taken_back_when_the_caret_leaves_reaches_the_table(cx: &mut TestAppContext) {
        let (input, cx) = open(
            |state| indenting(state, AutoIndent::Keep, true),
            |state, window, cx| state.set_value("    foo\nbar", window, cx),
            cx,
        );

        input.update_in(cx, |state, window, cx| {
            state.move_to(7, None, cx);
            state.enter(&Enter { secondary: false }, window, cx);
            assert_eq!(state.text.to_string(), "    foo\n    \nbar");
            assert_rows_in_step(state);
        });
        cx.run_until_parked();

        input.update_in(cx, |state, _, cx| {
            state.move_to(0, None, cx);
            assert_eq!(state.text.to_string(), "    foo\n\nbar");
            assert_rows_in_step(state);
        });
        // The frame that follows is drawn from the table.
        cx.run_until_parked();
        input.read_with(cx, |state, _| assert_rows_in_step(state));
    }

    /// A closing bracket typed as the first thing on a row pulls the row back
    /// a level (`AutoIndent::Brackets`), as an edit of its own. When the
    /// bracket is already there the caret only steps over it, and nothing
    /// that an edit goes on to do was reached -- the table among it.
    #[gpui::test]
    fn indent_pulled_back_by_a_closer_reaches_the_table(cx: &mut TestAppContext) {
        let (input, cx) = open(
            |state| indenting(state, AutoIndent::Brackets, false),
            |state, window, cx| state.set_value("{\n    }\n{\n    ", window, cx),
            cx,
        );

        // In front of the closer: one level goes -- two columns, the tab size
        // an editor is made with -- and the caret is past the closer that
        // was there.
        input.update_in(cx, |state, window, cx| {
            state.move_to(6, None, cx);
            state.replace_text_in_range(None, "}", window, cx);
            assert_eq!(state.text.to_string(), "{\n  }\n{\n    ");
            assert_eq!(state.cursor(), 5);
            assert_rows_in_step(state);
        });
        cx.run_until_parked();
        input.read_with(cx, |state, _| assert_rows_in_step(state));

        // With no closer to step over, the bracket is typed after the row is
        // pulled back: two edits of the one row.
        input.update_in(cx, |state, window, cx| {
            state.move_to(state.text.len(), None, cx);
            state.replace_text_in_range(None, "}", window, cx);
            assert_eq!(state.text.to_string(), "{\n  }\n{\n  }");
            assert_rows_in_step(state);
        });
        cx.run_until_parked();
        input.read_with(cx, |state, _| assert_rows_in_step(state));
    }

    /// Soft wrap turned on in an editor that has not been drawn has no width
    /// to wrap at: the bounds it would take one from have not been set, and
    /// are none wide. Every row was laid out nearly a letter to a line for
    /// that, and once more for the editor's font, before the first frame
    /// brought the width -- four layouts for a file opened this way.
    #[gpui::test]
    fn soft_wrap_turned_on_before_the_first_frame_waits_for_a_width(cx: &mut TestAppContext) {
        let text = format!("{}\nshort\n\n{}\n", "word ".repeat(200), "x".repeat(500));
        let (input, cx) = open(
            |state| state.code_editor("text").soft_wrap(false),
            |state, window, cx| {
                state.set_value(text.clone(), window, cx);
                state.set_soft_wrap(true, window, cx);
                assert_eq!(state.text_wrapper.wrap_width, None, "no width yet");
                assert_eq!(state.text_wrapper.rebuilds, 1);
            },
            cx,
        );

        let (font, font_size, wrap_width, wrapped) = input.read_with(cx, |state, _| {
            assert!(state.last_layout.is_some(), "a frame was drawn");
            let wrapper = &state.text_wrapper;
            assert_eq!(wrapper.rebuilds, 2);
            assert!(
                wrapper.lines[0].wrapped_lines.len() > 1,
                "the long row wraps"
            );
            (
                wrapper.font.clone(),
                wrapper.font_size,
                wrapper.wrap_width,
                rows(wrapper),
            )
        });
        assert!(wrap_width.is_some_and(|width| width > px(0.)));

        // What an editor made with soft wrap on comes to: every row laid out
        // for the editor's font and the width of its first frame.
        let text = Rope::from(text);
        cx.update(|_, cx| {
            let mut fresh = TextWrapper::new(font, font_size, wrap_width);
            fresh.update(&text, &(0..0), text.len(), cx);
            assert_eq!(wrapped, rows(&fresh));
        });
    }

    /// Once there has been a frame there is a width, and turning soft wrap
    /// on wraps at once, as it did.
    #[gpui::test]
    fn soft_wrap_turned_on_after_a_frame_wraps_at_once(cx: &mut TestAppContext) {
        let text = format!("{}\nshort\n", "word ".repeat(200));
        let (input, cx) = open(
            |state| state.code_editor("text").soft_wrap(false),
            |state, window, cx| state.set_value(text.clone(), window, cx),
            cx,
        );
        input.update_in(cx, |state, window, cx| {
            assert!(state.last_layout.is_some(), "a frame was drawn");
            assert_eq!(state.text_wrapper.rebuilds, 1);
            assert_eq!(state.text_wrapper.lines[0].wrapped_lines.len(), 1);

            state.set_soft_wrap(true, window, cx);
            let wrapper = &state.text_wrapper;
            assert_eq!(wrapper.wrap_width, Some(state.input_bounds.size.width));
            assert_eq!(wrapper.rebuilds, 2);
            assert!(
                wrapper.lines[0].wrapped_lines.len() > 1,
                "the long row wraps"
            );

            // And off again, which needs no width.
            state.set_soft_wrap(false, window, cx);
            assert_eq!(state.text_wrapper.wrap_width, None);
            assert_eq!(state.text_wrapper.lines[0].wrapped_lines.len(), 1);
        });
    }
}
