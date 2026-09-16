//! Continuing a Markdown list onto the next line (#248, Zed's
//! `extend_list_on_newline` / `indent_list_on_tab`).
//!
//! The shape is [`super::comment`]'s: a pure function decides what Enter
//! means, and a line that is **nothing but the marker** is the way out of the
//! list. Everything here is text-only so it can be unit tested.

use std::ops::Range;

/// A list marker at the head of a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListMarker {
    /// The line's own indent, kept verbatim so tabs survive.
    pub indent: String,
    /// What to put on the next line: `- `, `1. `, `- [ ] `.
    pub next: String,
    /// The marker's byte range in the line, indent included.
    pub range: Range<usize>,
}

/// The marker this line starts with, if any.
///
/// Reads bullets (`-`, `*`, `+`) and ordered items (`1.`, `2)`), optionally
/// followed by a task box. **An ordered item counts up**, which is what makes
/// a numbered list usable; a bullet repeats.
pub fn marker_of(line: &str) -> Option<ListMarker> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];

    let (token, after) = if let Some(after) = rest
        .strip_prefix("- ")
        .or_else(|| rest.strip_prefix("* "))
        .or_else(|| rest.strip_prefix("+ "))
    {
        (rest[..2].to_string(), after)
    } else {
        // `12. ` or `12) `
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let sep = rest.as_bytes().get(digits)?;
        if *sep != b'.' && *sep != b')' {
            return None;
        }
        if rest.as_bytes().get(digits + 1) != Some(&b' ') {
            return None;
        }
        let n: usize = rest[..digits].parse().ok()?;
        (
            format!("{}{} ", n + 1, *sep as char),
            &rest[digits + 2..],
        )
    };

    // A task box right after the bullet: the next item starts unchecked.
    let (token, after) = if let Some(rest) = after
        .strip_prefix("[ ] ")
        .or_else(|| after.strip_prefix("[x] "))
        .or_else(|| after.strip_prefix("[X] "))
    {
        (format!("{token}[ ] "), rest)
    } else {
        (token, after)
    };

    let used = line.len() - after.len();
    Some(ListMarker {
        indent: indent.to_string(),
        next: token,
        range: 0..used,
    })
}

/// What Enter should do on a line that is a list item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnterList {
    /// Start the next line with this (indent included).
    Continue { prefix: String },
    /// The line held nothing but the marker — take it off, so Enter is how
    /// you leave the list.
    Clear { range: Range<usize> },
}

/// What Enter at `caret` bytes into `line` should do, or `None` for a plain
/// line break.
///
/// Only continues when the caret is **after** the marker, the same rule
/// comments follow: pressing Enter in front of `- ` splits the line.
pub fn enter_list(line: &str, caret: usize) -> Option<EnterList> {
    let body = line.trim_end_matches('\r');
    let marker = marker_of(body)?;
    if caret < marker.range.end {
        return None;
    }
    if body[marker.range.end..].trim().is_empty() {
        return Some(EnterList::Clear {
            range: marker.range,
        });
    }
    Some(EnterList::Continue {
        prefix: format!("{}{}", marker.indent, marker.next),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn next(line: &str) -> Option<String> {
        match enter_list(line, line.len())? {
            EnterList::Continue { prefix } => Some(prefix),
            EnterList::Clear { .. } => None,
        }
    }

    #[test]
    fn a_bullet_repeats() {
        assert_eq!(next("- foo").as_deref(), Some("- "));
        assert_eq!(next("  * foo").as_deref(), Some("  * "));
        assert_eq!(next("+ foo").as_deref(), Some("+ "));
    }

    /// A numbered list counts up -- repeating `1.` is never what was meant.
    #[test]
    fn a_number_counts_up() {
        assert_eq!(next("1. foo").as_deref(), Some("2. "));
        assert_eq!(next("   9) foo").as_deref(), Some("   10) "));
    }

    #[test]
    fn a_task_box_starts_unchecked() {
        assert_eq!(next("- [x] done").as_deref(), Some("- [ ] "));
        assert_eq!(next("- [ ] todo").as_deref(), Some("- [ ] "));
    }

    /// The marker on its own is the way out.
    #[test]
    fn an_empty_item_clears() {
        assert_eq!(enter_list("- ", 2), Some(EnterList::Clear { range: 0..2 }));
        assert_eq!(
            enter_list("  - [ ] ", 8),
            Some(EnterList::Clear { range: 0..8 })
        );
    }

    /// Enter in front of the marker is a plain line break.
    #[test]
    fn the_caret_has_to_be_past_the_marker() {
        assert_eq!(enter_list("- foo", 0), None);
        assert_eq!(enter_list("- foo", 1), None);
    }

    #[test]
    fn plain_text_is_not_a_list() {
        assert_eq!(marker_of("foo"), None);
        assert_eq!(marker_of("-foo"), None, "a dash needs a space after it");
        assert_eq!(marker_of("1.foo"), None);
        assert_eq!(marker_of(""), None);
    }
}

impl super::InputState {
    /// The list marker Enter should carry, if any. Mirror of
    /// [`super::InputState::comment_to_carry`].
    pub(super) fn list_to_carry(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> Option<EnterList> {
        use gpui::EntityInputHandler as _;
        if !self.mode.list_on_newline() {
            return None;
        }
        let start = self.start_of_line();
        let end = self.end_of_line();
        let line = self.text_for_range(self.range_to_utf16(&(start..end)), &mut None, window, cx)?;
        let caret = self.cursor().checked_sub(start)?;
        match enter_list(&line, caret)? {
            EnterList::Continue { prefix } => Some(EnterList::Continue { prefix }),
            EnterList::Clear { range } => Some(EnterList::Clear {
                range: start + range.start..start + range.end,
            }),
        }
    }
}

impl super::InputState {
    /// Whether Tab should indent this list item rather than insert an indent
    /// at the caret (`indent_list_on_tab`).
    ///
    /// Only **at or before the end of the marker** -- Tab in the middle of an
    /// item's text is still a Tab.
    pub(super) fn tab_indents_list(&self) -> bool {
        if !self.mode.indent_list_on_tab() || !self.selected_range.is_empty() {
            return false;
        }
        let start = self.start_of_line();
        let end = self.end_of_line();
        let line = self.text.slice(start..end).to_string();
        let Some(marker) = marker_of(&line) else {
            return false;
        };
        self.cursor().saturating_sub(start) <= marker.range.end
    }
}
