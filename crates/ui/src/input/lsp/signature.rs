//! Asking the server what the call under the caret takes (#246).
//!
//! `textDocument/signatureHelp`. The **trigger** is a bracket or a comma, and
//! the **anchor** is the bracket that opened the call -- not the caret, so the
//! popover stays put while the arguments are typed.

use anyhow::Result;
use gpui::{App, Context, Task, Window};
use ropey::Rope;

use crate::input::{InputState, popovers::{SignatureHint, SignaturePopover}};

/// Signature help provider.
///
/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_signatureHelp>
pub trait SignatureProvider {
    /// textDocument/signatureHelp
    ///
    /// `overload` asks for a specific one; `None` takes the server's choice.
    fn signature_help(
        &self,
        _text: &Rope,
        _offset: usize,
        _overload: Option<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Option<SignatureHint>>>;
}

/// Where the call the caret sits in starts, or `None` when it is not in one.
///
/// Walks back over balanced brackets to the nearest unclosed `(`. **Stops at
/// a line that opens nothing** so a runaway scan cannot walk a whole file --
/// a call spanning more than a screen is not one the reader needs a hint for.
#[must_use]
pub(crate) fn call_anchor(text: &Rope, offset: usize, max_back: usize) -> Option<usize> {
    let start = offset.saturating_sub(max_back);
    let slice = text.slice(start..offset).to_string();
    let mut depth = 0i32;
    for (ix, ch) in slice.char_indices().rev() {
        match ch {
            ')' => depth += 1,
            '(' => {
                if depth == 0 {
                    return Some(start + ix);
                }
                depth -= 1;
            }
            // A statement boundary: there is no call to be inside of.
            ';' | '}' | '{' if depth == 0 => return None,
            _ => {}
        }
    }
    None
}

impl InputState {
    /// Ask for parameter hints, if the caret is inside a call (#246).
    ///
    /// Called when `(` or `,` is typed, and after an edit when the reader
    /// asked for that.
    pub(crate) fn request_signature_help(
        &mut self,
        overload: Option<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.mode.parameter_hints() {
            return;
        }
        let Some(provider) = self.lsp.signature_provider.clone() else {
            return;
        };
        let offset = self.cursor();
        let Some(anchor) = call_anchor(&self.text, offset, SIGNATURE_SCAN_BACK) else {
            self.signature_popover = None;
            cx.notify();
            return;
        };
        let text = self.text.clone();
        let task = provider.signature_help(&text, offset, overload, window, cx);
        self.lsp._signature_task = cx.spawn_in(window, async move |this, cx| {
            let hint = task.await.ok().flatten();
            let _ = this.update_in(cx, |state, window, cx| {
                match hint {
                    Some(hint) => {
                        let editor = cx.entity();
                        state.signature_popover = Some(SignaturePopover::new(
                            editor,
                            anchor..anchor + 1,
                            hint,
                            cx,
                        ));
                    }
                    None => state.signature_popover = None,
                }
                let _ = window;
                cx.notify();
            });
            Ok(())
        });
    }

    /// Take the hints away (Escape).
    pub(crate) fn hide_signature_help(&mut self, cx: &mut Context<Self>) {
        if self.signature_popover.take().is_some() {
            cx.notify();
        }
    }

    /// Take them away once the caret is no longer inside a call (#246).
    ///
    /// **Not the `)` key.** Auto-closing overtypes a closing bracket and
    /// returns before any text is written, so watching what was typed leaves
    /// the hints up over a call that is already finished. Watching **where
    /// the caret is** catches that, the arrow keys and a click alike.
    pub(crate) fn hide_signature_help_if_outside(&mut self, offset: usize, cx: &mut Context<Self>) {
        if self.signature_popover.is_none() {
            return;
        }
        if call_anchor(&self.text, offset, SIGNATURE_SCAN_BACK).is_none() {
            self.signature_popover = None;
            cx.notify();
        }
    }
}

/// How far back the bracket walk looks, in bytes.
///
/// Runs on a keystroke, so it is bounded: a call longer than this is not one
/// a single hint can help with anyway.
const SIGNATURE_SCAN_BACK: usize = 4096;

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        Rope::from(s)
    }

    #[test]
    fn the_anchor_is_the_bracket_that_opened_the_call() {
        let t = rope("foo(a, b");
        assert_eq!(call_anchor(&t, 8, 4096), Some(3));
    }

    /// Brackets inside the arguments do not count.
    #[test]
    fn nested_calls_are_balanced() {
        let t = rope("foo(bar(1), ");
        assert_eq!(call_anchor(&t, 12, 4096), Some(3));
    }

    #[test]
    fn a_closed_call_is_not_one_you_are_inside() {
        let t = rope("foo(a, b);");
        assert_eq!(call_anchor(&t, 10, 4096), None);
    }

    /// A statement boundary ends the walk: there is nothing to be inside of.
    #[test]
    fn the_walk_stops_at_a_statement() {
        let t = rope("let x = 1;\nlet y = 2");
        assert_eq!(call_anchor(&t, 20, 4096), None);
        let brace = rope("fn f() {\n    let y = 2");
        assert_eq!(call_anchor(&brace, 22, 4096), None);
    }

    #[test]
    fn plain_text_has_no_call() {
        let t = rope("hello world");
        assert_eq!(call_anchor(&t, 11, 4096), None);
        assert_eq!(call_anchor(&rope(""), 0, 4096), None);
    }
}
