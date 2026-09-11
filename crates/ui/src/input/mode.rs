use std::rc::Rc;
use std::{cell::RefCell, ops::Range};

use gpui::{App, SharedString};
use ropey::Rope;
use tree_sitter::InputEdit;

use super::text_wrapper::TextWrapper;
use crate::highlighter::DiagnosticSet;
use crate::highlighter::SyntaxHighlighter;
use crate::input::{AutoClose, BracketGuides, MatchBrackets, RopeExt as _, TabSize};

/// How the line number gutter of a [`InputMode::CodeEditor`] is rendered.
///
/// Mirrors VS Code's `editor.lineNumbers`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineNumbers {
    /// No gutter at all.
    Off,
    /// Absolute line numbers on every line.
    #[default]
    On,
    /// Distance from the cursor's line; the cursor's own line shows its
    /// absolute number (same as VS Code and vim's `relativenumber`).
    Relative,
    /// Only every n-th line, plus the cursor's line. VS Code uses 10.
    Interval(usize),
}

impl From<bool> for LineNumbers {
    fn from(on: bool) -> Self {
        if on {
            LineNumbers::On
        } else {
            LineNumbers::Off
        }
    }
}

impl LineNumbers {
    /// Whether the gutter takes up any width.
    #[inline]
    pub fn is_visible(&self) -> bool {
        !matches!(self, LineNumbers::Off)
    }

    /// The number to render on `row` (0-based), or `None` to leave it blank.
    ///
    /// `cursor_row` is the row the cursor is on, when the input has one.
    pub fn number_for(&self, row: usize, cursor_row: Option<usize>) -> Option<usize> {
        match self {
            LineNumbers::Off => None,
            LineNumbers::On => Some(row + 1),
            LineNumbers::Relative => match cursor_row {
                Some(cursor) if cursor == row => Some(row + 1),
                Some(cursor) => Some(cursor.abs_diff(row)),
                None => Some(row + 1),
            },
            LineNumbers::Interval(n) => {
                let n = (*n).max(1);
                if Some(row) == cursor_row || (row + 1) % n == 0 {
                    Some(row + 1)
                } else {
                    None
                }
            }
        }
    }
}

/// Which whitespace characters are drawn as visible marks.
///
/// Mirrors VS Code's `editor.renderWhitespace`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RenderWhitespace {
    /// Draw nothing.
    #[default]
    None,
    /// Everything except a single space between two words. Leading, trailing
    /// and repeated spaces are drawn; tabs are always drawn.
    Boundary,
    /// Only inside the selection.
    Selection,
    /// Only the whitespace after the last non-whitespace character of a line.
    Trailing,
    /// Every space and tab.
    All,
}

impl From<bool> for RenderWhitespace {
    fn from(on: bool) -> Self {
        if on {
            RenderWhitespace::All
        } else {
            RenderWhitespace::None
        }
    }
}

impl RenderWhitespace {
    /// Whether anything is drawn at all.
    #[inline]
    pub fn is_visible(&self) -> bool {
        !matches!(self, RenderWhitespace::None)
    }

    /// Whether the marks are limited to the selection.
    #[inline]
    pub fn follows_selection(&self) -> bool {
        matches!(self, RenderWhitespace::Selection)
    }
}

/// When the fold chevrons in the gutter are visible.
///
/// Mirrors VS Code's `editor.showFoldingControls`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum FoldingControls {
    /// Always, on every foldable row.
    #[default]
    Always,
    /// Only while the pointer is over the gutter.
    ///
    /// A row that is *already folded* keeps its chevron either way -- hiding it
    /// would leave no way to open the fold again.
    MouseOver,
}

impl FoldingControls {
    /// Whether a chevron shows on `row`.
    #[inline]
    pub fn shows(&self, row: usize, folded: bool, hovered_row: Option<usize>) -> bool {
        match self {
            FoldingControls::Always => true,
            FoldingControls::MouseOver => folded || hovered_row == Some(row),
        }
    }
}

#[derive(Clone)]
pub(crate) enum InputMode {
    /// A plain text input mode.
    PlainText {
        multi_line: bool,
        tab: TabSize,
        rows: usize,
    },
    /// An auto grow input mode.
    AutoGrow {
        rows: usize,
        min_rows: usize,
        max_rows: usize,
    },
    /// A code editor input mode.
    CodeEditor {
        multi_line: bool,
        tab: TabSize,
        rows: usize,
        /// How the line number gutter is rendered
        line_number: LineNumbers,
        language: SharedString,
        indent_guides: bool,
        /// Which whitespace characters are drawn as visible marks
        render_whitespace: RenderWhitespace,
        /// Whether rows can be folded
        folding: bool,
        /// When the fold chevrons are visible
        folding_controls: FoldingControls,
        /// Which auto-closing behaviours are on
        auto_close: AutoClose,
        /// When the matching bracket is outlined
        match_brackets: MatchBrackets,
        /// Whether bracket pairs are coloured by nesting depth
        bracket_colors: bool,
        /// Which bracket pairs get a guide line
        bracket_guides: BracketGuides,
        /// Whether a space follows the comment token
        comment_insert_space: bool,
        highlighter: Rc<RefCell<Option<SyntaxHighlighter>>>,
        diagnostics: DiagnosticSet,
    },
}

impl Default for InputMode {
    fn default() -> Self {
        InputMode::plain_text()
    }
}

#[allow(unused)]
impl InputMode {
    /// Create a plain input mode with default settings.
    pub(super) fn plain_text() -> Self {
        InputMode::PlainText {
            multi_line: false,
            tab: TabSize::default(),
            rows: 1,
        }
    }

    /// Create a code editor input mode with default settings.
    pub(super) fn code_editor(language: impl Into<SharedString>) -> Self {
        InputMode::CodeEditor {
            rows: 2,
            multi_line: true,
            tab: TabSize::default(),
            language: language.into(),
            highlighter: Rc::new(RefCell::new(None)),
            line_number: LineNumbers::default(),
            indent_guides: true,
            render_whitespace: RenderWhitespace::default(),
            folding: true,
            folding_controls: FoldingControls::default(),
            auto_close: AutoClose::default(),
            match_brackets: MatchBrackets::default(),
            bracket_colors: true,
            bracket_guides: BracketGuides::default(),
            comment_insert_space: true,
            diagnostics: DiagnosticSet::new(&Rope::new()),
        }
    }

    /// Create an auto grow input mode with given min and max rows.
    pub(super) fn auto_grow(min_rows: usize, max_rows: usize) -> Self {
        InputMode::AutoGrow {
            rows: min_rows,
            min_rows,
            max_rows,
        }
    }

    pub(super) fn multi_line(mut self, multi_line: bool) -> Self {
        match &mut self {
            InputMode::PlainText { multi_line: ml, .. } => *ml = multi_line,
            InputMode::CodeEditor { multi_line: ml, .. } => *ml = multi_line,
            InputMode::AutoGrow { .. } => {}
        }
        self
    }

    #[inline]
    pub(super) fn is_single_line(&self) -> bool {
        !self.is_multi_line()
    }

    #[inline]
    pub(super) fn is_code_editor(&self) -> bool {
        matches!(self, InputMode::CodeEditor { .. })
    }

    #[inline]
    pub(super) fn is_auto_grow(&self) -> bool {
        matches!(self, InputMode::AutoGrow { .. })
    }

    #[inline]
    pub(super) fn is_multi_line(&self) -> bool {
        match self {
            InputMode::PlainText { multi_line, .. } => *multi_line,
            InputMode::CodeEditor { multi_line, .. } => *multi_line,
            InputMode::AutoGrow { max_rows, .. } => *max_rows > 1,
        }
    }

    pub(super) fn set_rows(&mut self, new_rows: usize) {
        match self {
            InputMode::PlainText { rows, .. } => {
                *rows = new_rows;
            }
            InputMode::CodeEditor { rows, .. } => {
                *rows = new_rows;
            }
            InputMode::AutoGrow {
                rows,
                min_rows,
                max_rows,
            } => {
                *rows = new_rows.clamp(*min_rows, *max_rows);
            }
        }
    }

    pub(super) fn update_auto_grow(&mut self, text_wrapper: &TextWrapper) {
        if self.is_single_line() {
            return;
        }

        let wrapped_lines = text_wrapper.len();
        self.set_rows(wrapped_lines);
    }

    /// At least 1 row be return.
    pub(super) fn rows(&self) -> usize {
        if !self.is_multi_line() {
            return 1;
        }

        match self {
            InputMode::PlainText { rows, .. } => *rows,
            InputMode::CodeEditor { rows, .. } => *rows,
            InputMode::AutoGrow { rows, .. } => *rows,
        }
        .max(1)
    }

    /// At least 1 row be return.
    #[allow(unused)]
    pub(super) fn min_rows(&self) -> usize {
        match self {
            InputMode::AutoGrow { min_rows, .. } => *min_rows,
            _ => 1,
        }
        .max(1)
    }

    #[allow(unused)]
    pub(super) fn max_rows(&self) -> usize {
        if !self.is_multi_line() {
            return 1;
        }

        match self {
            InputMode::AutoGrow { max_rows, .. } => *max_rows,
            _ => usize::MAX,
        }
    }

    /// Whether the line number gutter is drawn.
    ///
    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn line_number(&self) -> bool {
        self.line_numbers().is_visible()
    }

    /// Return [`LineNumbers::Off`] if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn line_numbers(&self) -> LineNumbers {
        match self {
            InputMode::CodeEditor {
                line_number,
                multi_line,
                ..
            } if *multi_line => *line_number,
            _ => LineNumbers::Off,
        }
    }

    /// Return [`RenderWhitespace::None`] if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn render_whitespace(&self) -> RenderWhitespace {
        match self {
            InputMode::CodeEditor {
                render_whitespace,
                multi_line,
                ..
            } if *multi_line => *render_whitespace,
            _ => RenderWhitespace::None,
        }
    }

    /// Return the default when the mode is not [`InputMode::CodeEditor`].
    ///
    /// Plain inputs never auto-close: a single-line field is as likely to hold
    /// a search term as code.
    #[allow(unused)]
    #[inline]
    pub(super) fn auto_close(&self) -> AutoClose {
        match self {
            InputMode::CodeEditor { auto_close, .. } => *auto_close,
            _ => AutoClose::from(false),
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn comment_insert_space(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                comment_insert_space,
                ..
            } => *comment_insert_space,
            _ => false,
        }
    }

    /// Return [`BracketGuides::Off`] if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn bracket_guides(&self) -> BracketGuides {
        match self {
            InputMode::CodeEditor { bracket_guides, .. } => *bracket_guides,
            _ => BracketGuides::Off,
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn bracket_colors(&self) -> bool {
        match self {
            InputMode::CodeEditor { bracket_colors, .. } => *bracket_colors,
            _ => false,
        }
    }

    /// Return [`MatchBrackets::Never`] if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn match_brackets(&self) -> MatchBrackets {
        match self {
            InputMode::CodeEditor { match_brackets, .. } => *match_brackets,
            _ => MatchBrackets::Never,
        }
    }

    /// The language name, or `""` if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn language_name(&self) -> &str {
        match self {
            InputMode::CodeEditor { language, .. } => language,
            _ => "",
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn has_folding(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                folding,
                multi_line,
                ..
            } => *folding && *multi_line,
            _ => false,
        }
    }

    /// Return the default when the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn folding_controls(&self) -> FoldingControls {
        match self {
            InputMode::CodeEditor {
                folding_controls, ..
            } => *folding_controls,
            _ => FoldingControls::default(),
        }
    }

    pub(super) fn update_highlighter(
        &mut self,
        selected_range: &Range<usize>,
        text: &Rope,
        new_text: &str,
        force: bool,
        cx: &mut App,
    ) {
        match &self {
            InputMode::CodeEditor {
                language,
                highlighter,
                ..
            } => {
                if !force && highlighter.borrow().is_some() {
                    return;
                }

                let mut highlighter = highlighter.borrow_mut();
                if highlighter.is_none() {
                    let new_highlighter = SyntaxHighlighter::new(language);
                    highlighter.replace(new_highlighter);
                }

                let Some(highlighter) = highlighter.as_mut() else {
                    return;
                };

                // When full text changed, the selected_range may be out of bound (The before version).
                let mut selected_range = selected_range.clone();
                selected_range.end = selected_range.end.min(text.len());

                // If insert a chart, this is 1.
                // If backspace or delete, this is -1.
                // If selected to delete, this is the length of the selected text.
                // let changed_len = new_text.len() as isize - selected_range.len() as isize;
                let changed_len = new_text.len() as isize - selected_range.len() as isize;
                let new_end = (selected_range.end as isize + changed_len) as usize;

                let start_pos = text.offset_to_point(selected_range.start);
                let old_end_pos = text.offset_to_point(selected_range.end);
                let new_end_pos = text.offset_to_point(new_end);

                let edit = InputEdit {
                    start_byte: selected_range.start,
                    old_end_byte: selected_range.end,
                    new_end_byte: new_end,
                    start_position: start_pos,
                    old_end_position: old_end_pos,
                    new_end_position: new_end_pos,
                };

                highlighter.update(Some(edit), text);
            }
            _ => {}
        }
    }

    #[allow(unused)]
    pub(super) fn diagnostics(&self) -> Option<&DiagnosticSet> {
        match self {
            InputMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }

    pub(super) fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        match self {
            InputMode::CodeEditor { diagnostics, .. } => Some(diagnostics),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use ropey::Rope;

    use crate::{
        highlighter::DiagnosticSet,
        input::{
            AutoClose, BracketGuides, MatchBrackets, TabSize,
            mode::{FoldingControls, InputMode, LineNumbers, RenderWhitespace},
        },
    };

    #[test]
    fn test_code_editor() {
        let mode = InputMode::code_editor("rust");
        assert_eq!(mode.is_code_editor(), true);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), true);
        assert_eq!(mode.has_indent_guides(), true);
        assert_eq!(mode.has_folding(), true);
        assert_eq!(mode.folding_controls(), FoldingControls::Always);
        assert_eq!(mode.auto_close(), AutoClose::default(), "on like VS Code");
        assert_eq!(mode.match_brackets(), MatchBrackets::Always);
        assert!(mode.bracket_colors(), "on like VS Code");
        assert_eq!(mode.bracket_guides(), BracketGuides::Off, "off like VS Code");
        assert!(mode.comment_insert_space(), "`// a` like VS Code");
        assert_eq!(mode.language_name(), "rust");
        assert_eq!(mode.max_rows(), usize::MAX);
        assert_eq!(mode.min_rows(), 1);

        let mode = InputMode::CodeEditor {
            multi_line: false,
            line_number: LineNumbers::On,
            indent_guides: true,
            render_whitespace: RenderWhitespace::None,
            folding: true,
            folding_controls: FoldingControls::default(),
            auto_close: AutoClose::default(),
            match_brackets: MatchBrackets::default(),
            bracket_colors: true,
            bracket_guides: BracketGuides::default(),
            comment_insert_space: true,
            rows: 0,
            tab: Default::default(),
            language: "rust".into(),
            highlighter: Default::default(),
            diagnostics: DiagnosticSet::new(&Rope::new()),
        };
        assert_eq!(mode.is_code_editor(), true);
        assert_eq!(mode.is_multi_line(), false);
        assert_eq!(mode.is_single_line(), true);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.has_indent_guides(), false);
        assert_eq!(mode.has_folding(), false, "single line never folds");
        assert_eq!(mode.max_rows(), 1);
        assert_eq!(mode.min_rows(), 1);
    }

    #[test]
    fn test_plain() {
        let mode = InputMode::PlainText {
            multi_line: true,
            tab: TabSize::default(),
            rows: 5,
        };
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 5);
        assert_eq!(mode.max_rows(), usize::MAX);
        assert_eq!(mode.min_rows(), 1);

        let mode = InputMode::plain_text();
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), false);
        assert_eq!(mode.is_single_line(), true);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.max_rows(), 1);
        assert_eq!(mode.min_rows(), 1);
    }

    #[test]
    fn test_auto_grow() {
        let mut mode = InputMode::auto_grow(2, 5);
        assert_eq!(mode.is_code_editor(), false);
        assert_eq!(mode.is_multi_line(), true);
        assert_eq!(mode.is_single_line(), false);
        assert_eq!(mode.line_number(), false);
        assert_eq!(mode.rows(), 2);
        assert_eq!(mode.max_rows(), 5);
        assert_eq!(mode.min_rows(), 2);

        mode.set_rows(4);
        assert_eq!(mode.rows(), 4);

        mode.set_rows(1);
        assert_eq!(mode.rows(), 2);

        mode.set_rows(10);
        assert_eq!(mode.rows(), 5);
    }

    #[test]
    fn test_folding_controls() {
        assert_eq!(FoldingControls::default(), FoldingControls::Always);

        // Always: every foldable row, hovered or not.
        assert!(FoldingControls::Always.shows(3, false, None));
        assert!(FoldingControls::Always.shows(3, false, Some(9)));

        // MouseOver: only the hovered row...
        let m = FoldingControls::MouseOver;
        assert!(!m.shows(3, false, None));
        assert!(!m.shows(3, false, Some(9)));
        assert!(m.shows(3, false, Some(3)));
        // ...but a folded row always keeps its chevron, or there would be no
        // way to open it again.
        assert!(m.shows(3, true, None));
    }

    #[test]
    fn test_line_numbers() {
        assert_eq!(LineNumbers::default(), LineNumbers::On);
        assert_eq!(LineNumbers::from(true), LineNumbers::On);
        assert_eq!(LineNumbers::from(false), LineNumbers::Off);

        assert_eq!(LineNumbers::Off.is_visible(), false);
        assert_eq!(LineNumbers::On.is_visible(), true);
        assert_eq!(LineNumbers::Relative.is_visible(), true);
        assert_eq!(LineNumbers::Interval(10).is_visible(), true);

        assert_eq!(LineNumbers::Off.number_for(0, Some(0)), None);
        assert_eq!(LineNumbers::On.number_for(0, Some(3)), Some(1));
        assert_eq!(LineNumbers::On.number_for(41, None), Some(42));

        // Relative counts the distance, but the cursor's own row is absolute.
        assert_eq!(LineNumbers::Relative.number_for(3, Some(3)), Some(4));
        assert_eq!(LineNumbers::Relative.number_for(0, Some(3)), Some(3));
        assert_eq!(LineNumbers::Relative.number_for(5, Some(3)), Some(2));
        // No cursor -> absolute, so the gutter is never blank.
        assert_eq!(LineNumbers::Relative.number_for(5, None), Some(6));

        // Interval shows every n-th line plus the cursor's line.
        let every10 = LineNumbers::Interval(10);
        assert_eq!(every10.number_for(0, Some(3)), None);
        assert_eq!(every10.number_for(3, Some(3)), Some(4));
        assert_eq!(every10.number_for(9, Some(3)), Some(10));
        assert_eq!(every10.number_for(19, None), Some(20));
        // 0 would divide by zero; clamp to 1 (= every line).
        assert_eq!(LineNumbers::Interval(0).number_for(4, None), Some(5));
    }
}
