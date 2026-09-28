//! Breakpoint dots in the gutter -- the red circle VS Code and Zed draw left of
//! the line numbers (dopamine #278).
//!
//! **Meaning stays with the caller.** There is no debugger here: this module is
//! handed rows, draws a dot on each, and says which row was clicked
//! ([`super::InputEvent::BreakpointClicked`]). Edits shift the rows the same
//! way fold headers shift ([`super::fold::adjust_folded_rows`]), and the new
//! list is reported ([`super::InputEvent::BreakpointsMoved`]) so the caller can
//! keep its copy in step.
//!
//! The column only exists while [`InputState::set_breakpoint_gutter`] is on;
//! off, the gutter is exactly as wide as before.

use std::ops::Range;

use gpui::{px, Context, MouseButton, MouseDownEvent, Pixels, Point};
use ropey::Rope;

use super::state::InputState;
use super::RopeExt as _;

/// The column the dots sit in, left of the line numbers.
pub(super) const BREAKPOINT_COLUMN_WIDTH: Pixels = px(14.);
/// Where the column starts: clear of the change-mark bar on the left edge.
pub(super) const BREAKPOINT_COLUMN_X: Pixels = px(3.);
/// The dot's diameter.
pub(super) const BREAKPOINT_DOT: Pixels = px(9.);

/// How a dot is drawn (dopamine #278). The caller decides what each means.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum BreakpointKind {
    /// A filled dot.
    #[default]
    Normal,
    /// A dot with a bar through it (stops only when a condition holds).
    Conditional,
    /// A diamond (writes a message instead of stopping).
    Log,
    /// A ring in the muted colour (kept, but does nothing).
    Disabled,
}

/// The width the column takes out of the gutter, 0 when it is off.
pub(super) fn breakpoint_column_width(state: &InputState) -> Pixels {
    if state.breakpoint_gutter && state.mode.line_number() {
        BREAKPOINT_COLUMN_X + BREAKPOINT_COLUMN_WIDTH
    } else {
        px(0.)
    }
}

impl InputState {
    /// Show the breakpoint column. Off (the default) leaves the gutter as it was.
    pub fn set_breakpoint_gutter(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.breakpoint_gutter == on {
            return;
        }
        self.breakpoint_gutter = on;
        self.hovered_breakpoint_row = None;
        cx.notify();
    }

    /// Draw a dot on these rows (0-based). An empty list clears them.
    pub fn set_breakpoints(&mut self, mut rows: Vec<usize>, cx: &mut Context<Self>) {
        rows.sort_unstable();
        rows.dedup();
        if self.breakpoints == rows {
            return;
        }
        self.breakpoints = rows;
        cx.notify();
    }

    /// How to draw particular dots; rows not listed are [`BreakpointKind::Normal`].
    /// Rows are the caller's: after [`super::InputEvent::BreakpointsMoved`] it is
    /// expected to send them again.
    pub fn set_breakpoint_kinds(&mut self, kinds: Vec<(usize, BreakpointKind)>, cx: &mut Context<Self>) {
        let kinds: std::collections::HashMap<usize, BreakpointKind> = kinds.into_iter().collect();
        if self.breakpoint_kinds == kinds {
            return;
        }
        self.breakpoint_kinds = kinds;
        cx.notify();
    }

    pub(super) fn breakpoint_kind(&self, row: usize) -> BreakpointKind {
        self.breakpoint_kinds.get(&row).copied().unwrap_or_default()
    }

    /// The rows that carry a dot, after any edits since they were set.
    pub fn breakpoints(&self) -> &[usize] {
        &self.breakpoints
    }

    pub(super) fn has_breakpoint(&self, row: usize) -> bool {
        self.breakpoints.binary_search(&row).is_ok()
    }

    /// Whether `position` is over the column.
    fn in_breakpoint_column(&self, position: Point<Pixels>) -> bool {
        if breakpoint_column_width(self) == px(0.) {
            return false;
        }
        // `input_bounds`, not `last_bounds`: the gutter does not scroll sideways.
        let left = self.input_bounds.origin.x + BREAKPOINT_COLUMN_X;
        position.x >= left && position.x < left + BREAKPOINT_COLUMN_WIDTH
    }

    /// A left click in the column: report the row -- **with or without a dot**,
    /// since that is how one is added -- and keep the caret where it is.
    pub(super) fn handle_breakpoint_click(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        let right = event.button == MouseButton::Right;
        if !(event.button == MouseButton::Left || right) || !self.in_breakpoint_column(event.position) {
            return false;
        }
        let Some(row) = self.row_for_mouse_position(event.position) else {
            return false;
        };
        // A right click asks the host for a menu (condition, log message,
        // disable) instead of toggling.
        if right {
            cx.emit(super::InputEvent::BreakpointContextMenu { row, position: event.position });
        } else {
            cx.emit(super::InputEvent::BreakpointClicked { row });
        }
        cx.stop_propagation();
        true
    }

    /// The faint dot that says "click here" follows the pointer.
    pub(super) fn track_breakpoint_hover(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let row = self
            .in_breakpoint_column(position)
            .then(|| self.row_for_mouse_position(position))
            .flatten();
        if row != self.hovered_breakpoint_row {
            self.hovered_breakpoint_row = row;
            cx.notify();
        }
    }

    /// Keep the dots on the same lines across an edit.
    pub(super) fn shift_breakpoints_for_edit(
        &mut self,
        old_text: &Rope,
        edited: &Range<usize>,
        cx: &mut Context<Self>,
    ) {
        if self.breakpoints.is_empty() {
            return;
        }
        let start_row = old_text.offset_to_point(edited.start.min(old_text.len())).row;
        let end_row = old_text.offset_to_point(edited.end.min(old_text.len())).row;
        let delta = self.text.lines_len() as isize - old_text.lines_len() as isize;
        let pushes_first_row = edited.is_empty()
            && old_text.offset_to_point(edited.start.min(old_text.len())).column == 0;
        let before = self.breakpoints.clone();
        adjust_breakpoints(&mut self.breakpoints, start_row..=end_row, delta, pushes_first_row);
        if self.breakpoints != before {
            cx.emit(super::InputEvent::BreakpointsMoved {
                rows: self.breakpoints.clone(),
            });
        }
    }
}

/// Move dots to keep up with an edit.
///
/// - `edit_rows` are the rows the edit touched, **in the old text**.
/// - `delta` is `new_row_count - old_row_count`.
/// - `pushes_first_row`: the edit only inserted, at the very start of its row
///   -- Enter at column 0 carries the line (and its dot) down, as in VS Code.
///
/// Unlike a fold header, a dot on an edited row **stays**: typing on a line
/// must not remove its breakpoint. Dots on rows an edit removed land on the
/// last row the edit left; dots below shift.
pub(super) fn adjust_breakpoints(
    rows: &mut Vec<usize>,
    edit_rows: std::ops::RangeInclusive<usize>,
    delta: isize,
    pushes_first_row: bool,
) {
    if delta == 0 {
        return;
    }
    let (start, end) = (*edit_rows.start(), *edit_rows.end());
    let last_new = end.saturating_add_signed(delta).max(start);
    for row in rows.iter_mut() {
        if *row > end || (pushes_first_row && *row == start) {
            *row = row.saturating_add_signed(delta);
        } else if *row > start {
            *row = (*row).min(last_new);
        }
    }
    rows.sort_unstable();
    rows.dedup();
}

#[cfg(test)]
mod tests {
    use super::adjust_breakpoints;

    #[test]
    fn rows_below_an_insert_move_down() {
        let mut r = vec![1, 5, 9];
        adjust_breakpoints(&mut r, 3..=3, 2, false);
        assert_eq!(r, vec![1, 7, 11]);
    }

    #[test]
    fn typing_on_the_row_keeps_the_dot() {
        let mut r = vec![4];
        adjust_breakpoints(&mut r, 4..=4, 0, false);
        assert_eq!(r, vec![4]);
    }

    #[test]
    fn joined_rows_fold_onto_the_first() {
        let mut r = vec![2, 4, 5, 8];
        // Rows 3..=5 deleted up into row 2's end: old rows 2..=5 become row 2.
        adjust_breakpoints(&mut r, 2..=5, -3, false);
        assert_eq!(r, vec![2, 5]);
    }

    #[test]
    fn enter_at_the_start_of_a_row_carries_its_dot_down() {
        let mut r = vec![4];
        adjust_breakpoints(&mut r, 4..=4, 1, true);
        assert_eq!(r, vec![5]);
        let mut r = vec![4];
        adjust_breakpoints(&mut r, 4..=4, 1, false);
        assert_eq!(r, vec![4]);
    }
}
