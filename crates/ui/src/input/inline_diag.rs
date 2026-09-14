//! Diagnostics drawn at the end of the row they belong to.
//!
//! The squiggle says *where*; this says *what*, without asking the reader to
//! park the pointer on it. Zed calls it `diagnostics.inline`; the VS Code
//! world knows the idea as Error Lens.
//!
//! Drawn the same way [`super::inlay`] draws its hints -- a line of its own,
//! painted after the end of the row, touching none of the byte offsets the
//! editor uses for clicks, the caret, selection or the IME. See that module
//! for why nothing may be shaped *into* a line here.
//!
//! The caller picks the row, the severity and the words. This module places
//! them: after the inlay hints, `padding` columns clear of the code, and never
//! left of `min_column`.

use gpui::{App, Pixels, SharedString, ShapedLine, TextRun, TextStyle, Window};

use super::element::TextElement;
use super::state::{InputState, LastLayout};
use crate::highlighter::DiagnosticSeverity;

/// One row's diagnostic, already reduced to a single line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineDiagnostic {
    /// The buffer row, 0-based.
    pub row: usize,
    /// Which colour it gets -- the same one its squiggle has.
    pub severity: DiagnosticSeverity,
    /// What to draw after the end of that row.
    pub text: SharedString,
}

impl InputState {
    /// Draw these diagnostics at the end of their rows.
    ///
    /// Rows the buffer does not have are never painted, so a list that went
    /// stale between the reply and the frame is harmless. Passing an empty
    /// list is the same as [`InputState::clear_inline_diagnostics`].
    pub fn set_inline_diagnostics(
        &mut self,
        mut rows: Vec<InlineDiagnostic>,
        cx: &mut gpui::Context<Self>,
    ) {
        rows.sort_by_key(|r| r.row);
        rows.dedup_by_key(|r| r.row);
        if self.inline_diagnostics == rows {
            return;
        }
        self.inline_diagnostics = rows;
        cx.notify();
    }

    /// Stop drawing them. The squiggles stay.
    pub fn clear_inline_diagnostics(&mut self, cx: &mut gpui::Context<Self>) {
        if self.inline_diagnostics.is_empty() {
            return;
        }
        self.inline_diagnostics.clear();
        cx.notify();
    }
}

impl TextElement {
    /// Shape the diagnostic for every visible row that has one.
    ///
    /// Returns `(index into the visible rows, shaped line)`, matching
    /// [`TextElement::layout_inlay_hints`] so the paint loop finds both the
    /// same way. The severity is already in the colour.
    pub(super) fn layout_inline_diagnostics(
        &self,
        state: &InputState,
        last_layout: &LastLayout,
        text_size: Pixels,
        style: &TextStyle,
        window: &mut Window,
        cx: &App,
    ) -> Vec<(usize, ShapedLine)> {
        if state.inline_diagnostics.is_empty() {
            return vec![];
        }

        let mut out = vec![];
        for (ix, row) in last_layout.visible_range.clone().enumerate() {
            // A row hidden by a fold has nowhere to put the words.
            if state.is_folded(row) {
                continue;
            }
            let Ok(at) = state
                .inline_diagnostics
                .binary_search_by_key(&row, |d| d.row)
            else {
                continue;
            };
            let entry = &state.inline_diagnostics[at];
            let text = entry.text.clone();
            let line = window.text_system().shape_line(
                text.clone(),
                text_size,
                &[TextRun {
                    len: text.len(),
                    font: style.font(),
                    color: entry.severity.fg(cx),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                }],
                None,
            );
            out.push((ix, line));
        }

        out
    }
}
