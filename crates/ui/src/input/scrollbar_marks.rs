//! Marks painted in the scrollbar track (#252, Zed's `scrollbar.*`).
//!
//! VS Code calls this the overview ruler and cannot take it apart; Zed has a
//! flag per kind, which is what is followed here. **The scrollbar does not
//! know what a mark means** -- this decides which rows deserve one and what
//! colour it gets, and hands [`crate::scroll::ScrollbarMark`] over.
//!
//! The row-to-position arithmetic is a free function so it can be tested
//! without an editor.

use gpui::Hsla;

use crate::scroll::ScrollbarMark;

/// Which kinds of mark are painted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollbarMarks {
    /// Where the caret is.
    pub cursors: bool,
    /// What ⌘F found.
    pub search_results: bool,
    /// Other runs of the selected text.
    pub selected_text: bool,
    /// What the language server called an occurrence of the symbol.
    pub selected_symbol: bool,
    /// Diagnostics at or above this severity; `None` paints none.
    pub diagnostics: Option<MarkSeverity>,
}

/// The least severe diagnostic that still earns a mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MarkSeverity {
    Error,
    Warning,
    All,
}

impl ScrollbarMarks {
    /// Whether anything at all is painted.
    #[inline]
    pub fn any(&self) -> bool {
        self.cursors
            || self.search_results
            || self.selected_text
            || self.selected_symbol
            || self.diagnostics.is_some()
    }
}

/// Where a row sits in the track, 0.0 at the top and 1.0 at the bottom.
///
/// **Counts rows, not pixels.** The track stands for the whole document, so a
/// wrapped line and a plain one take the same space -- that is what makes the
/// marks line up with the line numbers rather than with the scroll offset.
#[must_use]
pub fn position_of(row: usize, total_rows: usize) -> f32 {
    if total_rows <= 1 {
        return 0.;
    }
    (row as f32 / (total_rows - 1) as f32).clamp(0., 1.)
}

/// Turn rows into marks, dropping the ones that would land on the same pixel.
///
/// **A file with ten thousand matches must not paint ten thousand quads.**
/// Two marks closer together than `min_gap` are the same mark to a reader, so
/// only the first survives. Rows are expected in order.
#[must_use]
pub fn marks_for_rows(
    rows: impl IntoIterator<Item = usize>,
    total_rows: usize,
    color: Hsla,
    min_gap: f32,
) -> Vec<ScrollbarMark> {
    let mut out: Vec<ScrollbarMark> = Vec::new();
    for row in rows {
        let at = position_of(row, total_rows);
        if let Some(last) = out.last() {
            if (at - last.at).abs() < min_gap {
                continue;
            }
        }
        out.push(ScrollbarMark { at, color });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::hsla;

    fn red() -> Hsla {
        hsla(0., 1., 0.5, 1.)
    }

    #[test]
    fn the_first_row_is_the_top_and_the_last_is_the_bottom() {
        assert_eq!(position_of(0, 100), 0.);
        assert_eq!(position_of(99, 100), 1.);
        assert!((position_of(49, 100) - 0.494_949_5).abs() < 1e-6);
    }

    /// A one-line file has nowhere to be but the top.
    #[test]
    fn a_file_with_one_row_does_not_divide_by_zero() {
        assert_eq!(position_of(0, 1), 0.);
        assert_eq!(position_of(0, 0), 0.);
        assert_eq!(position_of(5, 1), 0.);
    }

    /// Ten thousand matches must not become ten thousand quads.
    #[test]
    fn marks_too_close_together_collapse() {
        let rows = 0..1000usize;
        let marks = marks_for_rows(rows, 1000, red(), 0.01);
        assert!(marks.len() <= 101, "got {}", marks.len());
        assert_eq!(marks[0].at, 0.);
    }

    #[test]
    fn a_gap_of_zero_keeps_every_row() {
        let marks = marks_for_rows([0, 1, 2], 3, red(), 0.);
        assert_eq!(marks.len(), 3);
    }

    #[test]
    fn no_rows_means_no_marks() {
        assert!(marks_for_rows([], 10, red(), 0.01).is_empty());
    }
}

/// How close two marks may be before they read as one.
const MIN_GAP: f32 = 0.004;

impl super::InputState {
    /// The marks this editor wants in its scrollbar track (#252).
    ///
    /// Ordered so the important things end up on top: the caret last, because
    /// it is where **you** are and must not sit under a match.
    pub(super) fn track_marks(&self, cx: &gpui::App) -> Vec<ScrollbarMark> {
        use crate::{ActiveTheme as _, RopeExt as _};

        let want = self.mode.scrollbar_marks();
        if !want.any() {
            return vec![];
        }
        let total = self.text.lines_len().max(1);
        let row_of = |offset: usize| self.text.offset_to_point(offset).row;
        let mut out = vec![];

        if want.search_results {
            if let Some(ranges) = self.search_matches(cx) {
                let rows = ranges.iter().map(|r| row_of(r.start));
                out.extend(marks_for_rows(
                    rows,
                    total,
                    cx.theme().yellow.opacity(0.9),
                    MIN_GAP,
                ));
            }
        }
        if want.selected_symbol {
            let all = 0..self.text.len();
            let rows = self
                .lsp
                .document_highlights_for_range(&self.text, &all)
                .into_iter()
                .map(|(range, _)| row_of(range.start));
            out.extend(marks_for_rows(
                rows,
                total,
                cx.theme().accent_foreground.opacity(0.8),
                MIN_GAP,
            ));
        }
        if want.selected_text {
            out.extend(marks_for_rows(
                self.selection_match_rows(),
                total,
                cx.theme().muted_foreground.opacity(0.7),
                MIN_GAP,
            ));
        }
        if let Some(min) = want.diagnostics {
            out.extend(self.diagnostic_marks(min, total, cx));
        }
        if want.cursors {
            out.extend(marks_for_rows(
                [row_of(self.cursor())],
                total,
                cx.theme().foreground.opacity(0.9),
                0.,
            ));
        }
        out
    }

    /// What ⌘F has found, when the panel is open.
    fn search_matches(&self, cx: &gpui::App) -> Option<std::rc::Rc<Vec<std::ops::Range<usize>>>> {
        let panel = self.search_panel.as_ref()?;
        Some(panel.read(cx).matched_ranges())
    }

    /// Rows holding another run of the selected text.
    ///
    /// **Searches the whole document**, unlike the painted highlight, which
    /// only looks at what is on screen -- a mark off screen is the whole
    /// point of putting it in the scrollbar.
    fn selection_match_rows(&self) -> Vec<usize> {
        use crate::RopeExt as _;

        if !self.mode.selection_highlight() || self.masked {
            return vec![];
        }
        let selected: std::ops::Range<usize> = self.selected_range.into();
        let (start, end) = (
            selected.start.min(selected.end),
            selected.start.max(selected.end),
        );
        if start == end || end > self.text.len() {
            return vec![];
        }
        let needle = self.text.slice(start..end).to_string();
        if needle.chars().count() > self.mode.selection_highlight_max_len() {
            return vec![];
        }
        if needle.contains('\n') && !self.mode.selection_highlight_multiline() {
            return vec![];
        }
        let haystack = self.text.to_string();
        super::selection::occurrences(&needle, &haystack, 0, &(start..end))
            .into_iter()
            .map(|r| self.text.offset_to_point(r.start).row)
            .collect()
    }

    /// Diagnostic marks at or above `min`.
    fn diagnostic_marks(
        &self,
        min: MarkSeverity,
        total: usize,
        cx: &gpui::App,
    ) -> Vec<ScrollbarMark> {
        use crate::{ActiveTheme as _, RopeExt as _, highlighter::DiagnosticSeverity};

        let Some(set) = self.mode.diagnostics() else {
            return vec![];
        };
        let keep = |s: DiagnosticSeverity| match min {
            MarkSeverity::Error => matches!(s, DiagnosticSeverity::Error),
            MarkSeverity::Warning => {
                matches!(s, DiagnosticSeverity::Error | DiagnosticSeverity::Warning)
            }
            MarkSeverity::All => true,
        };
        let mut out = vec![];
        for entry in set.range(0..self.text.len()) {
            if !keep(entry.diagnostic.severity) {
                continue;
            }
            let color = match entry.diagnostic.severity {
                DiagnosticSeverity::Error => cx.theme().danger,
                DiagnosticSeverity::Warning => cx.theme().warning,
                _ => cx.theme().muted_foreground,
            };
            let row = self.text.offset_to_point(entry.range.start).row;
            out.extend(marks_for_rows([row], total, color.opacity(0.95), 0.));
        }
        out
    }
}
