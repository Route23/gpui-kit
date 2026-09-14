use std::{char, ops::Range};

use gpui::{Context, Window};
use ropey::Rope;
use sum_tree::Bias;

use crate::{input::InputState, RopeExt as _};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CharType {
    /// a-z, A-Z, 0-9, _
    Word,
    /// '\t', ' ', '\u{00A0}' etc.
    Whitespace,
    /// \n, \r
    Newline,
    /// . , ; : ( ) [ ] { } ... or CJK characters: `汉`, `🎉` etc.
    Other,
}

impl From<char> for CharType {
    fn from(c: char) -> Self {
        match c {
            '_' => CharType::Word,
            c if c.is_ascii_alphanumeric() => CharType::Word,
            c if c == '\n' || c == '\r' => CharType::Newline,
            c if c.is_whitespace() => CharType::Whitespace,
            _ => CharType::Other,
        }
    }
}

impl CharType {
    /// Check if two CharTypes are connectable
    fn is_connectable(self, c: char) -> bool {
        let other = CharType::from(c);
        match (self, other) {
            (CharType::Word, CharType::Word) => true,
            (CharType::Whitespace, CharType::Whitespace) => true,
            _ => false,
        }
    }
}

impl InputState {
    /// Select the word at the given offset on double-click.
    ///
    /// The offset is the UTF-8 offset.
    pub(super) fn select_word(&mut self, offset: usize, _: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = TextSelector::word_range(&self.text, offset) else {
            return;
        };

        self.selected_range = (range.start..range.end).into();
        self.selected_word_range = Some(self.selected_range);
        cx.notify()
    }
}

struct TextSelector;
impl TextSelector {
    /// Select a word in the given text at the specified offset.
    ///
    /// The offset is the UTF-8 offset.
    ///
    /// Returns the start and end offsets of the selected word.
    pub fn word_range(text: &Rope, offset: usize) -> Option<Range<usize>> {
        let offset = text.clip_offset(offset, Bias::Left);
        let Some(char) = text.char_at(offset) else {
            return None;
        };

        let char_type = CharType::from(char);
        let mut start = offset;
        let mut end = offset + char.len_utf8();
        let prev_chars = text.chars_at(start).reversed().take(128);
        let next_chars = text.chars_at(end).take(128);

        for ch in prev_chars {
            if char_type.is_connectable(ch) {
                start -= ch.len_utf8();
            } else {
                break;
            }
        }

        for ch in next_chars {
            if char_type.is_connectable(ch) {
                end += ch.len_utf8();
            } else {
                break;
            }
        }

        Some(start..end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn test_char_type_from_char() {
        assert_eq!(CharType::from('a'), CharType::Word);
        assert_eq!(CharType::from('Z'), CharType::Word);
        assert_eq!(CharType::from('0'), CharType::Word);
        assert_eq!(CharType::from('_'), CharType::Word);
        assert_eq!(CharType::from('.'), CharType::Other);
        assert_eq!(CharType::from(','), CharType::Other);
        assert_eq!(CharType::from(';'), CharType::Other);
        assert_eq!(CharType::from('!'), CharType::Other);
        assert_eq!(CharType::from('?'), CharType::Other);
        assert_eq!(CharType::from('['), CharType::Other);
        assert_eq!(CharType::from('{'), CharType::Other);
        assert_eq!(CharType::from(' '), CharType::Whitespace);
        assert_eq!(CharType::from('\t'), CharType::Whitespace);
        assert_eq!(CharType::from('\u{00A0}'), CharType::Whitespace);
        assert_eq!(CharType::from('\n'), CharType::Newline);
        assert_eq!(CharType::from('\r'), CharType::Newline);
        assert_eq!(CharType::from('汉'), CharType::Other);
        assert_eq!(CharType::from('é'), CharType::Other);
    }

    #[test]
    fn test_word_range() {
        use indoc::indoc;

        let rope = Rope::from(indoc! {
            r#"
            test text:
            abcde 中文🎉 test
            hello[()]
            test_connector ____
            Rope
            "#
        });

        let tests = vec![
            (0, 0, Some("test")),
            (0, 4, Some(" ")),
            (1, 0, Some("abcde")),
            (1, 4, Some("abcde")),
            (1, 5, Some(" ")),
            (1, 6, Some("中")),
            (1, 9, Some("文")),
            (1, 13, Some("🎉")),
            (1, 20, Some("test")),
            (2, 5, Some("[")),
            (2, 6, Some("(")),
            (2, 7, Some(")")),
            (2, 8, Some("]")),
            (3, 5, Some("test_connector")),
            (3, 14, Some(" ")),
            (3, 16, Some("____")),
            (4, 0, Some("Rope")),
        ];

        for (line, column, expected) in tests {
            let line_start_offset = rope.line_start_offset(line);
            let offset = line_start_offset + column;
            let range = TextSelector::word_range(&rope, offset);

            let actual = range.map(|r| rope.slice(r).to_string());
            let expect = expected.map(|s| s.to_string());
            assert_eq!(actual, expect, "line {}, column {}", line, column);
        }
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
