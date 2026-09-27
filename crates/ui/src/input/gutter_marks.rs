//! Change marks in the gutter -- the bar VS Code and Zed draw beside the line
//! numbers for rows that differ from the last commit.
//!
//! **Meaning stays with the caller.** This module neither runs git nor diffs
//! anything; it is handed rows and a kind, and draws a bar. Like
//! [`super::scrollbar_marks`], a list that went stale between the diff and the
//! frame is harmless: rows the buffer does not have are never painted.

use std::ops::Range;

use gpui::{px, Context, Hsla, MouseButton, MouseDownEvent, Pixels};

use super::state::InputState;
use crate::ActiveTheme as _;

/// The bar's width.
pub(super) const GUTTER_MARK_WIDTH: Pixels = px(3.);

/// How far from the gutter's left edge a click still counts as the mark.
/// Wider than the bar itself so a 3px target is not a pixel hunt (dopamine #490).
pub(super) const GUTTER_MARK_HIT_WIDTH: Pixels = px(8.);

/// What happened to the rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GutterMarkKind {
    /// New rows.
    Added,
    /// Rows that were there, changed.
    Modified,
    /// Rows removed just above `rows.start` (`rows` is empty).
    Deleted,
}

impl GutterMarkKind {
    pub(super) fn color(self, cx: &gpui::App) -> Hsla {
        match self {
            Self::Added => cx.theme().success,
            Self::Modified => cx.theme().info,
            Self::Deleted => cx.theme().danger,
        }
    }
}

/// One run of changed rows (0-based, end exclusive).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GutterMark {
    pub rows: Range<usize>,
    pub kind: GutterMarkKind,
}

impl InputState {
    /// Draw these marks in the gutter. An empty list clears them.
    ///
    /// Marks are painted with the line numbers, so a gutter with the numbers
    /// turned off shows none.
    pub fn set_gutter_marks(&mut self, mut marks: Vec<GutterMark>, cx: &mut gpui::Context<Self>) {
        marks.sort_by_key(|m| m.rows.start);
        if self.gutter_marks == marks {
            return;
        }
        self.gutter_marks = marks;
        cx.notify();
    }

    /// A left click on a change mark: report the row and consume the click so the
    /// caret does not move (dopamine #490). The host decides what to offer.
    ///
    /// Marks are painted with the line numbers, so without them nothing is hit.
    pub(super) fn handle_gutter_mark_click(
        &mut self,
        event: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.button != MouseButton::Left
            || self.gutter_marks.is_empty()
            || !self.mode.line_number()
        {
            return false;
        }
        // `input_bounds`, not `last_bounds`: the gutter does not scroll sideways.
        let left = self.input_bounds.origin.x;
        if event.position.x < left || event.position.x >= left + GUTTER_MARK_HIT_WIDTH {
            return false;
        }
        let Some(row) = self.row_for_mouse_position(event.position) else {
            return false;
        };
        if self.gutter_mark_at(row).is_none() {
            return false;
        }
        cx.emit(super::InputEvent::GutterMarkClicked { row });
        cx.stop_propagation();
        true
    }

    /// The mark on `row`, if any. A deletion marks the row below the gap.
    pub(super) fn gutter_mark_at(&self, row: usize) -> Option<GutterMarkKind> {
        self.gutter_marks.iter().find_map(|m| {
            let hit = if m.kind == GutterMarkKind::Deleted {
                m.rows.start == row
            } else {
                m.rows.contains(&row)
            };
            hit.then_some(m.kind)
        })
    }
}
