mod auto_indent;
mod blink_cursor;
mod brackets;
mod caret;
mod change;
mod comment;
mod contrast;
mod clear_button;
mod cursor;
mod element;
mod expand;
mod fold;
mod diff_marks;
mod gutter_marks;
mod indent;
mod inlay;
mod lens;
mod inline_diag;
mod input;
mod lsp;
mod mask_pattern;
mod mode;
mod movement;
mod number_input;
mod otp_input;
pub(crate) mod popovers;
mod rope_ext;
mod rulers;
mod scrollbar_marks;
mod links;
mod list;
mod search;
mod semantic;
mod state;
mod tags;
mod text_wrapper;
mod whitespace;
mod selection;
mod sticky;

pub(crate) use clear_button::*;
pub use caret::{
    appearance as caret_appearance, CaretAnimation, CursorBlinking, CursorStyle,
    SurroundingLinesStyle,
};
pub use cursor::*;
pub use indent::TabSize;
pub use input::*;
pub use lsp::*;
pub use mask_pattern::MaskPattern;
pub use brackets::{AutoClose, BracketGuides, MatchBrackets, Pair};
pub use comment::CommentTokens;
pub use fold::{FoldKind, FoldRange};
pub use gutter_marks::{GutterMark, GutterMarkKind};
pub use inlay::InlayRow;
pub use lens::LensRow;
pub use inline_diag::InlineDiagnostic;
pub use semantic::SemanticSpan;
pub use mode::{
    UnicodeHighlight,
    AutoIndent, FoldingControls, GuideBackground, GuideColoring, InlayModifier, LineHighlight,
    DetailAlignment, InsertMode, KindDisplay, LineNumbers, QuickSuggestions, RenderWhitespace,
    ScrollBeyondLastLine, StickyModel, SuggestBehaviour, SuggestSelection, SuggestStyle, WhitespaceMap, WrapAt,
};
pub use popovers::SignatureHint;
pub use scrollbar_marks::{MarkSeverity, ScrollbarMarks};
pub use number_input::{NumberInput, NumberInputEvent, StepAction};
pub use otp_input::*;
pub use state::*;

pub use lsp_types::Position;
pub use search::{SearchBehavior, SearchOptions};
pub use rope_ext::*;
pub use ropey::Rope;
