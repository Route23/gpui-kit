//! Backgrounds for whole rows and for byte ranges, and a scroll offset a
//! caller can read and set -- what a diff view needs from an editor
//! (dopamine #244 / #245).
//!
//! **Colours come from the caller.** Like [`super::gutter_marks`], nothing
//! here decides what "added" or "removed" means.

use std::ops::Range;

use gpui::{px, Hsla, Pixels};

use super::state::InputState;

impl InputState {
    /// Paint these rows' backgrounds across the whole width of the text area.
    /// An empty list clears them.
    pub fn set_row_backgrounds(&mut self, mut rows: Vec<(usize, Hsla)>, cx: &mut gpui::Context<Self>) {
        rows.sort_by_key(|r| r.0);
        if self.row_backgrounds == rows {
            return;
        }
        self.row_backgrounds = rows;
        cx.notify();
    }

    /// Paint behind these byte ranges (a word-level diff). An empty list clears them.
    pub fn set_range_backgrounds(
        &mut self,
        ranges: Vec<(Range<usize>, Hsla)>,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.range_backgrounds == ranges {
            return;
        }
        self.range_backgrounds = ranges;
        cx.notify();
    }

    pub(super) fn row_background(&self, row: usize) -> Option<Hsla> {
        self.row_backgrounds
            .binary_search_by_key(&row, |r| r.0)
            .ok()
            .map(|i| self.row_backgrounds[i].1)
    }

    /// How far down the text is scrolled, as a positive number of pixels.
    pub fn scroll_offset_y(&self) -> Pixels {
        -self.scroll_handle.offset().y
    }

    /// Scroll to `y` pixels from the top (clamped at 0). Does not move the caret.
    pub fn set_scroll_offset_y(&mut self, y: Pixels, cx: &mut gpui::Context<Self>) {
        let mut offset = self.scroll_handle.offset();
        let target = -y.max(px(0.));
        if offset.y == target {
            return;
        }
        offset.y = target;
        self.scroll_handle.set_offset(offset);
        cx.notify();
    }

    /// The rows on screen in the last frame. `None` before the first layout.
    pub fn visible_row_range(&self) -> Option<Range<usize>> {
        self.last_layout.as_ref().map(|l| l.visible_range.clone())
    }

    /// The height of one row in the last frame. `None` before the first layout.
    pub fn row_height(&self) -> Option<Pixels> {
        self.last_layout.as_ref().map(|l| l.line_height)
    }
}
