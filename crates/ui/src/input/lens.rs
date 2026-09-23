//! Clickable one-line labels **above** a row -- what VS Code calls CodeLens
//! (dopamine #412).
//!
//! # Why a row of its own
//!
//! Like [`super::inlay`], nothing may be shaped into a line: its byte offsets
//! are everybody's. A lens takes a whole row, so it rides the machinery the
//! multi-line inline completion already uses to push rows down
//! ([`super::text_wrapper::TextWrapper::set_extra_rows`]): a lens above row
//! `r` is one extra row **below `r - 1`**. Row 0 has nothing above it, so a
//! lens there is dropped.
//!
//! The caller decides what the labels say and what a click does; this module
//! draws them and says which one was clicked
//! ([`super::InputEvent::LensClicked`]).

use gpui::{Bounds, Pixels, Point, SharedString};

use super::state::InputState;

/// The labels above one row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LensRow {
    /// The buffer row the labels sit above, 0-based.
    pub row: usize,
    /// One entry per clickable label, drawn joined with ` | `.
    pub items: Vec<SharedString>,
}

impl InputState {
    /// Draw these labels above their rows. An empty list clears them.
    pub fn set_lens_rows(&mut self, mut rows: Vec<LensRow>, cx: &mut gpui::Context<Self>) {
        rows.retain(|r| r.row > 0 && !r.items.is_empty());
        rows.sort_by_key(|r| r.row);
        rows.dedup_by_key(|r| r.row);
        if self.lens_rows == rows {
            return;
        }
        self.lens_rows = rows;
        cx.notify();
    }

    /// Rows the wrapper has to leave room for: one below the row above each lens.
    pub(super) fn lens_extra_rows(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.lens_rows.iter().map(|l| (l.row - 1, 1))
    }

    pub(super) fn lens_for_row(&self, row: usize) -> Option<&LensRow> {
        self.lens_rows
            .binary_search_by_key(&row, |l| l.row)
            .ok()
            .map(|i| &self.lens_rows[i])
    }

    /// The label under `position`, as (lens row, item index). Filled by the
    /// last paint.
    pub(super) fn lens_hit(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        self.lens_hitboxes
            .iter()
            .find(|(b, _, _)| b.contains(&position))
            .map(|(_, row, item)| (*row, *item))
    }

    pub(super) fn set_lens_hitboxes(&mut self, boxes: Vec<(Bounds<Pixels>, usize, usize)>) {
        self.lens_hitboxes = boxes;
    }
}
