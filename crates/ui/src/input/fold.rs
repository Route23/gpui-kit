//! Code folding: which rows head a fold, and which rows a fold hides.
//!
//! Fold ranges come from **indentation**, not from the syntax tree: a row heads
//! a fold when the rows after it are indented more deeply. That is VS Code's
//! fallback strategy, and unlike a `folds.scm` query it works for every
//! language the editor can open without carrying a query file per grammar.
//!
//! Everything in the top half of this file is pure so it can be unit tested;
//! the state that drives it lives on [`InputState`].

use std::ops::{Range, RangeInclusive};

use ropey::Rope;

use crate::{RopeExt as _, input::TabSize};

/// Whether a line is blank (only whitespace, including a lone `\r`).
fn is_blank(line: &str) -> bool {
    line.chars().all(char::is_whitespace)
}

/// The rows hidden by folding at `row`, or `None` when `row` heads no fold.
///
/// End-exclusive, and never contains `row` itself.
///
/// Blank lines inside the region are absorbed but do **not** extend it, so the
/// blank lines between two blocks stay visible rather than being swallowed by
/// the block above (VS Code's `offSide: false`).
pub(super) fn fold_range(text: &Rope, row: usize, tab: TabSize) -> Option<Range<usize>> {
    let rows = text.lines_len();
    if row + 1 >= rows {
        return None;
    }

    let header = text.slice_line(row).to_string();
    if is_blank(&header) {
        return None;
    }
    let base = tab.indent_count(&text.slice_line(row));

    let mut end = row;
    let mut scan = row + 1;
    while scan < rows {
        let line = text.slice_line(scan);
        if is_blank(&line.to_string()) {
            scan += 1;
            continue;
        }
        if tab.indent_count(&line) > base {
            end = scan;
            scan += 1;
        } else {
            break;
        }
    }

    (end > row).then(|| row + 1..end + 1)
}

/// Whether `row` heads a fold.
///
/// Cheaper than [`fold_range`] — it only walks as far as the next non-blank
/// line — and is what the gutter asks once per visible row.
pub(super) fn is_foldable(text: &Rope, row: usize, tab: TabSize) -> bool {
    let rows = text.lines_len();
    if row + 1 >= rows {
        return false;
    }

    let header = text.slice_line(row);
    if is_blank(&header.to_string()) {
        return false;
    }
    let base = tab.indent_count(&header);

    let mut scan = row + 1;
    while scan < rows {
        let line = text.slice_line(scan);
        if is_blank(&line.to_string()) {
            scan += 1;
            continue;
        }
        return tab.indent_count(&line) > base;
    }

    false
}

/// The sorted, de-duplicated union of every fold's hidden rows.
///
/// Nested folds overlap; the union is what the wrapper needs.
pub(super) fn hidden_rows(text: &Rope, headers: &[usize], tab: TabSize) -> Vec<usize> {
    let mut rows: Vec<usize> = headers
        .iter()
        .filter_map(|&row| fold_range(text, row, tab))
        .flatten()
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows
}

/// Move fold headers to keep up with an edit.
///
/// - `edit_rows` are the rows the edit touched, **in the old text**.
/// - `delta` is `new_row_count - old_row_count`.
///
/// A header inside the edited region is **dropped**: what it used to head is
/// ambiguous once the user has typed over it. Headers below shift; headers
/// above are untouched.
pub(super) fn adjust_folded_rows(
    rows: &mut Vec<usize>,
    edit_rows: RangeInclusive<usize>,
    delta: isize,
) {
    rows.retain_mut(|row| {
        if *row < *edit_rows.start() {
            true
        } else if *row <= *edit_rows.end() {
            false
        } else {
            match row.checked_add_signed(delta) {
                Some(moved) => {
                    *row = moved;
                    true
                }
                None => false,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tab() -> TabSize {
        TabSize {
            tab_size: 4,
            hard_tabs: false,
        }
    }

    const NESTED: &str = "\
fn main() {
    if x {
        a();
    }
}
";

    #[test]
    fn folds_a_nested_block() {
        let text = Rope::from(NESTED);
        // `fn main() {` hides rows 1..=3 (everything indented under it).
        assert_eq!(fold_range(&text, 0, tab()), Some(1..4));
        // `if x {` hides only its own body.
        assert_eq!(fold_range(&text, 1, tab()), Some(2..3));
        // `a();` heads nothing.
        assert_eq!(fold_range(&text, 2, tab()), None);
        // The closing braces head nothing.
        assert_eq!(fold_range(&text, 3, tab()), None);
        assert_eq!(fold_range(&text, 4, tab()), None);
    }

    #[test]
    fn the_last_row_never_folds() {
        let text = Rope::from("a\n    b\n");
        // The final (empty) row after the trailing newline.
        assert_eq!(fold_range(&text, text.lines_len() - 1, tab()), None);
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
    }

    #[test]
    fn blank_lines_are_absorbed_but_do_not_extend_the_fold() {
        // Rows: 0 `a:`, 1 `    x`, 2 ``, 3 `    y`, 4 ``, 5 `b:`
        let text = Rope::from("a:\n    x\n\n    y\n\nb:\n");
        // The blank at 2 is absorbed (4 is not, it comes after the last deeper
        // line), so the fold stops at row 3.
        assert_eq!(fold_range(&text, 0, tab()), Some(1..4));
    }

    #[test]
    fn a_blank_header_folds_nothing() {
        let text = Rope::from("\n    a\n");
        assert_eq!(fold_range(&text, 0, tab()), None);
    }

    #[test]
    fn hard_tabs_and_spaces_agree() {
        // `\t` counts as `tab_size`, so a tab-indented body is deeper than a
        // space-indented header of 2.
        let text = Rope::from("a\n\tb\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
        let text = Rope::from("\ta\n\t\tb\n\tc\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
    }

    #[test]
    fn a_dedent_ends_the_fold() {
        // `} else {` dedents back to the header's level, so the first block
        // ends there.
        let text = Rope::from("if a {\n    x();\n} else {\n    y();\n}\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..2));
        assert_eq!(fold_range(&text, 2, tab()), Some(3..4));
    }

    #[test]
    fn a_trailing_indented_block_folds_to_the_end() {
        let text = Rope::from("a:\n    x\n    y\n");
        assert_eq!(fold_range(&text, 0, tab()), Some(1..3));
    }

    /// The cheap predicate must never disagree with the real computation.
    #[test]
    fn is_foldable_agrees_with_fold_range() {
        for src in [
            NESTED,
            "a:\n    x\n\n    y\n\nb:\n",
            "if a {\n    x();\n} else {\n    y();\n}\n",
            "\n    a\n",
            "one\ntwo\nthree\n",
            "\ta\n\t\tb\n\tc\n",
        ] {
            let text = Rope::from(src);
            for row in 0..text.lines_len() {
                assert_eq!(
                    is_foldable(&text, row, tab()),
                    fold_range(&text, row, tab()).is_some(),
                    "row {row} of {src:?}"
                );
            }
        }
    }

    #[test]
    fn hidden_rows_unions_nested_folds() {
        let text = Rope::from(NESTED);
        // Both the outer and the inner fold: the union counts row 2 once.
        assert_eq!(hidden_rows(&text, &[0, 1], tab()), vec![1, 2, 3]);
        assert_eq!(hidden_rows(&text, &[1], tab()), vec![2]);
        // A header that no longer folds contributes nothing.
        assert_eq!(hidden_rows(&text, &[2], tab()), Vec::<usize>::new());
    }

    #[test]
    fn adjust_shifts_headers_below_the_edit() {
        let mut rows = vec![1, 5, 9];
        // Two lines inserted at row 4.
        adjust_folded_rows(&mut rows, 4..=4, 2);
        assert_eq!(rows, vec![1, 7, 11]);

        let mut rows = vec![1, 5, 9];
        // Rows 3..=6 replaced by one line.
        adjust_folded_rows(&mut rows, 3..=6, -3);
        assert_eq!(rows, vec![1, 6], "5 was inside the edit and is dropped");

        let mut rows = vec![2];
        // An edit on the header's own row drops it.
        adjust_folded_rows(&mut rows, 2..=2, 0);
        assert!(rows.is_empty());

        let mut rows = vec![1];
        // Underflow drops rather than wrapping.
        adjust_folded_rows(&mut rows, 0..=0, -5);
        assert!(rows.is_empty());
    }
}
