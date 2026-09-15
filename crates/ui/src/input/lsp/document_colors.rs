use std::{ops::Range, time::Duration};

/// How long to wait after an edit before asking for colours again.
///
/// A colour literal does not move while someone is in the middle of typing
/// one, and every ask is a language server round trip.
const COLOR_DEBOUNCE: Duration = Duration::from_millis(300);

use anyhow::Result;
use gpui::{App, Context, Hsla, Task, Window};
use lsp_types::ColorInformation;
use ropey::Rope;

use crate::input::{InputState, Lsp, RopeExt};

pub trait DocumentColorProvider {
    /// Fetches document colors for the specified range.
    ///
    /// textDocument/documentColor
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_documentColor
    fn document_colors(
        &self,
        _text: &Rope,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<ColorInformation>>>;
}

impl Lsp {
    /// Get document colors that intersect with the visible range (0-based row).
    ///
    /// Returns byte ranges and colors.
    pub(crate) fn document_colors_for_range(
        &self,
        text: &Rope,
        visible_range: &Range<usize>,
    ) -> Vec<(Range<usize>, Hsla)> {
        self.document_colors
            .iter()
            .filter_map(|(range, color)| {
                if (range.start.line as usize) > visible_range.end
                    || (range.end.line as usize) < visible_range.start
                {
                    return None;
                }

                let start = text.position_to_offset(&range.start);
                let end = text.position_to_offset(&range.end);

                Some((start..end, *color))
            })
            .collect()
    }

    pub(crate) fn update_document_colors(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        let Some(provider) = self.document_color_provider.as_ref() else {
            return;
        };

        let text = text.clone();
        let provider = provider.clone();
        // **Its own slot, and a wait.** This was writing into `_hover_task`,
        // so asking for colours cancelled an in-flight hover and the other
        // way round -- and it fired on **every keystroke**, one language
        // server round trip per character typed.
        self._document_color_task = cx.spawn_in(window, async move |editor, cx| {
            cx.background_executor().timer(COLOR_DEBOUNCE).await;
            let task = editor.update_in(cx, |editor, window, cx| {
                let _ = editor;
                provider.document_colors(&text, window, cx)
            })?;
            let colors = task.await?;

            editor.update(cx, |editor, cx| {
                let mut document_colors: Vec<(lsp_types::Range, Hsla)> = colors
                    .iter()
                    .map(|info| {
                        let color = gpui::Rgba {
                            r: info.color.red,
                            g: info.color.green,
                            b: info.color.blue,
                            a: info.color.alpha,
                        }
                        .into();

                        (info.range, color)
                    })
                    .collect();
                document_colors.sort_by_key(|(range, _)| range.start);

                if document_colors == editor.lsp.document_colors {
                    return;
                }
                editor.lsp.document_colors = document_colors;
                cx.notify();
            })?;

            Ok(())
        });
    }
}
