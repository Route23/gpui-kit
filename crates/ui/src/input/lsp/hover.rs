use std::time::Duration;

use anyhow::Result;
use gpui::{App, Context, Task, Window};
use ropey::Rope;

use crate::input::{popovers::HoverPopover, InputState, RopeExt};

/// Hover provider
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
pub trait HoverProvider {
    /// textDocument/hover
    ///
    /// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#textDocument_hover
    fn hover(
        &self,
        _text: &Rope,
        _offset: usize,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Task<Result<Option<lsp_types::Hover>>>;
}

impl InputState {
    /// Hide the hover popover once the pointer has been away long enough.
    ///
    /// Zero means "do not hide on leave", which is what the editor did before
    /// this was settable: the popover only went away when the provider
    /// answered `None`, or when the input lost focus. That leaves it up for
    /// good when the pointer leaves the editor entirely -- no more mouse
    /// moves arrive to ask about.
    ///
    /// **Rearmed on every move**, so the wait is measured from the last one.
    pub(super) fn schedule_hover_hide(&mut self, cx: &mut Context<InputState>) {
        let ms = u64::from(self.mode.hover_hiding_delay());
        if ms == 0 || self.hover_popover.is_none() {
            return;
        }
        self.lsp._hover_hide_task = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            let _ = this.update(cx, |state, cx| {
                // Sticky: the pointer is on the popover, so it is still wanted.
                // The popover has to say so -- it occludes, and the editor
                // stops hearing about the pointer the moment it lands on it.
                if state.mode.hover_sticky() && state.hover_popover_hovered {
                    return;
                }
                if state.hover_popover.take().is_some() {
                    cx.notify();
                }
            });
            Ok(())
        });
    }

    /// Handle hover trigger LSP request.
    pub(super) fn handle_hover_popover(
        &mut self,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        // Off means off: drop whatever is showing too, or turning it off
        // looks like it did not take.
        if !self.mode.hover() {
            if self.hover_popover.take().is_some() {
                cx.notify();
            }
            return;
        }

        if self.selecting {
            return;
        }

        let Some(provider) = self.lsp.hover_provider.clone() else {
            return;
        };

        if let Some(hover_popover) = self.hover_popover.as_ref() {
            if hover_popover.read(cx).is_same(offset) {
                return;
            }
        }

        // Currently not implemented.
        let task = provider.hover(&self.text, offset, window, cx);
        let mut symbol_range = self.text.word_range(offset).unwrap_or(offset..offset);
        let editor = cx.entity();
        let should_delay = self.hover_popover.is_none();
        let delay = u64::from(self.mode.hover_delay());
        self.lsp._hover_task = cx.spawn_in(window, async move |_, cx| {
            if should_delay && delay > 0 {
                cx.background_executor()
                    .timer(Duration::from_millis(delay))
                    .await;
            }

            let result = task.await?;

            _ = editor.update(cx, |editor, cx| match result {
                Some(hover) => {
                    if let Some(range) = hover.range {
                        let start = editor.text.position_to_offset(&range.start);
                        let end = editor.text.position_to_offset(&range.end);
                        symbol_range = start..end;
                    }
                    let hover_popover = HoverPopover::new(cx.entity(), symbol_range, &hover, cx);
                    editor.hover_popover = Some(hover_popover);
                }
                None => {
                    editor.hover_popover = None;
                }
            });

            Ok(())
        });
    }
}
