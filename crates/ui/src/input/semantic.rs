//! Colours a language server assigned to spans of text (#388).
//!
//! The syntax pass reads a **tree**, and a tree only sees shape: the same
//! `foo` may be a variable here and a function there, and `static` /
//! `async` are properties no shape can show. `textDocument/semanticTokens`
//! is the server saying what each word actually is.
//!
//! # The host hands over names, not colours
//!
//! A span carries a **capture name** (`function`, `type`, `preproc`, …), the
//! same vocabulary the syntax theme already speaks. The colour is looked up
//! here, so swapping the theme recolours the spans without the host doing
//! anything -- and a host that knows nothing about colours can supply them.
//!
//! # Laid over the tree, not mixed with it
//!
//! `combine_highlights` folds overlapping styles through an unordered set, so
//! two passes that set the same `.color` swap winners between frames. These
//! are applied with `overwrite_colors` instead -- the same deterministic path
//! bracket colouring uses -- **after** the tree and **before** the brackets.

use std::ops::Range;

use gpui::{Context, SharedString};

use crate::input::InputState;

/// One span of text the server named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticSpan {
    /// Byte range in the buffer.
    pub range: Range<usize>,
    /// A capture name the syntax theme knows (`function`, `type`, …). A name
    /// the theme does not know leaves the span with the colour the tree gave
    /// it.
    pub capture: SharedString,
}

impl InputState {
    /// Hand over what the server said about this buffer.
    ///
    /// **Replaces the previous set**, and expects the spans sorted by
    /// `range.start` and non-overlapping (what the LSP wire format gives you
    /// once expanded). Passing an empty vec is how a host turns this off.
    pub fn set_semantic_spans(&mut self, spans: Vec<SemanticSpan>, cx: &mut Context<Self>) {
        if self.semantic_spans == spans {
            return;
        }
        self.semantic_spans = spans;
        cx.notify();
    }

    /// Drop them all -- the file changed, the server died, the setting went
    /// off. Cheap to call when there is nothing to drop.
    pub fn clear_semantic_spans(&mut self, cx: &mut Context<Self>) {
        self.set_semantic_spans(Vec::new(), cx);
    }

    /// The spans overlapping `range`, in order.
    ///
    /// Kept out of the paint path's way: the spans cover the whole file and
    /// only the visible slice is ever needed.
    pub(super) fn semantic_spans_in(&self, range: &Range<usize>) -> &[SemanticSpan] {
        if self.semantic_spans.is_empty() {
            return &[];
        }
        let start = self
            .semantic_spans
            .partition_point(|s| s.range.end <= range.start);
        let end = self
            .semantic_spans
            .partition_point(|s| s.range.start < range.end);
        self.semantic_spans.get(start..end).unwrap_or(&[])
    }
}
