//! "Run" triangles in the gutter -- Zed's `gutter.runnables` (dopamine #496).
//!
//! **Meaning stays with the caller.** This module is handed rows, draws a ▶ on
//! each, and says which row was clicked ([`super::InputEvent::RunnableClicked`]).
//! The rows are the caller's: it sends them again after edits.
//!
//! The column only exists while [`InputState::set_runnable_gutter`] is on; off,
//! the gutter is exactly as wide as before. It sits right of the breakpoint
//! column (when that is on) and left of the line numbers.

use gpui::{px, Context, MouseButton, MouseDownEvent, Pixels, Point};

use super::state::InputState;

/// The column's width.
pub(super) const RUNNABLE_COLUMN_WIDTH: Pixels = px(14.);
/// The triangle's height.
pub(super) const RUNNABLE_TRIANGLE: Pixels = px(9.);

/// Where the column starts, from the gutter's left edge.
pub(super) fn runnable_column_x(state: &InputState) -> Pixels {
    let bp = super::breakpoints::breakpoint_column_width(state);
    if bp > px(0.) { bp } else { super::breakpoints::BREAKPOINT_COLUMN_X }
}

/// The width the column takes out of the gutter, 0 when it is off.
pub(super) fn runnable_column_width(state: &InputState) -> Pixels {
    if state.runnable_gutter && state.mode.line_number() {
        let lead = if super::breakpoints::breakpoint_column_width(state) > px(0.) {
            px(0.)
        } else {
            super::breakpoints::BREAKPOINT_COLUMN_X
        };
        lead + RUNNABLE_COLUMN_WIDTH
    } else {
        px(0.)
    }
}

impl InputState {
    /// Reserve the run column. Off (the default) leaves the gutter as it was.
    pub fn set_runnable_gutter(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.runnable_gutter != on {
            self.runnable_gutter = on;
            cx.notify();
        }
    }

    /// Draw a ▶ on these rows (0-based). An empty list clears them.
    pub fn set_runnable_rows(&mut self, mut rows: Vec<usize>, cx: &mut Context<Self>) {
        rows.sort_unstable();
        rows.dedup();
        if self.runnable_rows != rows {
            self.runnable_rows = rows;
            cx.notify();
        }
    }

    pub(super) fn has_runnable(&self, row: usize) -> bool {
        self.runnable_gutter && self.runnable_rows.binary_search(&row).is_ok()
    }

    fn in_runnable_column(&self, position: Point<Pixels>) -> bool {
        if runnable_column_width(self) == px(0.) {
            return false;
        }
        let left = self.input_bounds.origin.x + runnable_column_x(self);
        position.x >= left && position.x < left + RUNNABLE_COLUMN_WIDTH
    }

    /// A left click on a ▶: report the row and keep the caret where it is.
    pub(super) fn handle_runnable_click(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) -> bool {
        if event.button != MouseButton::Left || !self.in_runnable_column(event.position) {
            return false;
        }
        let Some(row) = self.row_for_mouse_position(event.position) else {
            return false;
        };
        if !self.has_runnable(row) {
            return false;
        }
        cx.emit(super::InputEvent::RunnableClicked { row });
        cx.stop_propagation();
        true
    }
}
