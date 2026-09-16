use anyhow::Result;
use gpui::{App, Context, Hsla, MouseMoveEvent, Task, Window};
use ropey::Rope;
use std::rc::Rc;

use crate::input::{popovers::ContextMenu, InputState, RopeExt};

mod code_actions;
mod completions;
mod definitions;
mod document_colors;
mod document_highlights;
mod hover;
mod signature;

pub use code_actions::*;
pub use completions::*;
pub use definitions::*;
pub use document_colors::*;
pub use document_highlights::*;
pub use hover::*;
pub use signature::*;

/// LSP ServerCapabilities
///
/// https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/#serverCapabilities
pub struct Lsp {
    /// The completion provider.
    pub completion_provider: Option<Rc<dyn CompletionProvider>>,
    /// The code action providers.
    pub code_action_providers: Vec<Rc<dyn CodeActionProvider>>,
    /// The hover provider.
    pub hover_provider: Option<Rc<dyn HoverProvider>>,
    /// The definition provider.
    pub definition_provider: Option<Rc<dyn DefinitionProvider>>,
    /// The document color provider.
    pub document_color_provider: Option<Rc<dyn DocumentColorProvider>>,
    /// Where else the symbol under the caret appears.
    pub document_highlight_provider: Option<Rc<dyn DocumentHighlightProvider>>,
    /// What the call under the caret takes (#246).
    pub signature_provider: Option<Rc<dyn SignatureProvider>>,

    document_colors: Vec<(lsp_types::Range, Hsla)>,
    document_highlights: Vec<(lsp_types::Range, HighlightKind)>,
    _hover_task: Task<Result<()>>,
    _hover_hide_task: Task<Result<()>>,
    _document_color_task: Task<Result<()>>,
    _document_highlight_task: Task<Result<()>>,
    _signature_task: Task<Result<()>>,
}

impl Default for Lsp {
    fn default() -> Self {
        Self {
            completion_provider: None,
            code_action_providers: vec![],
            hover_provider: None,
            definition_provider: None,
            document_color_provider: None,
            document_highlight_provider: None,
            signature_provider: None,
            document_colors: vec![],
            document_highlights: vec![],
            _hover_task: Task::ready(Ok(())),
            _hover_hide_task: Task::ready(Ok(())),
            _document_color_task: Task::ready(Ok(())),
            _document_highlight_task: Task::ready(Ok(())),
            _signature_task: Task::ready(Ok(())),
        }
    }
}

impl Lsp {
    /// Whether the server has answered with occurrences.
    pub(crate) fn has_document_highlights(&self) -> bool {
        !self.document_highlights.is_empty()
    }

    /// Update the LSP when the text changes.
    pub(crate) fn update(
        &mut self,
        text: &Rope,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        self.update_document_colors(text, window, cx);
        // The offsets the server gave are for the text before this edit.
        self.clear_document_highlights();
    }

    /// Reset all LSP states.
    pub(crate) fn reset(&mut self) {
        self.document_colors.clear();
        self._hover_task = Task::ready(Ok(()));
        self._hover_hide_task = Task::ready(Ok(()));
        self._document_color_task = Task::ready(Ok(()));
    }
}

impl InputState {
    pub(crate) fn hide_context_menu(&mut self, cx: &mut Context<Self>) {
        self.context_menu = None;
        self._context_menu_task = Task::ready(Ok(()));
        cx.notify();
    }

    pub(crate) fn is_context_menu_open(&self, cx: &App) -> bool {
        let Some(menu) = self.context_menu.as_ref() else {
            return false;
        };

        menu.is_open(cx)
    }

    /// Handles an action for the completion menu, if it exists.
    ///
    /// Return true if the action was handled, otherwise false.
    pub fn handle_action_for_context_menu(
        &mut self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(menu) = self.context_menu.as_ref() else {
            return false;
        };

        let mut handled = false;

        match menu {
            ContextMenu::Completion(menu) => {
                _ = menu.update(cx, |menu, cx| {
                    handled = menu.handle_action(action, window, cx)
                });
            }
            ContextMenu::CodeAction(menu) => {
                _ = menu.update(cx, |menu, cx| {
                    handled = menu.handle_action(action, window, cx)
                });
            }
            ContextMenu::MouseContext(..) => {}
        };

        handled
    }

    /// Apply a list of [`lsp_types::TextEdit`] to mutate the text.
    pub fn apply_lsp_edits(
        &mut self,
        text_edits: &Vec<lsp_types::TextEdit>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for edit in text_edits {
            let start = self.text.position_to_offset(&edit.range.start);
            let end = self.text.position_to_offset(&edit.range.end);

            let range_utf16 = self.range_to_utf16(&(start..end));
            self.replace_text_in_range_silent(Some(range_utf16), &edit.new_text, window, cx);
        }
    }

    pub(super) fn handle_mouse_move(
        &mut self,
        offset: usize,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<InputState>,
    ) {
        if event.modifiers.secondary() {
            self.handle_hover_definition(offset, window, cx);
        } else {
            self.hover_definition.clear();
            self.handle_hover_popover(offset, window, cx);
        }
        cx.notify();
    }
}
