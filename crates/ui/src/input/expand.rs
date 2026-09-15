//! Growing and shrinking the selection (#253).
//!
//! VS Code calls this "expand selection"; it is the one selection command
//! that does not exist here at all. The pieces to build it with already do:
//! [`super::selection::word_range`] answers "what word is this", and
//! [`super::brackets::enclosing`] answers "what encloses this".
//!
//! **The syntax tree is not consulted.** `InputMode::CodeEditor` carries a
//! highlighter, so a later version could climb tree-sitter nodes and stop at
//! statements and blocks. This version stops at brackets, which covers most
//! of what the command is used for and costs nothing new.

use std::ops::Range;

use gpui::{Context, Window};

use crate::{
    input::{mode::InputMode, ExpandSelection, InputState, ShrinkSelection},
    RopeExt as _,
};

/// Where a piece of a word begins, for `smartSelect.selectSubwords`.
///
/// `fooBar_baz` has pieces at `foo`, `Bar`, `baz`. Splits at an underscore
/// and where a lower-case letter is followed by an upper-case one.
fn subword_at(word: &str, local: usize) -> Option<Range<usize>> {
    let mut bounds = vec![0usize];
    let mut prev: Option<char> = None;
    for (i, c) in word.char_indices() {
        if c == '_' {
            // The underscore itself belongs to neither side.
            bounds.push(i);
            bounds.push(i + c.len_utf8());
        } else if prev.is_some_and(|p| p.is_lowercase() || p.is_numeric()) && c.is_uppercase() {
            bounds.push(i);
        }
        prev = Some(c);
    }
    bounds.push(word.len());
    bounds.dedup();
    let piece = bounds
        .windows(2)
        .find(|w| w[0] <= local && local < w[1])
        .map(|w| w[0]..w[1])?;
    // A single piece is not a smaller step than the word.
    (piece.len() < word.len()).then_some(piece)
}

impl InputState {
    /// The string and comment spans, so bracket walks do not count a `}`
    /// that is inside `"…"`.
    pub(super) fn skipped_spans(&self, range: &Range<usize>) -> Vec<Range<usize>> {
        let InputMode::CodeEditor { highlighter, .. } = &self.mode else {
            return vec![];
        };
        let highlighter = highlighter.borrow();
        let Some(highlighter) = highlighter.as_ref() else {
            return vec![];
        };
        let spans: Vec<Range<usize>> = highlighter.skipped_ranges(range);
        spans
    }

    /// Select what the bracket under `offset` encloses.
    ///
    /// Returns whether it did. Used by a double-click when
    /// `double_click_selects_block` is on.
    pub(super) fn select_enclosing_block(&mut self, offset: usize, cx: &mut Context<Self>) -> bool {
        if !self.mode.double_click_selects_block() {
            return false;
        }
        let Some(here) = self.text.char_at(offset) else {
            return false;
        };
        let pairs = crate::input::brackets::pairs_for(self.mode.language_name());
        if !pairs.iter().any(|p| p.open == here || p.close == here) {
            return false;
        }
        let Some((open, close)) = self.enclosing_pair(offset) else {
            return false;
        };
        self.selected_range = (open.start..close.end).into();
        self.selected_word_range = None;
        cx.notify();
        true
    }

    /// The pair enclosing `offset`, searched over the whole document.
    fn enclosing_pair(&self, offset: usize) -> Option<(Range<usize>, Range<usize>)> {
        let pairs = crate::input::brackets::pairs_for(self.mode.language_name());
        if pairs.is_empty() {
            return None;
        }
        let bounds = 0..self.text.len();
        let skip = self.skipped_spans(&bounds);
        crate::input::brackets::enclosing(&self.text, offset, &pairs, &bounds, &skip)
    }

    /// Grow the selection by one step (⌃⇧⌘→).
    pub(super) fn on_action_expand_selection(
        &mut self,
        _: &ExpandSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current: Range<usize> = self.selected_range.into();
        let Some(next) = self.next_larger(&current) else {
            return;
        };
        // Never record a step that goes nowhere — ⌃⇧⌘← would then need two
        // presses to undo one visible ⌃⇧⌘→.
        if next == current {
            return;
        }
        self.expand_stack.push(current);
        self.selected_range = next.into();
        self.selected_word_range = None;
        cx.notify();
    }

    /// Go back one step (⌃⇧⌘←).
    pub(super) fn on_action_shrink_selection(
        &mut self,
        _: &ShrinkSelection,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(prev) = self.expand_stack.pop() else {
            return;
        };
        self.selected_range = prev.into();
        self.selected_word_range = None;
        cx.notify();
    }

    /// The next step out from `current`.
    fn next_larger(&self, current: &Range<usize>) -> Option<Range<usize>> {
        let at = current.start;
        let separators = self.mode.word_separators();
        let whitespace = self.mode.smart_select_whitespace();

        // 1. the piece of the word, then 2. the word.
        if let Some(word) = crate::input::selection::word_range(&self.text, at, &separators) {
            if self.mode.smart_select_subwords() && word.len() > current.len() {
                let text = self.text.slice(word.clone()).to_string();
                if let Some(piece) = subword_at(&text, at - word.start) {
                    let piece = word.start + piece.start..word.start + piece.end;
                    if piece.len() > current.len() {
                        return Some(piece);
                    }
                }
            }
            if word.len() > current.len() && word != *current {
                return Some(word);
            }
        }

        // 3. what the nearest pair encloses, then 4. the pair itself.
        //
        // **Trim before comparing, not after.** Trimming afterwards hands back
        // a range the reader is already sitting on: the untrimmed inside of a
        // braced block is longer than the trimmed one, so the size test passes
        // and `maybe_trim` then shrinks it straight back to where it started.
        // That made the second ⌃⇧⌘→ do nothing — and, because it still pushed
        // onto the stack, it made ⌃⇧⌘← look broken too.
        if let Some((open, close)) = self.enclosing_pair(at) {
            let inner = self.maybe_trim(open.end..close.start, whitespace);
            let outer = open.start..close.end;
            if inner.len() > current.len() && inner != *current {
                return Some(inner);
            }
            if outer.len() > current.len() && outer != *current {
                return Some(outer);
            }
        }

        // 5. the line, 6. everything.
        let row = self.text.offset_to_point(at).row;
        let line = self.text.line_start_offset(row)
            ..if row + 1 < self.text.lines_len() {
                self.text.line_start_offset(row + 1)
            } else {
                self.text.len()
            };
        let line = self.maybe_trim(line, whitespace);
        if line.len() > current.len() && line != *current {
            return Some(line);
        }
        let all = 0..self.text.len();
        (all != *current).then_some(all)
    }

    /// Drop the blank edges unless the reader asked to keep them
    /// (`smartSelect.selectLeadingAndTrailingWhitespace`).
    fn maybe_trim(&self, range: Range<usize>, keep: bool) -> Range<usize> {
        if keep {
            return range;
        }
        let text = self.text.slice(range.clone()).to_string();
        let start = range.start + (text.len() - text.trim_start().len());
        let end = range.end - (text.len() - text.trim_end().len());
        if start < end { start..end } else { range }
    }
}

#[cfg(test)]
mod tests {
    use super::subword_at;

    #[test]
    fn camel_and_snake_split_into_pieces() {
        assert_eq!(subword_at("fooBar_baz", 0), Some(0..3));
        assert_eq!(subword_at("fooBar_baz", 4), Some(3..6));
        assert_eq!(subword_at("fooBar_baz", 8), Some(7..10));
    }

    /// A word with no pieces is not a smaller step than itself.
    #[test]
    fn a_plain_word_has_no_pieces() {
        assert_eq!(subword_at("foo", 1), None);
        assert_eq!(subword_at("", 0), None);
    }

    /// Digits end a piece too: `utf8Bytes` is `utf8` + `Bytes`.
    #[test]
    fn digits_count_as_lower() {
        assert_eq!(subword_at("utf8Bytes", 2), Some(0..4));
    }
}
