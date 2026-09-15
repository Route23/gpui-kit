use std::{char, ops::Range};

use gpui::{Context, Window};
use ropey::Rope;
use sum_tree::Bias;

use crate::{input::InputState, RopeExt as _};

/// VS Code's `editor.wordSeparators`, verbatim.
///
/// Note `_` is **not** here: `foo_bar` is one word, `foo-bar` is two. UAX#29
/// agrees on both counts, which is why the two can be layered.
pub const DEFAULT_WORD_SEPARATORS: &str = "`~!@#$%^&*()-=+[{]}\\|;:\'\",.<>/?";

/// How many characters either side of the caret are examined.
///
/// A double-click should not read a megabyte to answer "what word is this".
const WORD_WINDOW: usize = 128;

/// What kind of run a character belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A separator, or punctuation: stands alone.
    Alone,
    Whitespace,
    Newline,
    /// Han. Runs of these group together.
    Han,
    Hiragana,
    Katakana,
    /// Everything else -- Latin, Greek, Cyrillic, digits, `_`, and anything
    /// the reader did not list as a separator.
    Word,
}

fn kind_of(c: char, separators: &str) -> Kind {
    if c == '\n' || c == '\r' {
        return Kind::Newline;
    }
    if c.is_whitespace() {
        return Kind::Whitespace;
    }
    // **The reader's list wins.** It is the whole point of the setting.
    if separators.contains(c) {
        return Kind::Alone;
    }
    match c {
        '\u{3040}'..='\u{309f}' => Kind::Hiragana,
        '\u{30a0}'..='\u{30ff}' | '\u{31f0}'..='\u{31ff}' => Kind::Katakana,
        '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{f900}'..='\u{faff}' => Kind::Han,
        // **Everything the reader did not call a separator is part of a
        // word.** That is VS Code's rule, and it is what makes the setting
        // mean something: take `.` out of the list and `foo.bar` becomes one
        // word. It also means an emoji joins the word beside it, as there.
        _ => Kind::Word,
    }
}

impl InputState {

    /// Select the word at the given offset on double-click.
    ///
    /// The offset is the UTF-8 offset.
    pub(super) fn select_word(&mut self, offset: usize, _: &mut Window, cx: &mut Context<Self>) {
        let separators = self.mode.word_separators();
        let Some(range) = word_range(&self.text, offset, &separators) else {
            return;
        };

        self.selected_range = (range.start..range.end).into();
        self.selected_word_range = Some(self.selected_range);
        cx.notify()
    }

    /// The word around `offset`, for the host and for the other word walks.
    pub(super) fn word_at_offset(&self, offset: usize) -> Option<Range<usize>> {
        word_range(&self.text, offset, &self.mode.word_separators())
    }
}

/// A slice of `text` around `offset`, and where it starts.
///
/// Both ends are moved to a character boundary, so the slice can be indexed.
fn window_around(text: &Rope, offset: usize) -> (String, usize) {
    let mut start = offset;
    for _ in 0..WORD_WINDOW {
        let Some(c) = text.chars_at(start).reversed().next() else {
            break;
        };
        start -= c.len_utf8();
    }
    let mut end = offset;
    for _ in 0..WORD_WINDOW {
        let Some(c) = text.chars_at(end).next() else {
            break;
        };
        end += c.len_utf8();
    }
    (text.slice(start..end).to_string(), start)
}

/// The word containing `offset` (a UTF-8 offset into `text`).
///
/// # What was wrong before
///
/// The old table counted only `is_ascii_alphanumeric` as a word character, so
/// `café` cut after `caf`, and **every Japanese word selected one character**
/// (everything non-ASCII fell into one "other" bucket that refused to join
/// itself). The two were the same bug seen from two sides.
///
/// # Why not UAX#29
///
/// `unicode-segmentation` was the obvious answer and it is already in the
/// tree, but it turned out to be both too little and too much:
///
/// - **Too little.** It splits Han and Hiragana one character at a time
///   (`中文です` → `中`, `文`, `で`, `す`); only Katakana groups. For a
///   double-click that is no better than what it replaced.
/// - **Too much.** It joins `foo.bar` into one word (`.` is `MidNumLet`),
///   which contradicts `word_separators` -- and the reader's list has to win,
///   because that is the whole point of the setting.
///
/// So the rule is a run of one kind, where the kinds are: the separator list
/// (always alone), whitespace, newline, each CJK script, and everything else
/// (`is_alphanumeric() || '_'`). That last one is **the same set dopamine
/// uses for hover, completion and whole-word search**.
///
/// It is not word segmentation: `食べた` stays whole rather than becoming
/// `食べ` + `た`, because knowing that needs a dictionary. It is the useful
/// approximation, and it is what an editor without one can honestly do.
pub(super) fn word_range(text: &Rope, offset: usize, separators: &str) -> Option<Range<usize>> {
    let offset = text.clip_offset(offset, Bias::Left);
    let here = text.char_at(offset)?;
    let kind = kind_of(here, separators);

    // Punctuation, separators and newlines stand alone, as they always did.
    if kind == Kind::Alone || kind == Kind::Newline {
        return Some(offset..offset + here.len_utf8());
    }

    let (window, base) = window_around(text, offset);
    let local = offset - base;

    let mut start = local;
    for c in window[..local].chars().rev() {
        if kind_of(c, separators) != kind {
            break;
        }
        start -= c.len_utf8();
    }
    let mut end = local;
    for c in window[local..].chars() {
        if kind_of(c, separators) != kind {
            break;
        }
        end += c.len_utf8();
    }
    Some(base + start..base + end)
}


#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[track_caller]
    fn word(text: &str, byte: usize) -> String {
        let rope = Rope::from(text);
        word_range(&rope, byte, DEFAULT_WORD_SEPARATORS)
            .map(|r| rope.slice(r).to_string())
            .unwrap_or_default()
    }

    #[test]
    fn kinds_are_decided_by_script_not_by_ascii() {
        let sep = DEFAULT_WORD_SEPARATORS;
        assert_eq!(kind_of('a', sep), Kind::Word);
        assert_eq!(kind_of('0', sep), Kind::Word);
        assert_eq!(kind_of('_', sep), Kind::Word, "`_` は区切りではない");
        assert_eq!(kind_of('é', sep), Kind::Word, "**ASCII だけではない**");
        assert_eq!(kind_of('漢', sep), Kind::Han);
        assert_eq!(kind_of('あ', sep), Kind::Hiragana);
        assert_eq!(kind_of('ア', sep), Kind::Katakana);
        assert_eq!(kind_of('.', sep), Kind::Alone);
        assert_eq!(kind_of('-', sep), Kind::Alone);
        assert_eq!(kind_of(' ', sep), Kind::Whitespace);
        assert_eq!(kind_of('\n', sep), Kind::Newline);
    }

    /// **The bug this replaced**: `café` used to cut after `caf`.
    #[test]
    fn an_accented_word_is_one_word() {
        assert_eq!(word("say café now", 4), "café");
        assert_eq!(word("say café now", 7), "café", "é の上でも");
        assert_eq!(word("naïve", 0), "naïve");
    }

    /// **The other half of the bug**: a Japanese word used to select one
    /// character. Runs of one script group; the scripts do not mix.
    #[test]
    fn japanese_groups_by_script() {
        // 中文です: Han run, then Hiragana run.
        assert_eq!(word("中文です", 0), "中文");
        assert_eq!(word("中文です", 3), "中文", "文 の上でも");
        assert_eq!(word("中文です", 6), "です");
        assert_eq!(word("カタカナ語", 0), "カタカナ");
        assert_eq!(word("カタカナ語", 12), "語");
    }

    /// No dictionary: 食べた stays whole rather than 食べ + た.
    #[test]
    fn there_is_no_dictionary() {
        assert_eq!(word("食べた", 0), "食");
        assert_eq!(word("食べた", 3), "べた", "かなの連なり");
    }

    #[test]
    fn identifiers_and_punctuation() {
        assert_eq!(word("test_connector x", 0), "test_connector");
        assert_eq!(word("fooBar_baz", 3), "fooBar_baz");
        assert_eq!(word("a.b", 1), ".", "区切りは 1 文字で立つ");
        assert_eq!(word("foo-bar", 3), "-");
        assert_eq!(word("hello[()]", 5), "[");
        assert_eq!(word("abc   def", 4), "   ", "空白は連なる");
    }

    /// The reader's list wins: drop `.` and `foo.bar` becomes one word.
    #[test]
    fn the_separator_list_decides() {
        let rope = Rope::from("foo.bar baz");
        let no_dot: String = DEFAULT_WORD_SEPARATORS.chars().filter(|c| *c != '.').collect();
        let r = word_range(&rope, 0, &no_dot).unwrap();
        assert_eq!(rope.slice(r).to_string(), "foo.bar");
        // With the default list it still splits.
        let r = word_range(&rope, 0, DEFAULT_WORD_SEPARATORS).unwrap();
        assert_eq!(rope.slice(r).to_string(), "foo");
    }

    #[test]
    fn a_newline_stands_alone() {
        assert_eq!(word("ab\ncd", 2), "\n");
    }
}

/// Where else the selected text appears, within the slice handed in.
///
/// # Why a scan of its own
///
/// [`super::search::SearchMatcher`] exists, but it copies the **whole**
/// document into a `String` and rebuilds an Aho-Corasick automaton whenever
/// the text changes. The selection changes on every frame of a drag, so that
/// bill would come due sixty times a second. Looking through what is on
/// screen is a memcmp over a few hundred lines.
///
/// `needle` is the selected text, `haystack` the visible slice, `offset` the
/// byte position the slice starts at, and `skip` the selection itself, which
/// is already drawn and must not be drawn twice. Returned ranges are absolute
/// byte offsets into the document.
///
/// Empty or whitespace-only selections match nothing: every space in the file
/// lighting up is noise, not information.
pub(super) fn occurrences(
    needle: &str,
    haystack: &str,
    offset: usize,
    skip: &std::ops::Range<usize>,
) -> Vec<std::ops::Range<usize>> {
    if needle.trim().is_empty() {
        return vec![];
    }
    let mut out = vec![];
    let mut at = 0;
    while let Some(found) = haystack[at..].find(needle) {
        let start = offset + at + found;
        let end = start + needle.len();
        if !(start == skip.start && end == skip.end) {
            out.push(start..end);
        }
        at += found + needle.len();
    }
    out
}

#[cfg(test)]
mod occurrence_tests {
    use super::occurrences;

    #[test]
    fn it_finds_the_other_runs_and_skips_the_selection() {
        let text = "let row = row + 1;";
        // The selection is the first `row`, at 4..7.
        let got = occurrences("row", text, 0, &(4..7));
        assert_eq!(got, vec![10..13]);
    }

    #[test]
    fn it_counts_from_where_the_slice_starts() {
        let got = occurrences("ab", "xxabxx", 100, &(0..0));
        assert_eq!(got, vec![102..104]);
    }

    /// Every space in the file lighting up is noise, not information.
    #[test]
    fn whitespace_matches_nothing() {
        assert!(occurrences(" ", "a b c", 0, &(1..2)).is_empty());
        assert!(occurrences("", "abc", 0, &(0..0)).is_empty());
        assert!(occurrences("\n  ", "a\n  b", 0, &(1..4)).is_empty());
    }

    /// Runs that overlap are not both matches -- `aa` in `aaa` is one.
    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(occurrences("aa", "aaaa", 0, &(99..99)), vec![0..2, 2..4]);
    }
}

