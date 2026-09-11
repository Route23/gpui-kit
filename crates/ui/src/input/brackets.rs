//! Auto-closing brackets and quotes.
//!
//! Everything here is pure so it can be unit tested; the state that drives it
//! lives on [`InputState`](super::InputState).
//!
//! The pairs come from a **table in this file**, not from the syntax tree:
//! tree-sitter grammars do not describe brackets, and every editor that has
//! this feature keeps its own table (VS Code's `language-configuration.json`,
//! Zed's `config.toml`).

use std::ops::Range;

use gpui::Hsla;
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

/// When the bracket that matches the caret's is outlined.
///
/// Mirrors VS Code's `editor.matchBrackets`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MatchBrackets {
    /// Never outline anything.
    Never,
    /// Only when the caret is right next to a bracket.
    Near,
    /// Also outline the innermost pair the caret sits inside.
    #[default]
    Always,
}

impl From<bool> for MatchBrackets {
    fn from(on: bool) -> Self {
        if on {
            MatchBrackets::Always
        } else {
            MatchBrackets::Never
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

/// The pair to outline for a caret at `offset`, as `(opener, closer)` byte
/// ranges.
///
/// `bounds` is how far the scan may go — pass the visible byte range. A partner
/// outside it cannot be drawn anyway, and scanning the whole buffer on every
/// frame would be wasteful.
///
/// **Strings and comments are not excluded.** A `(` inside `"a (b"` counts.
/// Telling them apart needs the syntax tree, which is not reachable from the
/// input state today; VS Code has the same gap when no tokenizer has run yet.
pub fn match_at(
    text: &Rope,
    offset: usize,
    language: &str,
    mode: MatchBrackets,
    bounds: Range<usize>,
    skip: &[Range<usize>],
) -> Option<(Range<usize>, Range<usize>)> {
    if matches!(mode, MatchBrackets::Never) {
        return None;
    }
    let pairs = pairs_for(language);
    // Quotes have no direction, so they cannot be counted — only brackets.
    let pairs: Vec<Pair> = pairs.into_iter().filter(|p| p.open != p.close).collect();

    // The character after the caret first, then the one before it: that is the
    // order VS Code resolves ties in.
    if let Some(next) = text.char_at(offset) {
        if !is_skipped(offset, skip) {
            if let Some(found) = partner_of(text, offset, next, &pairs, &bounds, skip) {
                return Some(found);
            }
        }
    }
    if let Some(prev) = prev_char(text, offset) {
        let start = offset - prev.len_utf8();
        if !is_skipped(start, skip) {
            if let Some(found) = partner_of(text, start, prev, &pairs, &bounds, skip) {
                return Some(found);
            }
        }
    }

    if matches!(mode, MatchBrackets::Near) {
        return None;
    }
    enclosing(text, offset, &pairs, &bounds, skip)
}

/// Whether `offset` falls inside a string or a comment.
///
/// `skip` is sorted and merged (see `SyntaxHighlighter::skipped_ranges`), so a
/// binary search would do; the list is a handful of entries per screen, and a
/// scan keeps the caller free to pass anything sorted.
fn is_skipped(offset: usize, skip: &[Range<usize>]) -> bool {
    skip.iter().any(|r| r.contains(&offset))
}

/// The pair `ch` at `at` belongs to, scanning in the direction it opens.
fn partner_of(
    text: &Rope,
    at: usize,
    ch: char,
    pairs: &[Pair],
    bounds: &Range<usize>,
    skip: &[Range<usize>],
) -> Option<(Range<usize>, Range<usize>)> {
    if let Some(p) = pairs.iter().find(|p| p.open == ch) {
        let close = scan_forward(text, at + ch.len_utf8(), *p, bounds, skip)?;
        return Some((at..at + ch.len_utf8(), close..close + p.close.len_utf8()));
    }
    if let Some(p) = pairs.iter().find(|p| p.close == ch) {
        let open = scan_backward(text, at, *p, bounds, skip)?;
        return Some((open..open + p.open.len_utf8(), at..at + ch.len_utf8()));
    }
    None
}

/// The innermost pair that encloses `offset`.
fn enclosing(
    text: &Rope,
    offset: usize,
    pairs: &[Pair],
    bounds: &Range<usize>,
    skip: &[Range<usize>],
) -> Option<(Range<usize>, Range<usize>)> {
    // Walk back to the nearest opener that is still unclosed, whichever kind it
    // is, then find its partner going forward.
    let start = bounds.start.min(offset);
    let mut depth = vec![0usize; pairs.len()];
    let mut at = offset;
    while at > start {
        let ch = prev_char_from(text, at)?;
        at -= ch.len_utf8();
        if is_skipped(at, skip) {
            continue;
        }
        if let Some(i) = pairs.iter().position(|p| p.close == ch) {
            depth[i] += 1;
        } else if let Some(i) = pairs.iter().position(|p| p.open == ch) {
            if depth[i] == 0 {
                let p = pairs[i];
                let close = scan_forward(text, at + ch.len_utf8(), p, bounds, skip)?;
                return Some((at..at + ch.len_utf8(), close..close + p.close.len_utf8()));
            }
            depth[i] -= 1;
        }
    }
    None
}

/// The offset of `p.close` that matches an opener just before `from`.
fn scan_forward(
    text: &Rope,
    from: usize,
    p: Pair,
    bounds: &Range<usize>,
    skip: &[Range<usize>],
) -> Option<usize> {
    let end = bounds.end.min(text.len());
    let mut depth = 0usize;
    let mut at = from;
    while at < end {
        let ch = text.char_at(at)?;
        if is_skipped(at, skip) {
            at += ch.len_utf8();
            continue;
        }
        if ch == p.open {
            depth += 1;
        } else if ch == p.close {
            if depth == 0 {
                return Some(at);
            }
            depth -= 1;
        }
        at += ch.len_utf8();
    }
    None
}

/// The offset of `p.open` that matches a closer at `from`.
fn scan_backward(
    text: &Rope,
    from: usize,
    p: Pair,
    bounds: &Range<usize>,
    skip: &[Range<usize>],
) -> Option<usize> {
    let start = bounds.start.min(from);
    let mut depth = 0usize;
    let mut at = from;
    while at > start {
        let ch = prev_char_from(text, at)?;
        at -= ch.len_utf8();
        if is_skipped(at, skip) {
            continue;
        }
        if ch == p.close {
            depth += 1;
        } else if ch == p.open {
            if depth == 0 {
                return Some(at);
            }
            depth -= 1;
        }
    }
    None
}

/// The character ending at `at`.
fn prev_char_from(text: &Rope, at: usize) -> Option<char> {
    if at == 0 {
        return None;
    }
    text.chars_at(at).reversed().next()
}

/// Every bracket in `range`, with the nesting depth it sits at.
///
/// Depth counts from 0 **inside `range`**: the screen usually starts in the
/// middle of a file, and a scan that tried to recover the true depth would have
/// to read from the top of the buffer on every frame. A closer with nothing
/// open in front of it gets no depth at all (it is left uncoloured).
///
/// `skip` is the string and comment spans — brackets in there are not code.
pub fn depths_in(
    text: &Rope,
    range: Range<usize>,
    language: &str,
    skip: &[Range<usize>],
) -> Vec<(Range<usize>, usize)> {
    let pairs: Vec<Pair> = pairs_for(language)
        .into_iter()
        .filter(|p| p.open != p.close)
        .collect();
    let end = range.end.min(text.len());
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut at = range.start;
    while at < end {
        let Some(ch) = text.char_at(at) else { break };
        let len = ch.len_utf8();
        if is_skipped(at, skip) {
            at += len;
            continue;
        }
        if pairs.iter().any(|p| p.open == ch) {
            out.push((at..at + len, depth));
            depth += 1;
        } else if pairs.iter().any(|p| p.close == ch) {
            if depth > 0 {
                depth -= 1;
                out.push((at..at + len, depth));
            }
        }
        at += len;
    }
    out
}

/// The smallest saturation a coloured bracket gets.
///
/// Most themes paint brackets in the plain foreground colour, which is close to
/// grey — and **rotating the hue of a grey changes nothing**. Deeper levels
/// therefore get at least this much saturation so the hues are actually
/// visible. Lightness is never touched, so contrast against the background is
/// whatever the theme chose.
const MIN_DEPTH_SATURATION: f32 = 0.45;

/// The colour for a bracket `depth` levels in.
///
/// The hue moves and the saturation is floored; **lightness stays where the
/// theme put it**, so the colours keep their contrast in light themes as well
/// as dark ones. **Depth 0 is the base colour unchanged**, which keeps the
/// common case looking exactly like it did before colouring existed.
pub fn depth_color(base: Hsla, depth: usize) -> Hsla {
    if depth == 0 {
        return base;
    }
    // Roughly 100° a step: far enough apart to tell three levels apart at a
    // glance, and it comes back near the base only after five.
    let h = (base.h + 0.28 * depth as f32).fract();
    Hsla {
        h,
        s: base.s.max(MIN_DEPTH_SATURATION),
        ..base
    }
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

    /// `|` marks the caret; the whole text is in scope for the scan.
    #[track_caller]
    fn matched(text: &str, mode: MatchBrackets) -> Option<(Range<usize>, Range<usize>)> {
        let offset = text.find('|').expect("mark the caret with |");
        let rope = Rope::from(text.replace('|', ""));
        let len = rope.len();
        match_at(&rope, offset, "rust", mode, 0..len, &[])
    }

    #[test]
    fn outlines_the_pair_on_either_side_of_the_caret() {
        // In front of the opener.
        assert_eq!(matched("f|(x)", MatchBrackets::Near), Some((1..2, 3..4)));
        // Right after the closer.
        assert_eq!(matched("f(x)|", MatchBrackets::Near), Some((1..2, 3..4)));
        // Inside, right after the opener: still "next to a bracket".
        assert_eq!(matched("f(|x)", MatchBrackets::Near), Some((1..2, 3..4)));
    }

    #[test]
    fn counts_nesting() {
        assert_eq!(matched("|((a))", MatchBrackets::Near), Some((0..1, 4..5)));
        assert_eq!(matched("(|(a))", MatchBrackets::Near), Some((1..2, 3..4)));
    }

    #[test]
    fn spans_lines() {
        let out = matched("fn f() |{\n    x\n}\n", MatchBrackets::Near);
        assert_eq!(out, Some((7..8, 15..16)));
    }

    #[test]
    fn an_unmatched_bracket_lights_nothing() {
        assert_eq!(matched("|(a", MatchBrackets::Near), None);
        assert_eq!(matched("a)|", MatchBrackets::Near), None);
    }

    #[test]
    fn always_finds_the_pair_around_the_caret() {
        assert_eq!(matched("f(a|b)", MatchBrackets::Always), Some((1..2, 4..5)));
        // The innermost one wins.
        assert_eq!(matched("((a|b))", MatchBrackets::Always), Some((1..2, 4..5)));
        // ...and `near` still says no.
        assert_eq!(matched("f(a|b)", MatchBrackets::Near), None);
    }

    #[test]
    fn never_never_matches() {
        assert_eq!(matched("|(x)", MatchBrackets::Never), None);
    }

    #[test]
    fn the_scan_stops_at_the_bounds() {
        let rope = Rope::from("(aaaaaaaaaa)");
        assert_eq!(
            match_at(&rope, 0, "rust", MatchBrackets::Near, 0..12, &[]),
            Some((0..1, 11..12))
        );
        assert_eq!(
            match_at(&rope, 0, "rust", MatchBrackets::Near, 0..5, &[]),
            None,
            "the partner is off screen, so there is nothing to draw"
        );
    }

    #[test]
    fn a_bracket_in_a_string_is_not_code() {
        // `f("(" )` — the `(` inside the string must not take part.
        let text = "f(\"(\")";
        let rope = Rope::from(text);
        let len = rope.len();
        let skip = vec![2..5];
        // From the real `(` the partner is the last char, not the one in the string.
        assert_eq!(
            match_at(&rope, 1, "rust", MatchBrackets::Near, 0..len, &skip),
            Some((1..2, 5..6))
        );
        // Standing next to the one inside the string lights nothing.
        assert_eq!(
            match_at(&rope, 3, "rust", MatchBrackets::Near, 0..len, &skip),
            None
        );
        // Without the skip list the string's `(` eats the real closer and the
        // outer pair finds nothing at all — that was the bug.
        assert_eq!(
            match_at(&rope, 1, "rust", MatchBrackets::Near, 0..len, &[]),
            None
        );
    }

    #[test]
    fn depths_count_from_zero_inside_the_range() {
        let rope = Rope::from("((a)b)");
        let out = depths_in(&rope, 0..rope.len(), "rust", &[]);
        assert_eq!(
            out,
            vec![(0..1, 0), (1..2, 1), (3..4, 1), (5..6, 0)],
            "openers and their closers share a depth"
        );
    }

    #[test]
    fn a_closer_with_nothing_open_gets_no_colour() {
        let rope = Rope::from(")a(");
        let out = depths_in(&rope, 0..rope.len(), "rust", &[]);
        assert_eq!(out, vec![(2..3, 0)], "the stray closer is left alone");
    }

    #[test]
    fn depths_skip_strings() {
        let rope = Rope::from("(\"(\")");
        let out = depths_in(&rope, 0..rope.len(), "rust", &[1..4]);
        assert_eq!(out, vec![(0..1, 0), (4..5, 0)]);
    }

    #[test]
    fn depth_zero_keeps_the_base_colour() {
        let base = Hsla { h: 0.1, s: 0.5, l: 0.6, a: 1.0 };
        assert_eq!(depth_color(base, 0), base);
        let one = depth_color(base, 1);
        assert_ne!(one.h, base.h, "the hue moves");
        assert_eq!((one.l, one.a), (base.l, base.a), "lightness is left alone");
        assert!((0.0..1.0).contains(&depth_color(base, 7).h), "the hue stays in range");

        // A grey base would show no hue at all, so deeper levels get saturated.
        let grey = Hsla { h: 0.0, s: 0.02, l: 0.9, a: 1.0 };
        assert_eq!(depth_color(grey, 0), grey, "the outermost level is untouched");
        assert!(depth_color(grey, 1).s >= MIN_DEPTH_SATURATION);
        assert_eq!(depth_color(grey, 1).l, grey.l, "still as bright as the theme wanted");
    }

    #[test]
    fn quotes_are_not_counted() {
        // Quotes have no direction, so they are never outlined.
        assert_eq!(matched("|\"a\"", MatchBrackets::Near), None);
    }

    #[test]
    fn a_language_that_drops_a_quote_still_overtypes_the_rest() {
        assert!(!is_closer('\'', "rust"));
        assert!(is_closer('\'', "javascript"));
        assert!(is_closer(')', "rust"));
    }
}
