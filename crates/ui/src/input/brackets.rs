//! Auto-closing brackets and quotes.
//!
//! Everything here is pure so it can be unit tested; the state that drives it
//! lives on [`InputState`](super::InputState).
//!
//! The pairs come from a **table in this file**, not from the syntax tree:
//! tree-sitter grammars do not describe brackets, and every editor that has
//! this feature keeps its own table (VS Code's `language-configuration.json`,
//! Zed's `config.toml`).

use ropey::Rope;

use crate::input::RopeExt as _;

/// Which of the three auto-closing behaviours are on.
///
/// Defaults match VS Code and Zed: all on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoClose {
    /// Typing an opener inserts its closer, and backspacing between a pair
    /// removes both.
    pub brackets: bool,
    /// Typing an opener with a selection wraps the selection instead of
    /// replacing it.
    pub surround: bool,
    /// Typing a closer that is already in front of the caret steps over it
    /// instead of inserting a second one.
    pub overtype: bool,
}

impl Default for AutoClose {
    fn default() -> Self {
        Self {
            brackets: true,
            surround: true,
            overtype: true,
        }
    }
}

impl From<bool> for AutoClose {
    fn from(on: bool) -> Self {
        if on {
            Self::default()
        } else {
            Self {
                brackets: false,
                surround: false,
                overtype: false,
            }
        }
    }
}

/// What auto-closing wants to do instead of the plain insertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum AutoCloseEdit {
    /// Write `text` (the pair) and pull the caret back inside it.
    Insert { text: String, caret_back: usize },
    /// Write `text` (opener + selection + closer) and keep the inner text
    /// selected.
    Surround {
        text: String,
        open_len: usize,
        inner_len: usize,
    },
    /// Write nothing; step the caret over the closer already in front of it.
    Overtype { to: usize },
}

impl AutoCloseEdit {
    pub(super) fn text(&self) -> &str {
        match self {
            AutoCloseEdit::Insert { text, .. } | AutoCloseEdit::Surround { text, .. } => text,
            AutoCloseEdit::Overtype { .. } => "",
        }
    }
}

/// An opener and the closer it takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pair {
    pub open: char,
    pub close: char,
}

const fn pair(open: char, close: char) -> Pair {
    Pair { open, close }
}

/// Brackets every language gets.
const BRACKETS: [Pair; 3] = [pair('(', ')'), pair('[', ']'), pair('{', '}')];

/// Quotes most languages get. A language can drop one (see [`pairs_for`]).
const QUOTES: [Pair; 3] = [pair('"', '"'), pair('\'', '\''), pair('`', '`')];

/// The pairs `language` uses.
///
/// The name is whatever was passed to `InputState::code_editor`.
///
/// Languages that give a quote another meaning drop it: Rust's `'` starts a
/// lifetime, so closing it turns `&'a str` into `&''a str` — the single most
/// complained-about auto-close bug in every editor that gets this wrong.
pub fn pairs_for(language: &str) -> Vec<Pair> {
    let mut out = BRACKETS.to_vec();
    for q in QUOTES {
        if quote_is_ambiguous(language, q.open) {
            continue;
        }
        out.push(q);
    }
    out
}

/// Whether `quote` means something other than "a string starts here".
fn quote_is_ambiguous(language: &str, quote: char) -> bool {
    match quote {
        // Lifetimes (Rust), char literals with no closer in OCaml/F# patterns.
        '\'' => matches!(language, "rust" | "ocaml" | "fsharp" | "haskell" | "lisp"),
        // Markdown and shells use the backtick in prose/substitution; closing it
        // is still what people want there, so nothing drops it today.
        _ => false,
    }
}

/// The closer `ch` opens in `language`, whatever the caret sits in front of.
///
/// Used when wrapping a selection: there the context after the caret is the
/// selection itself, so the "only before whitespace" rule does not apply.
pub fn closer_of(ch: char, language: &str) -> Option<char> {
    pairs_for(language)
        .into_iter()
        .find(|p| p.open == ch)
        .map(|p| p.close)
}

/// The closer to insert after `ch`, or `None` to type `ch` on its own.
///
/// `offset` is the caret, in bytes.
///
/// Only closes when the caret sits before whitespace, a closer, or the end of
/// the line. Typing `(` in front of an identifier means "wrap what follows",
/// and a closer there would land in the wrong place (VS Code's
/// `beforeWhitespace`, which is also how Zed behaves).
///
/// Quotes additionally refuse to close right after a word character, so typing
/// an apostrophe in `don't` does not produce `don''t`.
pub fn close_for(text: &Rope, offset: usize, ch: char, language: &str) -> Option<char> {
    let p = pairs_for(language).into_iter().find(|p| p.open == ch)?;
    let next = text.char_at(offset);
    let opens_and_closes_alike = p.open == p.close;

    if opens_and_closes_alike {
        // `""` and `''` are their own closer, so a lone one in front of a word
        // is almost always the *end* of something the user is editing.
        if let Some(prev) = prev_char(text, offset) {
            if prev.is_alphanumeric() || prev == '_' || prev == p.close {
                return None;
            }
        }
    }

    match next {
        None => Some(p.close),
        Some(c) if c.is_whitespace() => Some(p.close),
        Some(c) if is_closer(c, language) => Some(p.close),
        _ => None,
    }
}

/// Whether typing `ch` should step over the character in front of the caret
/// instead of inserting one.
pub fn is_overtype(text: &Rope, offset: usize, ch: char, language: &str) -> bool {
    if !is_closer(ch, language) {
        return false;
    }
    text.char_at(offset) == Some(ch)
}

/// Whether the caret sits between an opener and its own closer, so backspace
/// should take both.
pub fn is_inside_pair(text: &Rope, offset: usize, language: &str) -> bool {
    let (Some(prev), Some(next)) = (prev_char(text, offset), text.char_at(offset)) else {
        return false;
    };
    pairs_for(language)
        .into_iter()
        .any(|p| p.open == prev && p.close == next)
}

/// Whether `ch` closes one of `language`'s pairs.
fn is_closer(ch: char, language: &str) -> bool {
    pairs_for(language).into_iter().any(|p| p.close == ch)
}

/// The character before `offset`, or `None` at the start of the text.
fn prev_char(text: &Rope, offset: usize) -> Option<char> {
    if offset == 0 {
        return None;
    }
    text.chars_at(offset).reversed().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `|` marks the caret.
    #[track_caller]
    fn close(text: &str, ch: char, language: &str) -> Option<char> {
        let offset = text.find('|').expect("mark the caret with |");
        let rope = Rope::from(text.replace('|', ""));
        close_for(&rope, offset, ch, language)
    }

    #[test]
    fn closes_at_the_end_of_a_line() {
        assert_eq!(close("let x = |", '(', "rust"), Some(')'));
        assert_eq!(close("let x = |", '[', "rust"), Some(']'));
        assert_eq!(close("let x = |", '{', "rust"), Some('}'));
        assert_eq!(close("let x = |", '"', "rust"), Some('"'));
    }

    #[test]
    fn closes_before_whitespace_and_before_a_closer() {
        assert_eq!(close("f(| )", '(', "rust"), Some(')'));
        assert_eq!(close("f(|)", '(', "rust"), Some(')'));
        assert_eq!(close("f(|]", '(', "rust"), Some(')'));
    }

    #[test]
    fn does_not_close_in_front_of_a_word() {
        assert_eq!(close("|foo", '(', "rust"), None, "wrap what follows instead");
        assert_eq!(close("|foo", '"', "rust"), None);
    }

    #[test]
    fn a_quote_after_a_word_is_not_an_opener() {
        assert_eq!(close("don|", '\'', "javascript"), None, "don't");
        assert_eq!(close("x = |", '\'', "javascript"), Some('\''));
    }

    #[test]
    fn rust_lifetimes_never_close() {
        assert_eq!(close("&|", '\'', "rust"), None);
        // ...but the other pairs still work in Rust.
        assert_eq!(close("&|", '"', "rust"), Some('"'));
    }

    #[test]
    fn closer_of_ignores_what_follows() {
        assert_eq!(closer_of('(', "rust"), Some(')'));
        assert_eq!(closer_of('"', "rust"), Some('"'));
        assert_eq!(closer_of('\'', "rust"), None, "lifetimes");
        assert_eq!(closer_of(')', "rust"), None, "closers do not open");
    }

    #[test]
    fn overtype_only_when_the_same_closer_is_in_front() {
        let rope = Rope::from("f()");
        assert!(is_overtype(&rope, 2, ')', "rust"));
        assert!(!is_overtype(&rope, 1, ')', "rust"));
        assert!(!is_overtype(&rope, 2, '(', "rust"), "openers never overtype");
    }

    #[test]
    fn inside_pair_needs_both_halves() {
        let rope = Rope::from("f()");
        assert!(is_inside_pair(&rope, 2, "rust"));
        assert!(!is_inside_pair(&rope, 3, "rust"));
        let rope = Rope::from("f(x)");
        assert!(!is_inside_pair(&rope, 3, "rust"));
    }

    #[test]
    fn a_language_that_drops_a_quote_still_overtypes_the_rest() {
        assert!(!is_closer('\'', "rust"));
        assert!(is_closer('\'', "javascript"));
        assert!(is_closer(')', "rust"));
    }
}
