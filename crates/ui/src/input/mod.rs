mod blink_cursor;
mod brackets;
mod caret;
mod change;
mod comment;
mod clear_button;
mod cursor;
mod element;
mod fold;
mod indent;
mod inlay;
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
mod links;
mod search;
mod state;
mod tags;
mod text_wrapper;
mod whitespace;
mod selection;

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
pub use inlay::InlayRow;
pub use inline_diag::InlineDiagnostic;
pub use mode::{
    UnicodeHighlight,
    FoldingControls, InlayModifier, LineHighlight, LineNumbers, RenderWhitespace, WrapAt,
};
pub use number_input::{NumberInput, NumberInputEvent, StepAction};
pub use otp_input::*;
pub use state::*;

pub use lsp_types::Position;
pub use search::{SearchBehavior, SearchOptions};
pub use rope_ext::*;
pub use ropey::Rope;
