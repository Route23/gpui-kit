//! More than one caret (dopamine #390).
//!
//! **The primary selection stays `selected_range`.** The others live in
//! [`InputState::extra_selections`]. Nothing that edits text learned about
//! them: an action is run **once per caret**, last caret first, with
//! `selected_range` pointing at that caret, and the carets after it are shifted
//! by however much the text grew or shrank. Every one of those runs is a normal
//! single edit, so the text wrapper, folds, highlighter and `didChange` keep
//! seeing the one-edit-at-a-time world they were written for. The runs are
//! grouped into one undo step.

use std::ops::Range;

use gpui::{Context, Window};

use super::state::InputState;
use super::{RopeExt as _, Selection};

/// How the carets behave (VS Code `editor.multiCursor*`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MultiCursorOptions {
    /// `true`: ⌥-click adds a caret (VS Code `alt`). `false`: ⌘-click (`ctrlCmd`).
    pub alt_adds: bool,
    /// Paste one clipboard line per caret when the counts match (`spread`), or the
    /// whole clipboard at every caret (`full`).
    pub paste_spread: bool,
    /// Merge carets whose selections touch (`multiCursorMergeOverlapping`).
    pub merge_overlapping: bool,
    /// At most this many carets (`multiCursorLimit`).
    pub limit: usize,
    /// Drag a selection to move it (`editor.dragAndDrop`).
    pub drag_and_drop: bool,
}

impl Default for MultiCursorOptions {
    fn default() -> Self {
        Self { alt_adds: true, paste_spread: true, merge_overlapping: true, limit: 10_000, drag_and_drop: false }
    }
}

/// A selection being dragged to a new place (`editor.dragAndDrop`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SelectionDrag {
    pub range: Range<usize>,
    pub drop: Option<usize>,
}

impl InputState {
    pub fn set_multi_cursor_options(&mut self, opts: MultiCursorOptions, cx: &mut Context<Self>) {
        if self.multi_cursor != opts {
            self.multi_cursor = opts;
            cx.notify();
        }
    }

    /// How many carets besides the primary one.
    pub fn extra_cursor_count(&self) -> usize {
        self.extra_selections.len()
    }

    /// Back to one caret.
    pub fn clear_extra_cursors(&mut self, cx: &mut Context<Self>) {
        if !self.extra_selections.is_empty() {
            self.extra_selections.clear();
            cx.notify();
        }
    }

    /// All carets, primary first.
    pub(super) fn all_selections(&self) -> Vec<Selection> {
        std::iter::once(self.selected_range).chain(self.extra_selections.iter().copied()).collect()
    }

    /// Add a caret (an empty selection at `offset`) and make it the primary.
    pub(super) fn add_caret(&mut self, offset: usize, cx: &mut Context<Self>) {
        let offset = offset.min(self.text.len());
        if self.extra_selections.len() + 1 >= self.multi_cursor.limit {
            return;
        }
        let old = self.selected_range;
        // Clicking an existing caret removes it instead.
        if let Some(i) = self.extra_selections.iter().position(|s| s.start <= offset && offset <= s.end) {
            self.extra_selections.remove(i);
            cx.notify();
            return;
        }
        self.extra_selections.push(old);
        self.selected_range = (offset..offset).into();
        self.selection_reversed = false;
        self.normalize_cursors();
        cx.notify();
    }

    /// Run `f` once per caret (see the module docs). With one caret this is just `f`.
    pub(super) fn each_cursor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        mut f: impl FnMut(&mut Self, &mut Window, &mut Context<Self>),
    ) {
        if self.extra_selections.is_empty() {
            f(self, window, cx);
            return;
        }
        let mut all: Vec<(Selection, bool)> = std::iter::once((self.selected_range, true))
            .chain(self.extra_selections.drain(..).map(|s| (s, false)))
            .collect();
        all.sort_by(|a, b| b.0.start.cmp(&a.0.start));
        self.history.start_grouping();
        let mut done: Vec<(Selection, bool)> = Vec::with_capacity(all.len());
        for (sel, primary) in all {
            let before = self.text.len() as isize;
            self.selected_range = sel;
            self.selection_reversed = false;
            f(self, window, cx);
            let delta = self.text.len() as isize - before;
            if delta != 0 {
                for (d, _) in done.iter_mut() {
                    // The carets already done sit after this one.
                    if d.start >= sel.start {
                        d.start = (d.start as isize + delta).max(0) as usize;
                        d.end = (d.end as isize + delta).max(0) as usize;
                    }
                }
            }
            done.push((self.selected_range, primary));
        }
        self.history.end_grouping();
        let len = self.text.len();
        let primary = done.iter().find(|(_, p)| *p).map(|(s, _)| *s).unwrap_or_default();
        self.selected_range = (primary.start.min(len)..primary.end.min(len)).into();
        self.extra_selections = done
            .into_iter()
            .filter(|(_, p)| !*p)
            .map(|(s, _)| (s.start.min(len)..s.end.min(len)).into())
            .collect();
        self.normalize_cursors();
        cx.notify();
    }

    /// [`Self::each_cursor`] for an action handler.
    pub(super) fn each_action<A: Clone + 'static>(
        &mut self,
        action: &A,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: fn(&mut Self, &A, &mut Window, &mut Context<Self>),
    ) {
        let a = action.clone();
        self.each_cursor(window, cx, |s, w, cx| f(s, &a, w, cx));
    }

    /// Drop duplicates and (when asked) merge touching carets.
    pub(super) fn normalize_cursors(&mut self) {
        let primary = self.selected_range;
        let mut extras: Vec<Selection> = std::mem::take(&mut self.extra_selections);
        extras.retain(|s| *s != primary);
        extras.sort_by_key(|s| s.start);
        extras.dedup();
        if self.multi_cursor.merge_overlapping {
            let overlaps = |a: &Selection, b: &Selection| a.start <= b.end && b.start <= a.end && !(a.is_empty() && b.is_empty() && a.start != b.start);
            extras.retain(|s| !overlaps(s, &primary) || (s.is_empty() && primary.is_empty() && s.start != primary.start));
            let mut merged: Vec<Selection> = Vec::with_capacity(extras.len());
            for s in extras {
                match merged.last_mut() {
                    Some(m) if overlaps(m, &s) => m.end = m.end.max(s.end),
                    _ => merged.push(s),
                }
            }
            extras = merged;
        }
        self.extra_selections = extras;
    }

    /// ⌘D: select the word under the caret, or add the next match of the selection.
    pub(super) fn add_next_occurrence(&mut self, cx: &mut Context<Self>) {
        let sel = self.selected_range;
        if sel.is_empty() {
            if let Some(w) = self.word_at_offset(sel.start) {
                self.selected_range = w.into();
                cx.notify();
            }
            return;
        }
        let needle = self.text.slice(sel.start..sel.end).to_string();
        if needle.is_empty() {
            return;
        }
        let hay = self.text.to_string();
        let taken: Vec<Selection> = self.all_selections();
        let from = taken.iter().map(|s| s.end).max().unwrap_or(sel.end);
        let next = hay[from.min(hay.len())..]
            .find(&needle)
            .map(|i| from + i)
            .or_else(|| hay.find(&needle))
            .filter(|&i| !taken.iter().any(|s| s.start == i));
        if let Some(i) = next {
            if self.extra_selections.len() + 1 >= self.multi_cursor.limit {
                return;
            }
            self.extra_selections.push(sel);
            self.selected_range = (i..i + needle.len()).into();
            self.normalize_cursors();
            cx.notify();
        }
    }

    /// ⌘⇧L: a caret on every match of the selection (or the word under the caret).
    pub(super) fn select_all_occurrences(&mut self, cx: &mut Context<Self>) {
        let mut sel = self.selected_range;
        if sel.is_empty() {
            match self.word_at_offset(sel.start) {
                Some(w) => sel = w.into(),
                None => return,
            }
        }
        let needle = self.text.slice(sel.start..sel.end).to_string();
        if needle.is_empty() {
            return;
        }
        let hay = self.text.to_string();
        let mut found: Vec<Selection> = hay.match_indices(&needle).map(|(i, _)| (i..i + needle.len()).into()).collect();
        found.truncate(self.multi_cursor.limit.max(1));
        self.selected_range = sel;
        self.extra_selections = found.into_iter().filter(|s| *s != sel).collect();
        self.normalize_cursors();
        cx.notify();
    }

    /// ⌃⇧↑ / ⌃⇧↓: a caret on the line above / below at the same column.
    pub(super) fn add_caret_vertically(&mut self, down: bool, cx: &mut Context<Self>) {
        let carets: Vec<usize> = self.all_selections().iter().map(|s| s.end).collect();
        let edge = if down { carets.iter().copied().max() } else { carets.iter().copied().min() };
        let Some(at) = edge else { return };
        let p = self.text.offset_to_point(at);
        let rows = self.text.lines_len();
        let row = if down { p.row + 1 } else { match p.row.checked_sub(1) { Some(r) => r, None => return } };
        if row >= rows {
            return;
        }
        let start = self.text.line_start_offset(row);
        let end = if row + 1 < rows { self.text.line_start_offset(row + 1).saturating_sub(1) } else { self.text.len() };
        let offset = (start + p.column).min(end);
        if self.extra_selections.len() + 1 >= self.multi_cursor.limit {
            return;
        }
        self.extra_selections.push(self.selected_range);
        self.selected_range = (offset..offset).into();
        self.normalize_cursors();
        cx.notify();
    }

    /// ⇧⌥-drag: one selection per row between `anchor` and `offset`, from the
    /// anchor's column to the pointer's (VS Code column selection).
    pub(super) fn box_select(&mut self, anchor: usize, offset: usize, cx: &mut Context<Self>) {
        let a = self.text.offset_to_point(anchor.min(self.text.len()));
        let b = self.text.offset_to_point(offset.min(self.text.len()));
        let (r0, r1) = (a.row.min(b.row), a.row.max(b.row));
        let (c0, c1) = (a.column.min(b.column), a.column.max(b.column));
        let rows = self.text.lines_len();
        let mut sels: Vec<Selection> = Vec::new();
        for row in r0..=r1.min(rows.saturating_sub(1)) {
            let start = self.text.line_start_offset(row);
            let end = if row + 1 < rows { self.text.line_start_offset(row + 1).saturating_sub(1) } else { self.text.len() };
            let len = end - start;
            if c0 > len && c0 != c1 {
                continue;
            }
            let (s, e) = (start + c0.min(len), start + c1.min(len));
            let s = self.text.clip_offset(s, sum_tree::Bias::Left);
            let e = self.text.clip_offset(e, sum_tree::Bias::Left);
            sels.push((s..e).into());
        }
        sels.truncate(self.multi_cursor.limit.max(1));
        let Some(primary) = sels.pop() else { return };
        self.selected_range = primary;
        self.selection_reversed = false;
        self.extra_selections = sels;
        cx.notify();
    }

    /// Paste with several carets: one line each when the counts match and
    /// `paste_spread` is on, otherwise the whole text at every caret.
    pub(super) fn paste_multi(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.extra_selections.len() + 1;
        let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
        if self.multi_cursor.paste_spread && lines.len() == n {
            // In document order: the first line goes to the first caret.
            let mut order: Vec<Selection> = self.all_selections();
            order.sort_by_key(|s| s.start);
            let pieces: Vec<(Selection, String)> = order.into_iter().zip(lines.iter().map(|l| l.to_string())).collect();
            self.each_cursor(window, cx, |s, w, cx| {
                let at = s.selected_range;
                if let Some((_, piece)) = pieces.iter().find(|(sel, _)| sel.start == at.start) {
                    s.replace_text_in_range_silent(None, piece, w, cx);
                }
            });
            // `each_cursor` shifts the selections it has done but `pieces` keeps
            // the originals: carets are matched before any edit moves them.
        } else {
            let t = text.to_string();
            self.each_cursor(window, cx, |s, w, cx| s.replace_text_in_range_silent(None, &t, w, cx));
        }
    }

    /// The text a selection drag would move, if `offset` is inside the selection
    /// and dragging is on.
    pub(super) fn selection_drag_start(&self, offset: usize) -> Option<SelectionDrag> {
        let sel = self.selected_range;
        (self.multi_cursor.drag_and_drop && !sel.is_empty() && sel.start < offset && offset < sel.end && self.extra_selections.is_empty())
            .then(|| SelectionDrag { range: sel.start..sel.end, drop: None })
    }

    /// Drop the dragged selection at `offset` (one undo step). Dropping inside the
    /// selection itself does nothing.
    pub(super) fn finish_selection_drag(&mut self, drag: SelectionDrag, window: &mut Window, cx: &mut Context<Self>) {
        let Some(drop) = drag.drop else { return };
        let r = drag.range;
        if drop >= r.start && drop <= r.end {
            self.selected_range = (drop..drop).into();
            cx.notify();
            return;
        }
        let moved = self.text.slice(r.clone()).to_string();
        self.history.start_grouping();
        // Insert first when the drop is before the text, so the range stays valid.
        if drop < r.start {
            self.selected_range = (r.start..r.end).into();
            self.replace_text_in_range_silent(None, "", window, cx);
            self.selected_range = (drop..drop).into();
            self.replace_text_in_range_silent(None, &moved, window, cx);
            self.selected_range = (drop..drop + moved.len()).into();
        } else {
            self.selected_range = (drop..drop).into();
            self.replace_text_in_range_silent(None, &moved, window, cx);
            self.selected_range = (r.start..r.end).into();
            self.replace_text_in_range_silent(None, "", window, cx);
            let at = drop - moved.len();
            self.selected_range = (at..at + moved.len()).into();
        }
        self.history.end_grouping();
        cx.notify();
    }
}
