//! Inlay hints drawn at the end of a row.
//!
//! # Why the end of the row
//!
//! VS Code and Zed draw inlay hints *inside* the line, pushing the code to the
//! right. This editor cannot: a [`ShapedLine`]'s byte offsets are used as
//! buffer byte offsets everywhere -- clicks
//! ([`InputState::index_for_mouse_position`]), the caret
//! ([`LineLayout::position_for_index`]), selection and search rectangles,
//! whitespace marks, the IME rectangle, and the `len() + 1` walk that steps
//! from one row to the next. Shaping extra glyphs into a line would shift all
//! of them, for that row *and every row below it*. Zed needs a whole display
//! map to do it properly.
//!
//! So the hints are shaped as their own line and painted after the end of the
//! row, the way [`super::fold`] paints its `⋯` badge. Nothing reads those
//! glyphs for offsets, so nothing moves.
//!
//! The caller decides *what* the string says -- which hint kinds are in it,
//! how they are joined, how long it may be. This module only draws it.

use gpui::{App, SharedString, TextStyle, Window, px};
use gpui::{Pixels, ShapedLine, TextRun};

use super::element::TextElement;
use super::state::{InputState, LastLayout};
use crate::ActiveTheme as _;

/// The gap between the end of the code and the hint.
pub(super) const INLAY_GAP: Pixels = px(12.);

/// The padding a hint chip puts either side of the label.
pub(super) const INLAY_CHIP_PAD: Pixels = px(4.);

/// How much smaller than the code a hint is drawn when no size is set.
///
/// VS Code reads `editor.inlayHints.fontSize: 0` the same way.
const RELATIVE_SIZE: f32 = 0.9;

/// The smallest size a hint is ever shaped at.
const MIN_SIZE: Pixels = px(5.);

/// One row's inlay hints, already joined into the text to draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlayRow {
    /// The buffer row, 0-based.
    pub row: usize,
    /// What to draw after the end of that row.
    pub label: SharedString,
}

impl InputState {
    /// Draw these hints at the end of their rows.
    ///
    /// Rows the buffer does not have are simply never painted, so a list that
    /// went stale between the request and the reply is harmless. Passing an
    /// empty list is the same as [`InputState::clear_inlay_hints`].
    pub fn set_inlay_hints(&mut self, mut rows: Vec<InlayRow>, cx: &mut gpui::Context<Self>) {
        rows.sort_by_key(|r| r.row);
        rows.dedup_by_key(|r| r.row);
        if self.inlay_rows == rows {
            return;
        }
        self.inlay_rows = rows;
        cx.notify();
    }

    /// Stop drawing inlay hints.
    pub fn clear_inlay_hints(&mut self, cx: &mut gpui::Context<Self>) {
        if self.inlay_rows.is_empty() {
            return;
        }
        self.inlay_rows.clear();
        cx.notify();
    }

    /// Whether any inlay hints have been supplied.
    ///
    /// Says nothing about whether they are on screen -- a modifier may be
    /// hiding them. The caller uses this to tell "none supplied yet" from
    /// "supplied, and there were none for this file".
    pub fn has_inlay_hints(&self) -> bool {
        !self.inlay_rows.is_empty()
    }

    /// Whether the hints are visible this frame.
    ///
    /// The modifier *flips* what the editor is doing, the way Zed's
    /// `toggle_on_modifiers_press` does: holding it reveals hints that are off
    /// and hides hints that are on. With no modifier set, they are simply on.
    pub(super) fn inlay_hints_visible(&self, window: &Window) -> bool {
        let on = self.mode.inlay_hints_on();
        let held = self.mode.inlay_hint_modifier().held(&window.modifiers());
        on != held
    }
}

impl TextElement {
    /// Shape the hint for every visible row that has one.
    ///
    /// Returns `(index into the visible rows, shaped label)`, matching
    /// [`TextElement::layout_fold_markers`] so the paint loop can find both
    /// the same way.
    pub(super) fn layout_inlay_hints(
        &self,
        state: &InputState,
        last_layout: &LastLayout,
        text_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
        cx: &App,
    ) -> Vec<(usize, ShapedLine)> {
        if state.inlay_rows.is_empty() || !state.inlay_hints_visible(window) {
            return vec![];
        }

        let size = match state.mode.inlay_hint_font_size() {
            s if s <= 0. => (text_size * RELATIVE_SIZE).max(MIN_SIZE),
            s => px(s).max(MIN_SIZE),
        };
        let mut font = style.font();
        if let Some(family) = state.mode.inlay_hint_font_family() {
            font.family = family;
        }

        let mut hints = vec![];
        for (ix, row) in last_layout.visible_range.clone().enumerate() {
            // A row hidden by a fold has nowhere to put a hint.
            if state.is_folded(row) {
                continue;
            }
            let Ok(at) = state.inlay_rows.binary_search_by_key(&row, |r| r.row) else {
                continue;
            };
            let label = state.inlay_rows[at].label.clone();
            let line = window.text_system().shape_line(
                label.clone(),
                size,
                &[TextRun {
                    len: label.len(),
                    font: font.clone(),
                    color: cx.theme().muted_foreground,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            hints.push((ix, line));
        }

        hints
    }
}
