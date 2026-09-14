use std::rc::Rc;
use std::{cell::RefCell, ops::Range};

use gpui::{px, App, Pixels, SharedString};
use ropey::Rope;
use tree_sitter::InputEdit;

use super::text_wrapper::TextWrapper;
use crate::highlighter::DiagnosticSet;
use crate::highlighter::SyntaxHighlighter;
use crate::input::{
    AutoClose, BracketGuides, CaretAnimation, CursorBlinking, CursorStyle, MatchBrackets,
    RopeExt as _, SurroundingLinesStyle, TabSize,
};

/// How many rows the editor keeps above and below the caret when it scrolls
/// the caret into view.
///
/// This was a bare `3` in `state.rs` and `element.rs` before it was settable,
/// so it stays the default. VS Code's `editor.cursorSurroundingLines` is 0.
pub(super) const DEFAULT_SURROUNDING_LINES: u8 = 3;

/// The default smallest width of the line number gutter, in digits.
///
/// Four matches what most editors reserve, so a file does not shift sideways
/// the moment it grows past 999 lines.
pub(super) const DEFAULT_MIN_LINE_NUMBER_DIGITS: usize = 4;

/// How long the pointer has to rest before the hover popover appears.
///
/// This was a bare `150` in `lsp/hover.rs` before it was settable, so it stays
/// the default. VS Code's `editor.hover.delay` is 300.
pub(super) const DEFAULT_HOVER_DELAY: u16 = 150;

/// How long to wait after the pointer leaves a symbol before the hover popover
/// goes away.
///
/// **Zero means "do not hide on leave"**, which is what the editor did before
/// this was settable: the popover only went away when the provider answered
/// `None` for the new position, or when the input lost focus.
pub(super) const DEFAULT_HOVER_HIDING_DELAY: u16 = 0;

/// The most fold regions [`InputState::fold_all`] will create, and the most
/// folds that can be open at once.
///
/// VS Code's `editor.foldingMaximumRegions` defaults to the same number. It is
/// a guard for very large files, not a limit anyone should reach by hand.
pub(super) const DEFAULT_MAX_FOLD_REGIONS: u32 = 5_000;

/// Which modifier flips inlay hints while it is held down.
///
/// Mirrors Zed's `inlay_hints.toggle_on_modifiers_press`: holding the key
/// inverts whatever the editor is doing right now, so it both reveals hints
/// that are off and hides hints that are on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InlayModifier {
    /// No key flips them; the hints stay as they are.
    #[default]
    None,
    Control,
    /// The option/alt key.
    Alt,
    /// The command key.
    Platform,
    Shift,
}

impl InlayModifier {
    /// Whether `modifiers` holds this key down.
    pub(super) fn held(self, modifiers: &gpui::Modifiers) -> bool {
        match self {
            InlayModifier::None => false,
            InlayModifier::Control => modifiers.control,
            InlayModifier::Alt => modifiers.alt,
            InlayModifier::Platform => modifiers.platform,
            InlayModifier::Shift => modifiers.shift,
        }
    }
}

/// Where soft wrapping breaks a long line.
///
/// Only consulted while soft wrap is on; mirrors the non-`off` half of VS
/// Code's `editor.wordWrap` and Zed's `soft_wrap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WrapAt {
    /// At whatever fits the viewport -- VS Code's `on`, Zed's `editor_width`.
    #[default]
    EditorWidth,
    /// At a fixed column, however wide the viewport is. Lines longer than the
    /// column scroll horizontally -- VS Code's `wordWrapColumn`.
    Column(usize),
    /// At the column, or the viewport if that is narrower -- VS Code's
    /// `bounded`.
    Bounded(usize),
}

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

/// The variants are lopsided on purpose: `CodeEditor` carries the editor's
/// whole settings surface, and a plain text field carries three fields. Boxing
/// the payload to even them out would put an indirection on a value the
/// element reads several times per frame, and would touch every `match` on the
/// mode. There is one of these per input, not one per row.
#[allow(clippy::large_enum_variant)]
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
        /// The smallest number of digits the line number gutter reserves.
        ///
        /// The gutter never shrinks below what the last line number needs, so
        /// this only widens it — useful to keep the text from shifting as a
        /// file grows past a power of ten.
        min_line_number_digits: usize,
        /// Whether the empty line a trailing newline creates gets a number.
        ///
        /// A buffer ending in `\n` has one more (empty) line. Turning this off
        /// leaves that row blank in the gutter; the row itself stays, so the
        /// caret can still be placed on it.
        render_final_newline: bool,
        /// Where soft wrapping breaks a long line, when soft wrap is on.
        wrap_at: WrapAt,
        /// Columns that get a vertical ruler drawn through the whole editor.
        ///
        /// Mirrors VS Code's `editor.rulers` and Zed's `wrap_guides`. Empty
        /// means no rulers; a column past the right edge is simply not drawn.
        rulers: Rc<[usize]>,
        language: SharedString,
        indent_guides: bool,
        /// Which whitespace characters are drawn as visible marks
        render_whitespace: RenderWhitespace,
        /// Whether rows can be folded
        folding: bool,
        /// When the fold chevrons are visible
        folding_controls: FoldingControls,
        /// Whether a folded row gets a band behind it.
        fold_highlight: bool,
        /// How many regions may be folded at once.
        max_fold_regions: u32,
        /// Whether clicking past the end of a folded row unfolds it.
        unfold_on_click_after_end_of_line: bool,
        /// Whether the hover popover is shown at all.
        hover: bool,
        /// How long the pointer rests before the hover popover appears, in ms.
        hover_delay: u16,
        /// How long after the pointer leaves a symbol the popover goes away,
        /// in ms. Zero leaves it up.
        hover_hiding_delay: u16,
        /// Whether the popover stays up while the pointer is over it.
        hover_sticky: bool,
        /// Whether the popover prefers the space above the line.
        hover_above: bool,
        /// The font inlay hints are drawn in; `None` takes the editor's.
        inlay_hint_font_family: Option<SharedString>,
        /// The size inlay hints are drawn at in px; `0` is 90% of the text.
        inlay_hint_font_size: f32,
        /// Whether a chip is drawn behind an inlay hint.
        inlay_hint_background: bool,
        /// Whether the supplied inlay hints are shown with no key held.
        ///
        /// `false` together with an [`InlayModifier`] is "hidden until the key
        /// is held" -- the hints are still supplied, just out of the way.
        inlay_hints_on: bool,
        /// Which modifier flips inlay hints while it is held.
        inlay_hint_modifier: InlayModifier,
        /// Which auto-closing behaviours are on
        auto_close: AutoClose,
        /// When the matching bracket is outlined
        match_brackets: MatchBrackets,
        /// Whether bracket pairs are coloured by nesting depth
        bracket_colors: bool,
        /// Whether each bracket kind counts its depth on its own
        bracket_colors_per_type: bool,
        /// Which bracket pairs get a guide line
        bracket_guides: BracketGuides,
        /// Whether the guides get a stub at each end
        bracket_guides_horizontal: bool,
        /// Whether the pair the caret is in is drawn stronger
        highlight_active_bracket_pair: bool,
        /// Whether a space follows the comment token
        comment_insert_space: bool,
        /// Whether Enter carries a line comment onto the next line
        comment_on_newline: bool,
        /// The shape of the caret
        cursor_style: CursorStyle,
        /// How the caret blinks
        cursor_blinking: CursorBlinking,
        /// The width of a line caret in px; `0` is the built-in 1.5px
        cursor_width: u8,
        /// The height of a line caret as a percent of the line; `0` is auto
        cursor_height: u8,
        /// How many rows to keep above and below the caret when scrolling
        cursor_surrounding_lines: u8,
        /// When those rows are enforced
        surrounding_lines_style: SurroundingLinesStyle,
        /// Whether clicking near an edge scrolls to keep those rows
        autoscroll_on_clicks: bool,
        /// Whether the caret slides between positions
        caret_animation: CaretAnimation,
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
            min_line_number_digits: 4,
            render_final_newline: true,
            wrap_at: WrapAt::EditorWidth,
            rulers: Rc::from([] as [usize; 0]),
            indent_guides: true,
            render_whitespace: RenderWhitespace::default(),
            folding: true,
            folding_controls: FoldingControls::default(),
            fold_highlight: false,
            max_fold_regions: DEFAULT_MAX_FOLD_REGIONS,
            unfold_on_click_after_end_of_line: false,
            hover: true,
            hover_delay: 150,
            hover_hiding_delay: 0,
            hover_sticky: true,
            hover_above: true,
            inlay_hint_font_family: None,
            inlay_hint_font_size: 0.,
            inlay_hint_background: false,
            inlay_hints_on: true,
            inlay_hint_modifier: InlayModifier::None,
            auto_close: AutoClose::default(),
            match_brackets: MatchBrackets::default(),
            bracket_colors: true,
            bracket_colors_per_type: false,
            bracket_guides: BracketGuides::default(),
            bracket_guides_horizontal: true,
            highlight_active_bracket_pair: true,
            comment_insert_space: true,
            comment_on_newline: true,
            cursor_style: CursorStyle::default(),
            cursor_blinking: CursorBlinking::default(),
            cursor_width: 0,
            cursor_height: 0,
            cursor_surrounding_lines: DEFAULT_SURROUNDING_LINES,
            surrounding_lines_style: SurroundingLinesStyle::default(),
            autoscroll_on_clicks: false,
            caret_animation: CaretAnimation::default(),
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
    pub(super) fn comment_on_newline(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                comment_on_newline, ..
            } => *comment_on_newline,
            _ => false,
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

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn bracket_colors_per_type(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                bracket_colors_per_type,
                ..
            } => *bracket_colors_per_type,
            _ => false,
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn bracket_guides_horizontal(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                bracket_guides_horizontal,
                ..
            } => *bracket_guides_horizontal,
            _ => false,
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn highlight_active_bracket_pair(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                highlight_active_bracket_pair,
                ..
            } => *highlight_active_bracket_pair,
            _ => false,
        }
    }

    /// Return [`CaretAnimation::Off`] if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn caret_animation(&self) -> CaretAnimation {
        match self {
            InputMode::CodeEditor {
                caret_animation, ..
            } => *caret_animation,
            _ => CaretAnimation::Off,
        }
    }

    /// Return [`CursorStyle::Line`] if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn cursor_style(&self) -> CursorStyle {
        match self {
            InputMode::CodeEditor { cursor_style, .. } => *cursor_style,
            _ => CursorStyle::Line,
        }
    }

    /// Return [`CursorBlinking::Blink`] if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn cursor_blinking(&self) -> CursorBlinking {
        match self {
            InputMode::CodeEditor {
                cursor_blinking, ..
            } => *cursor_blinking,
            _ => CursorBlinking::Blink,
        }
    }

    /// The caret width in px, or `None` for the built-in one.
    #[allow(unused)]
    #[inline]
    pub(super) fn cursor_width(&self) -> Option<Pixels> {
        match self {
            InputMode::CodeEditor { cursor_width, .. } if *cursor_width > 0 => {
                Some(px(f32::from(*cursor_width)))
            }
            _ => None,
        }
    }

    /// The caret height as a percent of the line, or `0` for auto.
    #[allow(unused)]
    #[inline]
    pub(super) fn cursor_height(&self) -> u8 {
        match self {
            InputMode::CodeEditor { cursor_height, .. } => *cursor_height,
            _ => 0,
        }
    }

    /// Return [`DEFAULT_MIN_LINE_NUMBER_DIGITS`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn min_line_number_digits(&self) -> usize {
        match self {
            InputMode::CodeEditor {
                min_line_number_digits,
                ..
            } => *min_line_number_digits,
            _ => DEFAULT_MIN_LINE_NUMBER_DIGITS,
        }
    }

    /// Return `true` if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn render_final_newline(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                render_final_newline,
                ..
            } => *render_final_newline,
            _ => true,
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn fold_highlight(&self) -> bool {
        match self {
            InputMode::CodeEditor { fold_highlight, .. } => *fold_highlight,
            _ => false,
        }
    }

    /// Return [`DEFAULT_MAX_FOLD_REGIONS`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn max_fold_regions(&self) -> usize {
        let n = match self {
            InputMode::CodeEditor {
                max_fold_regions, ..
            } => *max_fold_regions,
            _ => DEFAULT_MAX_FOLD_REGIONS,
        };
        n as usize
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn unfold_on_click_after_end_of_line(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                unfold_on_click_after_end_of_line,
                ..
            } => *unfold_on_click_after_end_of_line,
            _ => false,
        }
    }

    /// Return true if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn hover(&self) -> bool {
        match self {
            InputMode::CodeEditor { hover, .. } => *hover,
            _ => true,
        }
    }

    /// Return [`DEFAULT_HOVER_DELAY`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn hover_delay(&self) -> u16 {
        match self {
            InputMode::CodeEditor { hover_delay, .. } => *hover_delay,
            _ => DEFAULT_HOVER_DELAY,
        }
    }

    /// Return [`DEFAULT_HOVER_HIDING_DELAY`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn hover_hiding_delay(&self) -> u16 {
        match self {
            InputMode::CodeEditor {
                hover_hiding_delay, ..
            } => *hover_hiding_delay,
            _ => DEFAULT_HOVER_HIDING_DELAY,
        }
    }

    /// Return true if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn hover_sticky(&self) -> bool {
        match self {
            InputMode::CodeEditor { hover_sticky, .. } => *hover_sticky,
            _ => true,
        }
    }

    /// Return true if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn hover_above(&self) -> bool {
        match self {
            InputMode::CodeEditor { hover_above, .. } => *hover_above,
            _ => true,
        }
    }

    /// Return `None` if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn inlay_hint_font_family(&self) -> Option<SharedString> {
        match self {
            InputMode::CodeEditor {
                inlay_hint_font_family,
                ..
            } => inlay_hint_font_family.clone(),
            _ => None,
        }
    }

    /// Return `0.` if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn inlay_hint_font_size(&self) -> f32 {
        match self {
            InputMode::CodeEditor {
                inlay_hint_font_size,
                ..
            } => *inlay_hint_font_size,
            _ => 0.,
        }
    }

    /// Return true if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn inlay_hints_on(&self) -> bool {
        match self {
            InputMode::CodeEditor { inlay_hints_on, .. } => *inlay_hints_on,
            _ => true,
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn inlay_hint_background(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                inlay_hint_background,
                ..
            } => *inlay_hint_background,
            _ => false,
        }
    }

    /// Return [`InlayModifier::None`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn inlay_hint_modifier(&self) -> InlayModifier {
        match self {
            InputMode::CodeEditor {
                inlay_hint_modifier,
                ..
            } => *inlay_hint_modifier,
            _ => InlayModifier::None,
        }
    }

    /// Return [`WrapAt::EditorWidth`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn wrap_at(&self) -> WrapAt {
        match self {
            InputMode::CodeEditor { wrap_at, .. } => *wrap_at,
            _ => WrapAt::EditorWidth,
        }
    }

    /// Return an empty slice if the mode is not [`InputMode::CodeEditor`].
    #[inline]
    pub(super) fn rulers(&self) -> &[usize] {
        match self {
            InputMode::CodeEditor { rulers, .. } => rulers,
            _ => &[],
        }
    }

    /// Return [`DEFAULT_SURROUNDING_LINES`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn cursor_surrounding_lines(&self) -> u8 {
        match self {
            InputMode::CodeEditor {
                cursor_surrounding_lines,
                ..
            } => *cursor_surrounding_lines,
            _ => DEFAULT_SURROUNDING_LINES,
        }
    }

    /// Return [`SurroundingLinesStyle::OnMove`] if the mode is not
    /// [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn surrounding_lines_style(&self) -> SurroundingLinesStyle {
        match self {
            InputMode::CodeEditor {
                surrounding_lines_style,
                ..
            } => *surrounding_lines_style,
            _ => SurroundingLinesStyle::OnMove,
        }
    }

    /// Return false if the mode is not [`InputMode::CodeEditor`].
    #[allow(unused)]
    #[inline]
    pub(super) fn autoscroll_on_clicks(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                autoscroll_on_clicks,
                ..
            } => *autoscroll_on_clicks,
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
    use gpui::px;
    use ropey::Rope;
    use std::rc::Rc;

    use crate::{
        highlighter::DiagnosticSet,
        input::{
            AutoClose, BracketGuides, CaretAnimation, CursorBlinking, CursorStyle, MatchBrackets,
            SurroundingLinesStyle, TabSize,
            mode::{
                DEFAULT_MAX_FOLD_REGIONS, DEFAULT_SURROUNDING_LINES, FoldingControls,
                InlayModifier, InputMode, LineNumbers, RenderWhitespace, WrapAt,
            },
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
        assert!(!mode.bracket_colors_per_type(), "one pool like VS Code");
        assert!(mode.bracket_guides_horizontal());
        assert!(mode.highlight_active_bracket_pair());
        assert!(mode.comment_insert_space(), "`// a` like VS Code");
        assert!(mode.comment_on_newline(), "Enter keeps the comment going");
        // Every caret default is the caret we had before it was settable.
        assert_eq!(mode.cursor_style(), CursorStyle::Line);
        assert_eq!(mode.cursor_blinking(), CursorBlinking::Blink);
        assert_eq!(mode.cursor_width(), None, "the built-in 1.5px");
        assert_eq!(mode.cursor_height(), 0, "auto, from the input size");
        assert_eq!(mode.cursor_surrounding_lines(), 3, "was a bare 3");
        assert_eq!(mode.surrounding_lines_style(), SurroundingLinesStyle::OnMove);
        assert!(!mode.autoscroll_on_clicks(), "off like Zed");
        assert_eq!(
            mode.wrap_at(),
            WrapAt::EditorWidth,
            "soft wrap broke at the viewport before this was settable"
        );
        assert!(mode.rulers().is_empty(), "no rulers like VS Code");
        assert!(mode.hover(), "the popover was always shown before this");
        // Inlay hints draw nothing until someone supplies rows, so the
        // defaults only have to keep the look they had with none.
        assert!(mode.inlay_hints_on(), "no modifier to hold, so shown");
        assert_eq!(mode.inlay_hint_modifier(), InlayModifier::None);
        assert_eq!(mode.inlay_hint_font_family(), None, "the editor's font");
        assert_eq!(mode.inlay_hint_font_size(), 0., "90% of the code");
        assert!(!mode.inlay_hint_background(), "no chip like VS Code");
        assert_eq!(mode.hover_delay(), 150, "was a bare 150");
        assert_eq!(
            mode.hover_hiding_delay(),
            0,
            "nothing hid the popover on leave before this"
        );
        assert!(mode.hover_sticky());
        assert!(mode.hover_above(), "the placement already tried above first");
        assert_eq!(
            mode.caret_animation(),
            CaretAnimation::Off,
            "the caret jumped before this existed"
        );
        assert_eq!(mode.language_name(), "rust");
        assert_eq!(mode.max_rows(), usize::MAX);
        assert_eq!(mode.min_rows(), 1);

        let mode = InputMode::CodeEditor {
            min_line_number_digits: 4,
            render_final_newline: true,
            wrap_at: WrapAt::EditorWidth,
            rulers: Rc::from([] as [usize; 0]),
            multi_line: false,
            line_number: LineNumbers::On,
            indent_guides: true,
            render_whitespace: RenderWhitespace::None,
            folding: true,
            folding_controls: FoldingControls::default(),
            fold_highlight: false,
            max_fold_regions: DEFAULT_MAX_FOLD_REGIONS,
            unfold_on_click_after_end_of_line: false,
            hover: true,
            hover_delay: 150,
            hover_hiding_delay: 0,
            hover_sticky: true,
            hover_above: true,
            inlay_hint_font_family: None,
            inlay_hint_font_size: 0.,
            inlay_hint_background: false,
            inlay_hints_on: true,
            inlay_hint_modifier: InlayModifier::None,
            auto_close: AutoClose::default(),
            match_brackets: MatchBrackets::default(),
            bracket_colors: true,
            bracket_colors_per_type: false,
            bracket_guides: BracketGuides::default(),
            bracket_guides_horizontal: true,
            highlight_active_bracket_pair: true,
            comment_insert_space: true,
            comment_on_newline: true,
            cursor_style: CursorStyle::Block,
            cursor_blinking: CursorBlinking::Solid,
            cursor_width: 3,
            cursor_height: 60,
            cursor_surrounding_lines: 0,
            surrounding_lines_style: SurroundingLinesStyle::Always,
            autoscroll_on_clicks: true,
            caret_animation: CaretAnimation::On,
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
        // Set explicitly above, so they read back rather than falling to the
        // non-code-editor defaults.
        assert_eq!(mode.cursor_style(), CursorStyle::Block);
        assert_eq!(mode.cursor_blinking(), CursorBlinking::Solid);
        assert_eq!(mode.cursor_width(), Some(px(3.)));
        assert_eq!(mode.cursor_height(), 60);
        assert_eq!(mode.cursor_surrounding_lines(), 0);
        assert_eq!(mode.surrounding_lines_style(), SurroundingLinesStyle::Always);
        assert!(mode.autoscroll_on_clicks());
        assert_eq!(mode.caret_animation(), CaretAnimation::On);
    }

    #[test]
    fn a_plain_input_keeps_the_caret_it_always_had() {
        let mode = InputMode::plain_text();
        assert_eq!(mode.cursor_style(), CursorStyle::Line);
        assert_eq!(mode.cursor_blinking(), CursorBlinking::Blink);
        assert_eq!(mode.cursor_width(), None);
        assert_eq!(mode.cursor_height(), 0);
        assert_eq!(mode.cursor_surrounding_lines(), DEFAULT_SURROUNDING_LINES);
        assert_eq!(mode.surrounding_lines_style(), SurroundingLinesStyle::OnMove);
        assert!(!mode.autoscroll_on_clicks());
        assert_eq!(mode.caret_animation(), CaretAnimation::Off);
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

    /// The modifier **flips** what the editor is doing, the way Zed's
    /// `toggle_on_modifiers_press` does -- it is not a plain "show while
    /// held", or turning the hints on would make the key do nothing.
    #[test]
    fn the_modifier_flips_the_hints_either_way() {
        let held = gpui::Modifiers {
            alt: true,
            ..Default::default()
        };
        let none = gpui::Modifiers::default();

        assert!(InlayModifier::Alt.held(&held));
        assert!(!InlayModifier::Alt.held(&none));
        assert!(!InlayModifier::Control.held(&held), "a different key");
        assert!(
            !InlayModifier::None.held(&held),
            "no key flips them when none is set"
        );
    }
}
