//! What a new line, a paste, and a backspace do to indentation (#248).
//!
//! Everything that can be decided from text alone lives here as a free
//! function so it can be unit tested; the state that drives it is on
//! [`InputState`](super::InputState). Same shape as [`super::comment`].
//!
//! **The syntax tree is not consulted.** VS Code's `autoIndent: full` and
//! Zed's `syntax_aware` need per-language indent queries (`indents.scm`), and
//! this fork ships highlight and injection queries only. `Brackets` covers the
//! cases those queries are mostly used for and costs nothing new.

use super::brackets::Pair;
use crate::RopeExt as _;

/// Does this line leave a bracket open, so the next line goes one deeper?
///
/// Looks at the **last non-blank character**: a line ending in `{`, `(` or
/// `[` opens a block. A line that opens and closes on itself (`foo() {}`)
/// does not, because the last character is the closer.
pub(super) fn opens_block(line: &str, pairs: &[Pair]) -> bool {
    let Some(last) = line.trim_end().chars().next_back() else {
        return false;
    };
    pairs.iter().any(|p| p.open == last)
}

/// Is `typed` a closer that should pull this line back one level?
///
/// Only when everything before the caret on this line is whitespace --
/// typing `}` at the end of `foo() ` is not the end of a block.
pub(super) fn closes_block(before_caret_on_line: &str, typed: char, pairs: &[Pair]) -> bool {
    before_caret_on_line.chars().all(char::is_whitespace)
        && pairs.iter().any(|p| p.close == typed)
}

/// Drop one indent unit from the front of `indent`.
///
/// Takes a tab if the indent starts with one, otherwise up to `unit` spaces --
/// a file that mixes both should not lose more than one level per press.
pub(super) fn outdent_once(indent: &str, unit: usize) -> String {
    if let Some(rest) = indent.strip_prefix('\t') {
        return rest.to_string();
    }
    let take = indent.chars().take(unit.max(1)).take_while(|c| *c == ' ').count();
    indent[take..].to_string()
}

/// Where backspace lands when it steps back a whole tab stop.
///
/// `column` is counted in spaces, the way [`super::indent::TabSize`] counts
/// an indent. Returns the previous multiple of `tab`, never below zero.
pub(super) fn prev_tab_stop(column: usize, tab: usize) -> usize {
    let tab = tab.max(1);
    if column == 0 {
        return 0;
    }
    let rem = column % tab;
    column - if rem == 0 { tab } else { rem }
}

/// Re-indent a pasted block so it sits where it landed.
///
/// The **first line is left alone** -- it continues whatever the caret was
/// already on. Every following line has the block's own common indent removed
/// and `target` put in its place, so the shape inside the block survives.
///
/// Returns `None` when there is nothing to move, so the caller can skip the
/// edit entirely rather than replacing text with itself.
pub(super) fn reindent_block(pasted: &str, target: &str) -> Option<String> {
    if !pasted.contains('\n') {
        return None;
    }
    let lines: Vec<&str> = pasted.split('\n').collect();
    // The common indent of every non-blank line after the first.
    let common = lines[1..]
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()?;
    let mut out = String::with_capacity(pasted.len());
    out.push_str(lines[0]);
    for line in &lines[1..] {
        out.push('\n');
        if line.trim().is_empty() {
            // A blank line keeps no indent -- trailing spaces on an empty
            // line are exactly what `trim_auto_whitespace` exists to remove.
            continue;
        }
        out.push_str(target);
        out.push_str(&line[common..]);
    }
    (out != pasted).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::brackets::pairs_for;

    fn rust() -> Vec<Pair> {
        pairs_for("rust")
    }

    #[test]
    fn a_line_ending_in_an_opener_opens_a_block() {
        assert!(opens_block("fn main() {", &rust()));
        assert!(opens_block("    let v = vec![   ", &rust()));
        assert!(!opens_block("fn main() {}", &rust()));
        assert!(!opens_block("let x = 1;", &rust()));
        assert!(!opens_block("", &rust()));
    }

    /// Typing `}` after code is not the end of a block.
    #[test]
    fn only_a_closer_on_its_own_pulls_back() {
        assert!(closes_block("    ", '}', &rust()));
        assert!(closes_block("", ')', &rust()));
        assert!(!closes_block("    foo() ", '}', &rust()));
        assert!(!closes_block("    ", 'x', &rust()));
    }

    #[test]
    fn outdent_takes_one_level_at_most() {
        assert_eq!(outdent_once("        ", 4), "    ");
        assert_eq!(outdent_once("  ", 4), "");
        assert_eq!(outdent_once("\t\t", 4), "\t");
        assert_eq!(outdent_once("", 4), "");
    }

    #[test]
    fn tab_stops_land_on_multiples() {
        assert_eq!(prev_tab_stop(8, 4), 4);
        assert_eq!(prev_tab_stop(6, 4), 4);
        assert_eq!(prev_tab_stop(4, 4), 0);
        assert_eq!(prev_tab_stop(1, 4), 0);
        assert_eq!(prev_tab_stop(0, 4), 0);
    }

    /// The shape inside the pasted block survives; only the block moves.
    #[test]
    fn a_paste_keeps_its_own_shape() {
        // The block's own base is the `}` line at column 0, so moving the
        // block to 8 puts `foo();` at 12 -- the 4 it had inside the block is
        // kept, not flattened.
        let got = reindent_block("if x {\n    foo();\n}", "        ").unwrap();
        assert_eq!(got, "if x {\n            foo();\n        }");
    }

    #[test]
    fn a_blank_line_in_a_paste_gets_no_indent() {
        let got = reindent_block("a\n\nb", "  ").unwrap();
        assert_eq!(got, "a\n\n  b");
    }

    /// Nothing to do: one line, or already where it belongs.
    #[test]
    fn a_paste_that_does_not_move_is_skipped() {
        assert_eq!(reindent_block("foo();", "    "), None);
        assert_eq!(reindent_block("a\nb", ""), None);
    }
}

impl super::InputState {
    /// Where backspace should land when it steps back a whole tab stop
    /// (`editor.useTabStops`), or `None` to delete one character.
    ///
    /// Only inside the **leading whitespace of the line, and only spaces** --
    /// a tab is already one press, and text before the caret means the reader
    /// is editing, not indenting.
    pub(super) fn tab_stop_before_caret(&self) -> Option<usize> {
        if !self.mode.use_tab_stops() {
            return None;
        }
        let start = self.start_of_line();
        let caret = self.cursor();
        if caret <= start {
            return None;
        }
        let before = self.text.slice(start..caret).to_string();
        if before.is_empty() || !before.chars().all(|c| c == ' ') {
            return None;
        }
        let tab = self.mode.tab_size().indent_unit();
        let stop = prev_tab_stop(before.chars().count(), tab);
        (stop < before.chars().count()).then(|| start + stop)
    }

    /// Where backspace should land when joining two lines also drops the
    /// second one's indent (`editor.trimWhitespaceOnDelete`), or `None`.
    ///
    /// Fires only when the caret sits at the **first non-blank character** of
    /// a line that has an indent: that is the position where the reader is
    /// pulling the line up, and the indent has no meaning on the line above.
    pub(super) fn join_without_indent(&self) -> Option<usize> {
        if !self.mode.trim_whitespace_on_delete() {
            return None;
        }
        let start = self.start_of_line();
        let caret = self.cursor();
        if caret <= start || start == 0 {
            return None;
        }
        let before = self.text.slice(start..caret).to_string();
        if before.is_empty() || !before.chars().all(char::is_whitespace) {
            return None;
        }
        // Back over the indent and the newline in one press.
        Some(self.previous_boundary(start))
    }
}

impl super::InputState {
    /// What a paste should look like once it is re-indented to where it
    /// landed (`editor.autoIndentOnPaste`), or `None` to paste as-is.
    pub(super) fn reindent_paste(&self, pasted: &str) -> Option<String> {
        let (on, in_string) = self.mode.auto_indent_on_paste();
        if !on || !self.mode.is_code_editor() {
            return None;
        }
        let caret = self.cursor();
        if !in_string {
            // A paste inside a string or a comment is data, not code
            // (`editor.autoIndentOnPasteWithinString`).
            let bounds = 0..self.text.len();
            if self
                .skipped_spans(&bounds)
                .iter()
                .any(|r| r.contains(&caret))
            {
                return None;
            }
        }
        let start = self.start_of_line();
        let indent: String = self
            .text
            .slice(start..caret)
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        reindent_block(pasted, &indent)
    }

    /// Pull this line back one level, because a closing bracket was typed as
    /// the first thing on it (`AutoIndent::Brackets`).
    ///
    /// Returns the range to delete, or `None` to leave the line alone.
    pub(super) fn outdent_for_closer(&self, typed: &str) -> Option<std::ops::Range<usize>> {
        if self.mode.auto_indent() != super::mode::AutoIndent::Brackets {
            return None;
        }
        let typed = typed.chars().next().filter(|_| typed.chars().count() == 1)?;
        let start = self.start_of_line();
        let caret = self.cursor();
        let before = self.text.slice(start..caret).to_string();
        let pairs = super::brackets::pairs_for(self.mode.language_name());
        if !closes_block(&before, typed, &pairs) || before.is_empty() {
            return None;
        }
        let unit = self.mode.tab_size().indent_unit();
        let shorter = outdent_once(&before, unit);
        (shorter.len() < before.len()).then(|| start + shorter.len()..caret)
    }
}

impl super::InputState {
    /// Where ← lands when leading spaces are crossed a tab stop at a time
    /// (`editor.stickyTabStops`), or `None` for one character.
    pub(super) fn sticky_stop_left(&self) -> Option<usize> {
        let (start, column) = self.sticky_column()?;
        let tab = self.mode.tab_size().indent_unit();
        let stop = prev_tab_stop(column, tab);
        (stop < column).then(|| start + stop)
    }

    /// Where → lands, or `None` for one character.
    pub(super) fn sticky_stop_right(&self) -> Option<usize> {
        let (start, column) = self.sticky_column()?;
        let tab = self.mode.tab_size().indent_unit().max(1);
        // The line's whole run of leading spaces: never step past it.
        let run = self
            .text
            .slice(start..self.text.len())
            .chars()
            .take_while(|c| *c == ' ')
            .count();
        let next = (column / tab + 1) * tab;
        (next <= run).then(|| start + next)
    }

    /// The caret's column, when it is inside a run of leading **spaces**.
    ///
    /// A tab is already one press, and a caret past the indent is editing
    /// text, not moving through it.
    fn sticky_column(&self) -> Option<(usize, usize)> {
        if !self.mode.sticky_tab_stops() {
            return None;
        }
        let start = self.start_of_line();
        let caret = self.cursor();
        let before = self.text.slice(start..caret).to_string();
        (!before.is_empty() && before.chars().all(|c| c == ' '))
            .then(|| (start, before.chars().count()))
    }
}

impl super::InputState {
    /// Take back indent the editor inserted, when the caret leaves the line
    /// without anything being typed on it (`editor.trimAutoWhitespace`).
    ///
    /// Returns the target offset, moved back if text before it went away.
    /// **Not an undo step** -- it removes what the editor put there itself,
    /// so putting it in the history would make Undo restore whitespace.
    pub(super) fn trim_auto_whitespace_on_leave(&mut self, target: usize) -> usize {
        let Some(range) = self.auto_ws.clone() else {
            return target;
        };
        if !self.mode.trim_auto_whitespace() || range.end > self.text.len() {
            self.auto_ws = None;
            return target;
        }
        let row_of = |text: &ropey::Rope, o: usize| text.offset_to_point(o).row;
        // Still on the line: keep watching.
        if row_of(&self.text, target) == row_of(&self.text, range.start) {
            return target;
        }
        self.auto_ws = None;
        // Something was typed on the line, or the indent is gone already.
        let line_end = {
            let row = row_of(&self.text, range.start);
            let next = row + 1;
            if next < self.text.lines_len() {
                self.text.line_start_offset(next).saturating_sub(1)
            } else {
                self.text.len()
            }
        };
        if line_end != range.end {
            return target;
        }
        let slice = self.text.slice(range.clone()).to_string();
        if slice.is_empty() || !slice.chars().all(|c| c == ' ' || c == '\t') {
            return target;
        }
        self.text.replace(range.clone(), "");
        if target >= range.end {
            target - range.len()
        } else {
            target
        }
    }
}
