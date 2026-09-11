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
mod input;
mod lsp;
mod mask_pattern;
mod mode;
mod movement;
mod number_input;
mod otp_input;
pub(crate) mod popovers;
mod rope_ext;
mod search;
mod state;
mod tags;
mod text_wrapper;
mod whitespace;
mod selection;

pub(crate) use clear_button::*;
pub use caret::{
    appearance as caret_appearance, CursorBlinking, CursorStyle, SurroundingLinesStyle,
};
pub use cursor::*;
pub use indent::TabSize;
pub use input::*;
pub use lsp::*;
pub use mask_pattern::MaskPattern;
pub use brackets::{AutoClose, BracketGuides, MatchBrackets, Pair};
pub use comment::CommentTokens;
pub use mode::{FoldingControls, LineNumbers, RenderWhitespace};
pub use number_input::{NumberInput, NumberInputEvent, StepAction};
pub use otp_input::*;
pub use state::*;

pub use lsp_types::Position;
pub use rope_ext::*;
pub use ropey::Rope;
