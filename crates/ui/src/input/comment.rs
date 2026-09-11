//! Toggling line and block comments.
//!
//! The comment tokens come from a **table in this file**. `LanguageConfig`
//! does not carry them, and the tree-sitter language list is behind a feature
//! flag, so tying comments to it would make them disappear in builds without
//! the grammars. Every editor keeps its own table for exactly this reason
//! (VS Code's `language-configuration.json`, Zed's `config.toml`).
//!
//! Everything here is pure so it can be unit tested; the state that drives it
//! lives on [`InputState`](super::InputState).

use std::ops::Range;

/// What a language uses to mark a comment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CommentTokens {
    /// Marks the rest of the line, e.g. `//`.
    pub line: Option<&'static str>,
    /// Wraps a span, e.g. `/*` and `*/`.
    pub block: Option<(&'static str, &'static str)>,
}

const SLASHES: CommentTokens = CommentTokens {
    line: Some("//"),
    block: Some(("/*", "*/")),
};
const HASH: CommentTokens = CommentTokens {
    line: Some("#"),
    block: None,
};
const C_BLOCK: CommentTokens = CommentTokens {
    line: None,
    block: Some(("/*", "*/")),
};
const MARKUP: CommentTokens = CommentTokens {
    line: None,
    block: Some(("<!--", "-->")),
};

/// The tokens `language` comments with.
///
/// The names are the ones the editor is opened with — whatever
/// `language_of` resolved the file extension to.
///
/// `json` gets `//` even though strict JSON has no comments: everything that
/// edits JSON in practice is editing JSONC, and VS Code does the same.
pub fn tokens_for(language: &str) -> CommentTokens {
    match language {
        "rust" | "typescript" | "javascript" | "go" | "c" | "cpp" | "swift" | "java" | "json" => {
            SLASHES
        }
        "python" | "ruby" | "toml" | "yaml" | "bash" => HASH,
        "css" => C_BLOCK,
        "html" | "xml" | "markdown" => MARKUP,
        _ => CommentTokens::default(),
    }
}

/// Whether toggling the selected lines should comment or uncomment them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineToggle {
    /// Insert the token at `column` bytes into each non-blank line.
    Comment { column: usize },
    /// Take the token off every line that has one.
    Uncomment,
}

/// What toggling `lines` should do, or `None` when there is nothing to do.
///
/// Follows VS Code:
///
/// - **blank lines do not count** and are left alone;
/// - if *every* non-blank line is already commented, the toggle removes;
///   one bare line is enough to make it add instead;
/// - the token goes in at the **shallowest indent** of the non-blank lines,
///   not at column 0, so a commented block keeps its shape.
pub fn line_toggle(lines: &[&str], token: &str) -> Option<LineToggle> {
    let mut column = usize::MAX;
    let mut all_commented = true;
    let mut any = false;
    for line in lines {
        let body = line.trim_end_matches('\r');
        if body.trim().is_empty() {
            continue;
        }
        any = true;
        column = column.min(indent_len(body));
        if line_prefix(body, token).is_none() {
            all_commented = false;
        }
    }
    if !any {
        return None;
    }
    Some(if all_commented {
        LineToggle::Uncomment
    } else {
        LineToggle::Comment { column }
    })
}

/// The byte range of `line`'s comment token, if it has one.
///
/// The range covers **one space after the token** as well, so uncommenting
/// `// a` gives back `a` rather than ` a`.
pub fn line_prefix(line: &str, token: &str) -> Option<Range<usize>> {
    let start = indent_len(line);
    let rest = line.get(start..)?;
    if !rest.starts_with(token) {
        return None;
    }
    let mut end = start + token.len();
    if line[end..].starts_with(' ') {
        end += 1;
    }
    Some(start..end)
}

/// How many bytes of leading whitespace `line` has.
fn indent_len(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// What toggling a block comment around `selected` should do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockToggle {
    /// Put the tokens around it.
    Wrap,
    /// Take them off: the byte ranges of the opener and the closer.
    Unwrap {
        open: Range<usize>,
        close: Range<usize>,
    },
}

/// What toggling `(open, close)` around `selected` should do.
///
/// Already-wrapped text unwraps even when there is whitespace between the
/// tokens and the body, which is what you get after wrapping and then
/// re-indenting.
pub fn block_toggle(selected: &str, (open, close): (&str, &str)) -> BlockToggle {
    let head = selected.trim_start();
    let tail = selected.trim_end();
    if head.starts_with(open) && tail.ends_with(close) && tail.len() >= open.len() + close.len() {
        let start = indent_len(selected);
        let mut open_end = start + open.len();
        if selected[open_end..].starts_with(' ') {
            open_end += 1;
        }
        let end = tail.len();
        let mut close_start = end - close.len();
        if selected[..close_start].ends_with(' ') {
            close_start -= 1;
        }
        return BlockToggle::Unwrap {
            open: start..open_end,
            close: close_start..end,
        };
    }
    BlockToggle::Wrap
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_covers_what_the_editor_can_open() {
        assert_eq!(tokens_for("rust").line, Some("//"));
        assert_eq!(tokens_for("python").line, Some("#"));
        assert_eq!(tokens_for("json").line, Some("//"), "JSONC が普通に使われる");
        assert_eq!(tokens_for("css").line, None, "CSS に行コメントは無い");
        assert_eq!(tokens_for("css").block, Some(("/*", "*/")));
        assert_eq!(tokens_for("html").block, Some(("<!--", "-->")));
        assert_eq!(tokens_for("text"), CommentTokens::default(), "素の文章は何もしない");
        assert_eq!(tokens_for("nonesuch"), CommentTokens::default());
    }

    #[test]
    fn one_bare_line_makes_the_whole_block_comment() {
        let lines = ["// a", "b"];
        assert_eq!(
            line_toggle(&lines, "//"),
            Some(LineToggle::Comment { column: 0 })
        );
    }

    #[test]
    fn all_commented_means_uncomment() {
        let lines = ["// a", "  // b"];
        assert_eq!(line_toggle(&lines, "//"), Some(LineToggle::Uncomment));
    }

    #[test]
    fn the_token_goes_in_at_the_shallowest_indent() {
        let lines = ["        deep", "    shallow"];
        assert_eq!(
            line_toggle(&lines, "//"),
            Some(LineToggle::Comment { column: 4 })
        );
    }

    #[test]
    fn blank_lines_do_not_count() {
        let lines = ["// a", "", "   ", "// b"];
        assert_eq!(
            line_toggle(&lines, "//"),
            Some(LineToggle::Uncomment),
            "空行があっても「全部コメント済み」"
        );
        assert_eq!(line_toggle(&["", "  "], "//"), None, "空行だけなら何もしない");
    }

    #[test]
    fn the_prefix_takes_one_space_with_it() {
        assert_eq!(line_prefix("// a", "//"), Some(0..3));
        assert_eq!(line_prefix("//a", "//"), Some(0..2), "空白が無ければ記号だけ");
        assert_eq!(line_prefix("    // a", "//"), Some(4..7), "字下げは残す");
        assert_eq!(line_prefix("a // b", "//"), None, "行頭のものだけ");
        assert_eq!(line_prefix("//  a", "//"), Some(0..3), "2 つ目の空白は残す");
    }

    #[test]
    fn carriage_returns_do_not_confuse_the_count() {
        let lines = ["// a\r", "// b\r"];
        assert_eq!(line_toggle(&lines, "//"), Some(LineToggle::Uncomment));
    }

    #[test]
    fn block_wraps_then_unwraps() {
        assert_eq!(block_toggle("a", ("/*", "*/")), BlockToggle::Wrap);
        assert_eq!(
            block_toggle("/* a */", ("/*", "*/")),
            BlockToggle::Unwrap {
                open: 0..3,
                close: 4..7
            }
        );
        assert_eq!(
            block_toggle("/*a*/", ("/*", "*/")),
            BlockToggle::Unwrap {
                open: 0..2,
                close: 3..5
            }
        );
    }
}

use gpui::{Context, EntityInputHandler as _, Window};

use crate::input::{InputState, RopeExt as _, ToggleBlockComment, ToggleComment};

impl InputState {
    /// ⌘/ — comment the selected lines, or take the comments off.
    ///
    /// Falls through to the block tokens for languages with no line comment
    /// (CSS, HTML), which is what VS Code does.
    pub(super) fn on_toggle_comment(
        &mut self,
        _: &ToggleComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tokens = tokens_for(self.mode.language_name());
        let Some(token) = tokens.line else {
            if tokens.block.is_some() {
                self.toggle_block_comment(window, cx);
            } else {
                cx.propagate();
            }
            return;
        };
        self.toggle_line_comment(token, window, cx);
    }

    /// ⌥⌘/ — wrap the selection in the block tokens, or unwrap it.
    pub(super) fn on_toggle_block_comment(
        &mut self,
        _: &ToggleBlockComment,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if tokens_for(self.mode.language_name()).block.is_none() {
            cx.propagate();
            return;
        }
        self.toggle_block_comment(window, cx);
    }

    /// Whether a space follows the comment token.
    fn comment_space(&self) -> &'static str {
        if self.mode.comment_insert_space() {
            " "
        } else {
            ""
        }
    }

    fn toggle_line_comment(&mut self, token: &str, window: &mut Window, cx: &mut Context<Self>) {
        let selected_range = self.selected_range;
        let start_offset = self.start_of_line_of_selection(window, cx);
        let block = self
            .text_for_range(
                self.range_to_utf16(&(start_offset..selected_range.end)),
                &mut None,
                window,
                cx,
            )
            .unwrap_or_default();
        let lines: Vec<&str> = block.split('\n').collect();
        let Some(toggle) = line_toggle(&lines, token) else {
            return;
        };

        // **One edit for the reader, one edit for ⌘Z.** Each line is its own
        // `replace_text_in_range`, and that ends the history group every time,
        // so without this the undo takes as many presses as there are lines.
        self.history.start_grouping();

        let insert = format!("{token}{}", self.comment_space());
        let mut delta: isize = 0;
        let mut offset = start_offset;
        for line in &lines {
            let body = line.trim_end_matches('\r');
            let bare = body.trim().is_empty();
            match toggle {
                LineToggle::Comment { column } if !bare => {
                    let at = offset + column.min(body.len());
                    self.replace_text_in_range_silent(
                        Some(self.range_to_utf16(&(at..at))),
                        &insert,
                        window,
                        cx,
                    );
                    delta += insert.len() as isize;
                    offset += line.len() + insert.len() + 1;
                }
                LineToggle::Uncomment => {
                    if let Some(prefix) = line_prefix(body, token) {
                        let at = offset + prefix.start;
                        self.replace_text_in_range_silent(
                            Some(self.range_to_utf16(&(at..at + prefix.len()))),
                            "",
                            window,
                            cx,
                        );
                        delta -= prefix.len() as isize;
                        offset += line.len() - prefix.len() + 1;
                    } else {
                        offset += line.len() + 1;
                    }
                }
                // A blank line while commenting: left alone.
                _ => offset += line.len() + 1,
            }
        }

        self.history.end_grouping();

        let end = selected_range.end.saturating_add_signed(delta);
        self.selected_range = if selected_range.is_empty() {
            let caret = selected_range.start.saturating_add_signed(delta);
            (caret..caret).into()
        } else {
            (start_offset..end).into()
        };
        cx.notify();
    }

    fn toggle_block_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((open, close)) = tokens_for(self.mode.language_name()).block else {
            return;
        };
        let selected_range = self.selected_range;
        // With no selection, wrap the line the caret is on.
        let range = if selected_range.is_empty() {
            let start = self.start_of_line_of_selection(window, cx);
            let row = self.text.offset_to_point(start).row;
            start..self.text.line_end_offset(row)
        } else {
            selected_range.into()
        };
        let text = self
            .text_for_range(self.range_to_utf16(&range), &mut None, window, cx)
            .unwrap_or_default();
        if text.is_empty() {
            return;
        }

        self.history.start_grouping();
        let space = self.comment_space();
        let replacement = match block_toggle(&text, (open, close)) {
            BlockToggle::Wrap => format!("{open}{space}{text}{space}{close}"),
            BlockToggle::Unwrap { open, close } => {
                let mut out = String::with_capacity(text.len());
                out.push_str(&text[..open.start]);
                out.push_str(&text[open.end..close.start]);
                out.push_str(&text[close.end..]);
                out
            }
        };
        let len = replacement.len();
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&range)),
            &replacement,
            window,
            cx,
        );
        self.history.end_grouping();

        self.selected_range = (range.start..range.start + len).into();
        cx.notify();
    }
}
