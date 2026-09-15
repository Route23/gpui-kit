use std::{ops::Range, time::Duration};

use anyhow::Result;
use gpui::{App, Context, Task, Window};
use ropey::Rope;

use crate::input::{InputState, Lsp, RopeExt};

/// How long the caret must sit still before asking.
///
/// The caret moves on every arrow key and 60 times a second during a drag;
/// each ask is a language server round trip.
const HIGHLIGHT_DEBOUNCE: Duration = Duration::from_millis(250);

/// What the server said the occurrence is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HighlightKind {
    /// Just a mention.
    Text,
    /// The value is read here.
    Read,
    /// The value is written here.
    Write,
}

/// Where else the symbol under the caret appears.
///
/// <https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_documentHighlight>
pub trait DocumentHighlightProvider {
    fn document_highlights(
        &self,
        text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<(lsp_types::Range, HighlightKind)>>>;
}

impl Lsp {
    /// The highlights that fall inside the visible rows, as byte ranges.
    pub(crate) fn document_highlights_for_range(
        &self,
        text: &Rope,
        visible_range: &Range<usize>,
    ) -> Vec<(Range<usize>, HighlightKind)> {
        self.document_highlights
            .iter()
            .filter_map(|(range, kind)| {
                if (range.start.line as usize) > visible_range.end
                    || (range.end.line as usize) < visible_range.start
                {
                    return None;
                }
                let start = text.position_to_offset(&range.start);
                let end = text.position_to_offset(&range.end);
                Some((start..end, *kind))
            })
            .collect()
    }

    /// Ask again, after the caret has been still for a moment.
    pub(crate) fn update_document_highlights(
        &mut self,
        text: &Rope,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.document_highlight_provider.clone() else {
            return;
        };
        let text = text.clone();
        self._document_highlight_task = cx.spawn_in(window, async move |editor, cx| {
            cx.background_executor().timer(HIGHLIGHT_DEBOUNCE).await;
            let task = editor.update_in(cx, |_editor, window, cx| {
                provider.document_highlights(&text, offset, window, cx)
            })?;
            let found = task.await?;
            editor.update(cx, |editor, cx| {
                if found == editor.lsp.document_highlights {
                    return;
                }
                editor.lsp.document_highlights = found;
                cx.notify();
            })?;
            Ok(())
        });
    }

    /// Forget them -- the text changed, so the offsets are stale.
    pub(crate) fn clear_document_highlights(&mut self) {
        self.document_highlights.clear();
    }
}
