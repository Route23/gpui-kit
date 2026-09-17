//! Code folding: which rows head a fold, and which rows a fold hides.
//!
//! Fold ranges come from **indentation**, not from the syntax tree: a row heads
//! a fold when the rows after it are indented more deeply. That is VS Code's
//! fallback strategy, and unlike a `folds.scm` query it works for every
//! language the editor can open without carrying a query file per grammar.
//!
//! Everything in the top half of this file is pure so it can be unit tested;
//! the state that drives it lives on [`InputState`].

use std::ops::{Range, RangeInclusive};

use ropey::Rope;

use gpui::{
    App, Context, MouseButton, MouseDownEvent, Pixels, Point, ShapedLine, SharedString, TextRun,
    TextStyle, Window, px,
};

use crate::{
    ActiveTheme as _, RopeExt as _,
    input::{
        FoldAll, InputState, LastLayout, TabSize, ToggleFold, UnfoldAll, element::TextElement,
    },
};

/// Whether a line is blank (only whitespace, including a lone `\r`).
fn is_blank(line: &str) -> bool {
    line.chars().all(char::is_whitespace)
}

/// What a supplied range is for.
///
/// Mirrors LSP's `FoldingRangeKind`; anything the server does not name is a
/// plain region.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FoldKind {
    #[default]
    Region,
    Comment,
    Imports,
}

/// A fold range handed in from outside, instead of being read off the
/// indentation.
///
/// Rows are 0-based and **inclusive on both ends**, the way LSP reports them:
/// `start_row` stays visible (it heads the fold) and everything up to and
/// including `end_row` is hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldRange {
    pub start_row: usize,
    pub end_row: usize,
    pub kind: FoldKind,
}

/// The rows a supplied range hides, or `None` when no range heads `row`.
///
/// The same end-exclusive shape [`fold_range`] returns, so the two can be used
/// interchangeably.
fn supplied_range(ranges: &[FoldRange], row: usize) -> Option<Range<usize>> {
    ranges
        .iter()
        .find(|r| r.start_row == row && r.end_row > r.start_row)
        .map(|r| r.start_row + 1..r.end_row + 1)
}

/// The rows hidden by folding at `row`, or `None` when `row` heads no fold.
///
/// End-exclusive, and never contains `row` itself.
///
/// **When ranges are supplied, only they are consulted** -- indentation is not
/// mixed in. A language server that answers knows the language better than the
/// indentation rule does, and mixing the two would put chevrons on rows the
/// server deliberately left out (VS Code behaves the same way).
pub(super) fn fold_range_with(
    text: &Rope,
    row: usize,
    tab: TabSize,
    supplied: Option<&[FoldRange]>,
) -> Option<Range<usize>> {
    match supplied {
        Some(ranges) => supplied_range(ranges, row),
        None => fold_range(text, row, tab),
    }
}

/// Whether `row` heads a fold, honouring supplied ranges.
///
/// **Must agree with [`fold_range_with`]** -- the gutter asks this one and the
/// click asks the other, and a disagreement puts a chevron on a row that does
/// not fold.
pub(super) fn is_foldable_with(
    text: &Rope,
    row: usize,
    tab: TabSize,
    supplied: Option<&[FoldRange]>,
) -> bool {
    match supplied {
        Some(ranges) => supplied_range(ranges, row).is_some(),
        None => is_foldable(text, row, tab),
    }
}

pub(super) fn fold_range(text: &Rope, row: usize, tab: TabSize) -> Option<Range<usize>> {
    let rows = text.lines_len();
    if row + 1 >= rows {
        return None;
    }

    let header = text.slice_line(row).to_string();
    if is_blank(&header) {
        return None;
    }
    let base = tab.indent_count(&text.slice_line(row));

    let mut end = row;
    let mut scan = row + 1;
    while scan < rows {
        let line = text.slice_line(scan);
        if is_blank(&line.to_string()) {
            scan += 1;
            continue;
        }
        if tab.indent_count(&line) > base {
            end = scan;
            scan += 1;
        } else {
            break;
        }
    }

    (end > row).then(|| row + 1..end + 1)
}

/// Whether `row` heads a fold.
///
/// Cheaper than [`fold_range`] — it only walks as far as the next non-blank
/// line — and is what the gutter asks once per visible row.
pub(super) fn is_foldable(text: &Rope, row: usize, tab: TabSize) -> bool {
    let rows = text.lines_len();
    if row + 1 >= rows {
        return false;
    }

    let header = text.slice_line(row);
    if is_blank(&header.to_string()) {
        return false;
    }
    let base = tab.indent_count(&header);

    let mut scan = row + 1;
    while scan < rows {
        let line = text.slice_line(scan);
        if is_blank(&line.to_string()) {
            scan += 1;
            continue;
        }
        return tab.indent_count(&line) > base;
    }

    false
}

/// The sorted, de-duplicated union of every fold's hidden rows.
///
/// Nested folds overlap; the union is what the wrapper needs.
pub(super) fn hidden_rows(
    text: &Rope,
    headers: &[usize],
    tab: TabSize,
    supplied: Option<&[FoldRange]>,
) -> Vec<usize> {
    let mut rows: Vec<usize> = headers
        .iter()
        .filter_map(|&row| fold_range_with(text, row, tab, supplied))
        .flatten()
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// The headers left once every fold that **hides** `row` is opened.
///
/// Nested folds come open together: `headers` is the set of closed folds, and
/// a row deep inside three of them is hidden by all three -- so all three have
/// to go. A fold never hides its own header row, only an enclosing fold does,
/// so a header that is merely closed stays closed.
fn headers_showing_row(
    text: &Rope,
    headers: &[usize],
    row: usize,
    tab: TabSize,
    supplied: Option<&[FoldRange]>,
) -> Vec<usize> {
    headers
        .iter()
        .copied()
        .filter(|&header| {
            fold_range_with(text, header, tab, supplied).is_none_or(|r| !r.contains(&row))
        })
        .collect()
}

/// Move fold headers to keep up with an edit.
///
/// - `edit_rows` are the rows the edit touched, **in the old text**.
/// - `delta` is `new_row_count - old_row_count`.
///
/// A header inside the edited region is **dropped**: what it used to head is
/// ambiguous once the user has typed over it. Headers below shift; headers
/// above are untouched.
pub(super) fn adjust_folded_rows(
    rows: &mut Vec<usize>,
    edit_rows: RangeInclusive<usize>,
    delta: isize,
) {
    rows.retain_mut(|row| {
        if *row < *edit_rows.start() {
            true
        } else if *row <= *edit_rows.end() {
            false
        } else {
            match row.checked_add_signed(delta) {
                Some(moved) => {
                    *row = moved;
                    true
                }
                None => false,
            }
        }
    });
}

/// Move supplied ranges to keep up with an edit.
///
/// A range the edit touched is **dropped**: what it used to cover is anyone's
/// guess once the text under it changed. Ranges below shift; ranges above are
/// untouched. Same rule as [`adjust_folded_rows`], applied to both ends.
pub(super) fn adjust_fold_ranges(
    ranges: &mut Vec<FoldRange>,
    edit_rows: RangeInclusive<usize>,
    delta: isize,
) {
    ranges.retain_mut(|r| {
        if r.end_row < *edit_rows.start() {
            return true;
        }
        if r.start_row <= *edit_rows.end() {
            return false;
        }
        match (
            r.start_row.checked_add_signed(delta),
            r.end_row.checked_add_signed(delta),
        ) {
            (Some(start), Some(end)) => {
                r.start_row = start;
                r.end_row = end;
                true
            }
            _ => false,
        }
    });
}

/// Where a caret pushed into a hidden row should come to rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SnapDirection {
    /// To the end of the fold's header row.
    Up,
    /// To the start of the first row after the fold.
    Down,
    /// Whichever edge is nearer.
    Nearest,
}

impl InputState {
    /// Whether `row` is currently folded.
    pub fn is_folded(&self, row: usize) -> bool {
        self.folded_rows.contains(&row)
    }

    /// Whether `row` heads a fold, folded or not.
    pub(super) fn is_foldable(&self, row: usize) -> bool {
        self.mode.has_folding()
            && is_foldable_with(
                &self.text,
                row,
                self.mode.tab_size(),
                self.supplied_folds.as_deref(),
            )
    }

    /// Use these ranges instead of the indentation rule.
    ///
    /// Hands folding over to whoever knows the language -- a language server's
    /// `textDocument/foldingRange`, for instance. Passing an empty list still
    /// takes over (nothing folds); call [`InputState::clear_fold_ranges`] to
    /// go back to indentation.
    pub fn set_fold_ranges(&mut self, ranges: Vec<FoldRange>, cx: &mut Context<Self>) {
        self.supplied_folds = Some(ranges);
        self.update_folds();
        cx.notify();
    }

    /// Go back to reading fold ranges off the indentation.
    pub fn clear_fold_ranges(&mut self, cx: &mut Context<Self>) {
        if self.supplied_folds.take().is_none() {
            return;
        }
        self.update_folds();
        cx.notify();
    }

    /// Fold every supplied range marked [`FoldKind::Imports`].
    ///
    /// Does nothing while folding is off or no ranges have been supplied --
    /// the indentation rule has no notion of an import block.
    pub fn fold_imports(&mut self, cx: &mut Context<Self>) {
        if !self.mode.has_folding() {
            return;
        }
        let Some(ranges) = self.supplied_folds.as_ref() else {
            return;
        };
        let rows: Vec<usize> = ranges
            .iter()
            .filter(|r| r.kind == FoldKind::Imports && r.end_row > r.start_row)
            .map(|r| r.start_row)
            .collect();
        for row in rows {
            if !self.is_folded(row) {
                self.toggle_fold(row, cx);
            }
        }
    }

    /// Fold or unfold the region headed by `row`.
    ///
    /// Does nothing when `row` heads no fold.
    pub fn toggle_fold(&mut self, row: usize, cx: &mut Context<Self>) {
        if !self.mode.has_folding() {
            return;
        }

        if let Some(ix) = self.folded_rows.iter().position(|&r| r == row) {
            self.folded_rows.remove(ix);
        } else {
            // The cap is checked here too, not only in `fold_all`: folding one
            // row at a time must not sneak past it.
            if self.folded_rows.len() >= self.mode.max_fold_regions() {
                return;
            }
            let Some(hidden) = fold_range_with(
                &self.text,
                row,
                self.mode.tab_size(),
                self.supplied_folds.as_deref(),
            ) else {
                return;
            };
            // Never leave the caret inside a region the user cannot see.
            let bytes = self.text.line_start_offset(hidden.start)
                ..self.text.line_end_offset(hidden.end.saturating_sub(1));
            let selected: Range<usize> = self.selected_range.into();
            if selected.start < bytes.end && selected.end > bytes.start {
                let anchor = self.text.line_end_offset(row);
                self.selected_range = (anchor..anchor).into();
                self.preferred_column = None;
            }
            self.folded_rows.push(row);
            self.folded_rows.sort_unstable();
        }

        self.update_folds();
        cx.notify();
    }

    /// The rows that head a fold that is closed right now.
    ///
    /// Sorted, and only the *headers* -- the rows hidden inside a fold are not
    /// listed. Hand the same list back to [`InputState::set_folded_rows`] to
    /// put the editor back the way it was.
    pub fn folded_rows(&self) -> &[usize] {
        &self.folded_rows
    }

    /// Close exactly these folds, opening anything else.
    ///
    /// A row that heads no fold is dropped: the list may have been remembered
    /// from a version of the file that has since been edited, and folding
    /// something else instead would be worse than folding nothing. Rows past
    /// the cap ([`InputMode::max_fold_regions`]) are dropped too.
    ///
    /// Does nothing at all while folding is off, or while no fold ranges have
    /// been supplied yet and the text has none by indentation -- the caller
    /// has to wait for [`InputState::set_fold_ranges`] before a remembered set
    /// can be honoured.
    pub fn set_folded_rows(&mut self, rows: &[usize], cx: &mut Context<Self>) {
        if !self.mode.has_folding() {
            return;
        }
        let tab = self.mode.tab_size();
        let supplied = self.supplied_folds.clone();
        let cap = self.mode.max_fold_regions();
        let mut headers: Vec<usize> = rows
            .iter()
            .copied()
            .filter(|row| {
                *row < self.text.lines_len()
                    && fold_range_with(&self.text, *row, tab, supplied.as_deref()).is_some()
            })
            .take(cap)
            .collect();
        headers.sort_unstable();
        headers.dedup();
        if headers == self.folded_rows {
            return;
        }

        self.folded_rows = headers;
        let cursor = self.cursor();
        self.update_folds();
        // The caret may now be inside a fold; pull it to the nearest edge.
        let snapped = self.snap_out_of_fold(cursor, SnapDirection::Nearest);
        if snapped != cursor {
            self.selected_range = (snapped..snapped).into();
            self.preferred_column = None;
        }
        cx.notify();
    }

    /// Fold every foldable row that is not already inside another fold.
    pub fn fold_all(&mut self, cx: &mut Context<Self>) {
        if !self.mode.has_folding() {
            return;
        }

        let tab = self.mode.tab_size();
        let supplied = self.supplied_folds.clone();
        let cap = self.mode.max_fold_regions();
        let mut headers: Vec<usize> = vec![];
        let mut row = 0;
        while row < self.text.lines_len() && headers.len() < cap {
            if let Some(range) = fold_range_with(&self.text, row, tab, supplied.as_deref()) {
                headers.push(row);
                // Skip the body: the outermost fold already hides it.
                row = range.end;
            } else {
                row += 1;
            }
        }

        self.folded_rows = headers;
        let cursor = self.cursor();
        self.update_folds();
        // The caret may now be inside a fold; pull it to the nearest edge.
        let snapped = self.snap_out_of_fold(cursor, SnapDirection::Nearest);
        if snapped != cursor {
            self.selected_range = (snapped..snapped).into();
            self.preferred_column = None;
        }
        cx.notify();
    }

    /// Open every fold that hides `row`, leaving the rest closed.
    ///
    /// For **jumping into folded code**: a search hit, a definition, a line
    /// the host was asked to reveal. The caret cannot rest on a hidden row --
    /// [`InputState::snap_out_of_fold`] pushes it to the fold's edge -- so a
    /// caller that moves first lands somewhere else and, if it then selects to
    /// the end of what it found, selects the whole fold along the way.
    ///
    /// Call this **before** moving. Returns whether anything opened; the caret
    /// is left exactly where it was, since the caller is about to move it.
    pub fn unfold_row(&mut self, row: usize, cx: &mut Context<Self>) -> bool {
        if self.folded_rows.is_empty() {
            return false;
        }
        let headers = headers_showing_row(
            &self.text,
            &self.folded_rows,
            row,
            self.mode.tab_size(),
            self.supplied_folds.as_deref(),
        );
        if headers.len() == self.folded_rows.len() {
            return false;
        }
        self.folded_rows = headers;
        self.update_folds();
        cx.notify();
        true
    }

    /// Unfold everything.
    pub fn unfold_all(&mut self, cx: &mut Context<Self>) {
        if self.folded_rows.is_empty() {
            return;
        }
        self.folded_rows.clear();
        self.update_folds();
        cx.notify();
    }

    /// Fold or unfold the region the cursor is in the header of.
    ///
    /// Does nothing when the cursor's row heads no fold; walking up to the
    /// enclosing header is a later refinement.
    pub(super) fn on_action_toggle_fold(
        &mut self,
        _: &ToggleFold,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let row = self.text.offset_to_point(self.cursor()).row;
        self.toggle_fold(row, cx);
    }

    pub(super) fn on_action_fold_all(
        &mut self,
        _: &FoldAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fold_all(cx);
    }

    pub(super) fn on_action_unfold_all(
        &mut self,
        _: &UnfoldAll,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.unfold_all(cx);
    }

    /// Recompute which rows are hidden, dropping headers that no longer fold.
    ///
    /// The hidden set is **always derived from the current text**, so the worst
    /// a stale header can do is fold the wrong block -- never hide bytes the
    /// user has no way to reveal.
    pub(super) fn update_folds(&mut self) {
        if !self.mode.has_folding() {
            self.folded_rows.clear();
        }

        let tab = self.mode.tab_size();
        let text = self.text.clone();
        let supplied = self.supplied_folds.clone();
        let s = supplied.as_deref();
        self.folded_rows
            .retain(|&row| is_foldable_with(&text, row, tab, s));
        let rows = hidden_rows(&text, &self.folded_rows, tab, s);
        self.text_wrapper.set_hidden_rows(rows);
    }

    /// Keep the fold headers pointing at the same blocks across an edit.
    pub(super) fn shift_folds_for_edit(&mut self, old_text: &Rope, edited: &Range<usize>) {
        if self.folded_rows.is_empty() && self.supplied_folds.is_none() {
            return;
        }

        let start_row = old_text.offset_to_point(edited.start.min(old_text.len())).row;
        let end_row = old_text.offset_to_point(edited.end.min(old_text.len())).row;
        let delta = self.text.lines_len() as isize - old_text.lines_len() as isize;
        adjust_folded_rows(&mut self.folded_rows, start_row..=end_row, delta);
        // Supplied ranges go stale the same way headers do. Whoever supplied
        // them is expected to ask again; until then the surviving ranges keep
        // pointing at the right blocks.
        if let Some(ranges) = self.supplied_folds.as_mut() {
            adjust_fold_ranges(ranges, start_row..=end_row, delta);
        }
    }

    /// Push an offset that landed on a hidden row out to a visible one.
    ///
    /// Selections are allowed to span a fold -- this only stops the caret from
    /// *resting* inside one.
    pub(super) fn snap_out_of_fold(&self, offset: usize, direction: SnapDirection) -> usize {
        if self.folded_rows.is_empty() {
            return offset;
        }

        let row = self.text.offset_to_point(offset.min(self.text.len())).row;
        let tab = self.mode.tab_size();
        // The innermost fold that hides this row.
        let enclosing = self
            .folded_rows
            .iter()
            .filter_map(|&header| {
                fold_range(&self.text, header, tab)
                    .filter(|range| range.contains(&row))
                    .map(|range| (header, range))
            })
            .max_by_key(|(header, _)| *header);

        let Some((header, range)) = enclosing else {
            return offset;
        };

        let up = self.text.line_end_offset(header);
        let down = if range.end < self.text.lines_len() {
            self.text.line_start_offset(range.end)
        } else {
            self.text.len()
        };

        match direction {
            SnapDirection::Up => up,
            SnapDirection::Down => down,
            SnapDirection::Nearest => {
                if offset.abs_diff(up) <= offset.abs_diff(down) {
                    up
                } else {
                    down
                }
            }
        }
    }
}

/// A chevron placed in the gutter, ready to paint.
pub(super) struct FoldChevron {
    /// Index into `LastLayout::lines`, so paint can reuse the y it already
    /// accumulates for the line numbers.
    pub(super) ix: usize,
    pub(super) line: ShapedLine,
}

impl TextElement {
    /// Shape a chevron for every visible row that heads a fold.
    pub(super) fn layout_fold_chevrons(
        &self,
        state: &InputState,
        last_layout: &LastLayout,
        text_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
        cx: &App,
    ) -> Vec<FoldChevron> {
        if !state.mode.has_folding() {
            return vec![];
        }

        let mut chevrons = vec![];
        for (ix, row) in last_layout.visible_range.clone().enumerate() {
            if !state.is_foldable(row) {
                continue;
            }
            let folded = state.is_folded(row);
            if !state
                .mode
                .folding_controls()
                .shows(row, folded, state.hovered_gutter_row)
            {
                continue;
            }

            // **gpui has no `transform: rotate`** -- swapping the glyph is how
            // the "rotate the chevron by 90 degrees" affordance is expressed.
            let glyph: SharedString = if folded { "\u{203a}" } else { "\u{2304}" }.into();
            let line = window.text_system().shape_line(
                glyph.clone(),
                text_size,
                &[TextRun {
                    len: glyph.len(),
                    font: style.font(),
                    color: cx.theme().muted_foreground,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            chevrons.push(FoldChevron { ix, line });
        }

        chevrons
    }
}

impl TextElement {
    /// Shape a `…` badge for every folded header that is on screen.
    pub(super) fn layout_fold_markers(
        &self,
        state: &InputState,
        last_layout: &LastLayout,
        text_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
        cx: &App,
    ) -> Vec<(usize, ShapedLine)> {
        if !state.mode.has_folding() || state.folded_rows.is_empty() {
            return vec![];
        }

        let glyph: SharedString = "\u{22ef}".into();
        let mut markers = vec![];
        for (ix, row) in last_layout.visible_range.clone().enumerate() {
            if !state.is_folded(row) {
                continue;
            }
            let line = window.text_system().shape_line(
                glyph.clone(),
                text_size,
                &[TextRun {
                    len: glyph.len(),
                    font: style.font(),
                    color: cx.theme().muted_foreground,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            markers.push((ix, line));
        }

        markers
    }
}

impl InputState {
    /// The buffer row under `position`, or `None` when it is past the text.
    ///
    /// Deliberately a separate y-walk from `index_for_mouse_position`: this
    /// runs on gutter clicks only, and keeping it out of that hot path is worth
    /// twenty lines.
    pub(super) fn row_for_mouse_position(&self, position: Point<Pixels>) -> Option<usize> {
        let last_layout = self.last_layout.as_ref()?;
        let bounds = self.last_bounds?;
        let line_height = last_layout.line_height;

        let mut y = bounds.origin.y + last_layout.visible_top;
        for row in last_layout.visible_range.clone() {
            let line = self.text_wrapper.lines.get(row)?;
            let height = line.height(line_height);
            if height > px(0.) && position.y >= y && position.y < y + height {
                return Some(row);
            }
            y += height;
        }

        None
    }

    /// Unfold the row that was clicked past the end of its text.
    ///
    /// Returns `true` when the click was swallowed. Only ever fires on a
    /// **folded** row, and only to the right of the text -- clicking the text
    /// itself still places the caret.
    ///
    /// `closest_index_for_x` clamps anything past the line to the line end, so
    /// nothing downstream can tell "on the last glyph" from "400px past it".
    /// The x test has to happen here.
    pub(super) fn handle_click_after_end_of_line(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mode.has_folding() || !self.mode.unfold_on_click_after_end_of_line() {
            return false;
        }
        if event.button != MouseButton::Left || event.click_count != 1 {
            return false;
        }
        let Some(row) = self.row_for_mouse_position(event.position) else {
            return false;
        };
        if !self.is_folded(row) {
            return false;
        }
        let (Some(last_layout), Some(bounds)) = (self.last_layout.as_ref(), self.last_bounds)
        else {
            return false;
        };
        let Some(width) = last_layout
            .line(row)
            .and_then(|l| l.wrapped_lines.last())
            .map(|l| l.width)
        else {
            return false;
        };
        // `last_bounds` carries the scroll offset, which is what the text was
        // drawn against.
        let text_end = bounds.origin.x + last_layout.line_number_width + width;
        if event.position.x <= text_end {
            return false;
        }
        self.toggle_fold(row, cx);
        cx.stop_propagation();
        true
    }

    /// Remember which gutter row the pointer is over, for
    /// [`FoldingControls::MouseOver`].
    ///
    /// **Only notifies when the row actually changes** -- mouse-move fires
    /// constantly, and redrawing on every one of them would be a frame tax for
    /// nothing.
    pub(super) fn track_fold_gutter_hover(
        &mut self,
        position: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.has_folding()
            || self.mode.folding_controls() == crate::input::FoldingControls::Always
        {
            return;
        }

        let row = self
            .in_fold_gutter(position)
            .then(|| self.row_for_mouse_position(position))
            .flatten();
        if row != self.hovered_gutter_row {
            self.hovered_gutter_row = row;
            cx.notify();
        }
    }

    /// Whether `position` is inside the chevron strip.
    fn in_fold_gutter(&self, position: Point<Pixels>) -> bool {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return false;
        };
        // **`input_bounds`, not `last_bounds`** -- the latter is shifted by the
        // horizontal scroll offset, and the gutter does not scroll with it.
        //
        // The strip has to match where the chevron is *painted*, which is
        // inside `LINE_NUMBER_RIGHT_MARGIN`, not flush against the text.
        let left = crate::input::element::fold_chevron_x(
            self.input_bounds.origin.x,
            last_layout.line_number_width,
        );
        position.x >= left && position.x < left + crate::input::element::FOLD_CHEVRON_WIDTH
    }

    /// Handle a click on the fold chevron strip.
    ///
    /// Returns true when the click was consumed, so the caller can return
    /// before moving the caret or starting a drag-selection.
    pub(super) fn handle_fold_gutter_click(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.mode.has_folding() || event.button != MouseButton::Left {
            return false;
        }

        if !self.in_fold_gutter(event.position) {
            return false;
        }

        let Some(row) = self.row_for_mouse_position(event.position) else {
            return false;
        };
        if !self.is_foldable(row) {
            // Still inside the strip: swallow it so the caret does not jump to
            // the start of the line the user was aiming a chevron at.
            return true;
        }

        self.toggle_fold(row, cx);
        cx.stop_propagation();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab() -> TabSize {
        TabSize {
            tab_size: 4,
            hard_tabs: false,
    indent_size: 0,
        }
    }

    const NESTED: &str = "\
fn main() {
    if x {
        a();
    }
}
";

    #[test]
    fn folds_a_nested_block() {
        let text = Rope::from(NESTED);
        // `fn main() {` hides rows 1..=3 (everything indented under it).
        assert_eq!(fold_range(&text, 0, tab()), Some(1..4));
        // `if x {` hides only its own body.
        assert_eq!(fold_range(&text, 1, tab()), Some(2..3));
        // `a();` heads nothing.
        assert_eq!(fold_range(&text, 2, tab()), None);
        // The closing braces head nothing.
        assert_eq!(fold_range(&text, 3, tab()), None);
        assert_eq!(fold_range(&text, 4, tab()), None);
    }

    #[test]
    fn the_last_row_never_folds() {
        let text = Rope::from("a\n    b\n");
        // The final (empty) row after the trailing newline.
        assert_eq!(fold_range(&text, text.lines_len() - 1, tab()), None);
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
    }

    #[test]
    fn blank_lines_are_absorbed_but_do_not_extend_the_fold() {
        // Rows: 0 `a:`, 1 `    x`, 2 ``, 3 `    y`, 4 ``, 5 `b:`
        let text = Rope::from("a:\n    x\n\n    y\n\nb:\n");
        // The blank at 2 is absorbed (4 is not, it comes after the last deeper
        // line), so the fold stops at row 3.
        assert_eq!(fold_range(&text, 0, tab()), Some(1..4));
    }

    #[test]
    fn a_blank_header_folds_nothing() {
        let text = Rope::from("\n    a\n");
        assert_eq!(fold_range(&text, 0, tab()), None);
    }

    #[test]
    fn hard_tabs_and_spaces_agree() {
        // `\t` counts as `tab_size`, so a tab-indented body is deeper than a
        // space-indented header of 2.
        let text = Rope::from("a\n\tb\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
        let text = Rope::from("\ta\n\t\tb\n\tc\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
    }

    #[test]
    fn a_dedent_ends_the_fold() {
        // `} else {` dedents back to the header's level, so the first block
        // ends there.
        let text = Rope::from("if a {\n    x();\n} else {\n    y();\n}\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
        assert_eq!(fold_range(&text, 2, tab()), Some(3..4));
    }

    #[test]
    fn a_trailing_indented_block_folds_to_the_end() {
        let text = Rope::from("a:\n    x\n    y\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..3));
    }

    fn ranges() -> Vec<FoldRange> {
        vec![
            FoldRange { start_row: 0, end_row: 2, kind: FoldKind::Imports },
            FoldRange { start_row: 4, end_row: 9, kind: FoldKind::Region },
            // Degenerate: a server may report a one-line "range".
            FoldRange { start_row: 11, end_row: 11, kind: FoldKind::Comment },
        ]
    }

    /// Supplied ranges take over completely -- indentation is not mixed in.
    #[test]
    fn supplied_ranges_replace_the_indentation_rule() {
        let text = Rope::from(NESTED);
        let r = ranges();
        let s = Some(r.as_slice());

        assert_eq!(fold_range_with(&text, 0, tab(), s), Some(1..3));
        assert_eq!(fold_range_with(&text, 4, tab(), s), Some(5..10));
        // A row the server did not name does not fold, however it is indented.
        assert_eq!(fold_range_with(&text, 1, tab(), s), None);
        // start == end is not a fold.
        assert_eq!(fold_range_with(&text, 11, tab(), s), None);
        // No table: the indentation rule, unchanged.
        assert_eq!(
            fold_range_with(&text, 0, tab(), None),
            fold_range(&text, 0, tab())
        );
    }

    /// The two entry points must agree for supplied ranges too.
    #[test]
    fn is_foldable_with_agrees_with_fold_range_with() {
        let text = Rope::from(NESTED);
        let r = ranges();
        let s = Some(r.as_slice());
        for row in 0..text.lines_len() {
            assert_eq!(
                is_foldable_with(&text, row, tab(), s),
                fold_range_with(&text, row, tab(), s).is_some(),
                "row {row}"
            );
        }
    }

    /// An edit drops the ranges it touched and shifts the ones below.
    #[test]
    fn supplied_ranges_move_with_an_edit() {
        let mut r = ranges();
        // Two rows inserted above everything.
        adjust_fold_ranges(&mut r, 0..=0, 2);
        assert_eq!(
            r,
            vec![
                FoldRange { start_row: 6, end_row: 11, kind: FoldKind::Region },
                FoldRange { start_row: 13, end_row: 13, kind: FoldKind::Comment },
            ],
            "the range the edit landed in is dropped, the rest shift"
        );

        let mut r = ranges();
        // An edit below everything changes nothing.
        adjust_fold_ranges(&mut r, 20..=20, 1);
        assert_eq!(r, ranges());
    }

    /// `fold_all` stops at the cap, and so does folding one row at a time.
    ///
    /// Capping only `fold_all` would let anyone walk past it with the mouse.
    #[test]
    fn the_cap_holds_for_both_ways_of_folding() {
        // Three sibling blocks, each with a body.
        let src = "a:\n    x\nb:\n    y\nc:\n    z\n";
        let text = Rope::from(src);
        let mut headers: Vec<usize> = vec![];
        let cap = 2;
        let mut row = 0;
        while row < text.lines_len() && headers.len() < cap {
            if let Some(range) = fold_range_with(&text, row, tab(), None) {
                headers.push(row);
                row = range.end;
            } else {
                row += 1;
            }
        }
        assert_eq!(headers, vec![0, 2], "3 つ目で止まる");

        // Every header would fold without the cap.
        assert!(fold_range_with(&text, 4, tab(), None).is_some());
    }

    /// The cheap predicate must never disagree with the real computation.
    #[test]
    fn is_foldable_agrees_with_fold_range() {
        for src in [
            NESTED,
            "a:\n    x\n\n    y\n\nb:\n",
            "if a {\n    x();\n} else {\n    y();\n}\n",
            "\n    a\n",
            "one\ntwo\nthree\n",
            "\ta\n\t\tb\n\tc\n",
        ] {
            let text = Rope::from(src);
            for row in 0..text.lines_len() {
                assert_eq!(
                    is_foldable(&text, row, tab()),
                    fold_range(&text, row, tab()).is_some(),
                    "row {row} of {src:?}"
                );
            }
        }
    }

    #[test]
    fn hidden_rows_unions_nested_folds() {
        let text = Rope::from(NESTED);
        // Both the outer and the inner fold: the union counts row 2 once.
        assert_eq!(hidden_rows(&text, &[0, 1], tab(), None), vec![1, 2, 3]);
        assert_eq!(hidden_rows(&text, &[1], tab(), None), vec![2]);
        // A header that no longer folds contributes nothing.
        assert_eq!(hidden_rows(&text, &[2], tab(), None), Vec::<usize>::new());
    }

    /// Jumping into a nest opens the whole nest, not just the outermost fold.
    #[test]
    fn opening_a_row_opens_every_fold_over_it() {
        let text = Rope::from(NESTED);
        // Row 2 (`a();`) sits inside both `fn main() {` and `if x {`.
        assert_eq!(headers_showing_row(&text, &[0, 1], 2, tab(), None), Vec::<usize>::new());
        // Row 3 (`}`) is inside the outer fold only; the inner one stays shut.
        assert_eq!(headers_showing_row(&text, &[0, 1], 3, tab(), None), vec![1]);
    }

    /// A fold does not hide its own header, so a closed header stays closed.
    #[test]
    fn a_header_row_is_not_hidden_by_its_own_fold() {
        let text = Rope::from(NESTED);
        // Row 1 heads the inner fold and is hidden by the outer one: only the
        // outer fold opens.
        assert_eq!(headers_showing_row(&text, &[0, 1], 1, tab(), None), vec![1]);
        // Row 0 heads the outer fold and nothing hides it.
        assert_eq!(headers_showing_row(&text, &[0, 1], 0, tab(), None), vec![0, 1]);
    }

    /// Rows nothing hides -- and headers that no longer fold -- are untouched.
    #[test]
    fn opening_leaves_unrelated_folds_alone() {
        let text = Rope::from(NESTED);
        // Row 4 (the last `}`) is outside every fold.
        assert_eq!(headers_showing_row(&text, &[0, 1], 4, tab(), None), vec![0, 1]);
        // A stale header folds nothing, so it hides nothing either. It is kept
        // as-is; `update_folds` is what drops it.
        assert_eq!(headers_showing_row(&text, &[2], 3, tab(), None), vec![2]);
        assert_eq!(headers_showing_row(&text, &[], 2, tab(), None), Vec::<usize>::new());
    }

    /// Supplied ranges decide it when a server has taken folding over.
    #[test]
    fn opening_reads_supplied_ranges_too() {
        let text = Rope::from(NESTED);
        // `0..=2` by the table -- rows 1..=2, so row 3 is *not* inside it even
        // though the indentation rule would hide it.
        let r = vec![FoldRange { start_row: 0, end_row: 2, kind: FoldKind::Region }];
        let s = Some(r.as_slice());
        assert_eq!(headers_showing_row(&text, &[0], 2, tab(), s), Vec::<usize>::new());
        assert_eq!(headers_showing_row(&text, &[0], 3, tab(), s), vec![0]);
    }

    #[test]
    fn adjust_shifts_headers_below_the_edit() {
        let mut rows = vec![1, 5, 9];
        // Two lines inserted at row 4.
        adjust_folded_rows(&mut rows, 4..=4, 2);
        assert_eq!(rows, vec![1, 7, 11]);

        let mut rows = vec![1, 5, 9];
        // Rows 3..=6 replaced by one line.
        adjust_folded_rows(&mut rows, 3..=6, -3);
        assert_eq!(rows, vec![1, 6], "5 was inside the edit and is dropped");

        let mut rows = vec![2];
        // An edit on the header's own row drops it.
        adjust_folded_rows(&mut rows, 2..=2, 0);
        assert!(rows.is_empty());

        let mut rows = vec![1];
        // Underflow drops rather than wrapping.
        adjust_folded_rows(&mut rows, 0..=0, -5);
        assert!(rows.is_empty());
    }
}
