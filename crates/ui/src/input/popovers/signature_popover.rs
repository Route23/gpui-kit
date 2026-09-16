//! The parameter hints shown while you are inside a call (#246).
//!
//! VS Code calls this `editor.parameterHints`, Zed `auto_signature_help`;
//! LSP calls it `textDocument/signatureHelp`. **Neither this fork nor
//! dopamine had any of it** -- this is the request, the popover and the
//! trigger.
//!
//! The popover itself is [`super::hover_popover::Popover`], which already
//! knows how to sit above or below a range and stay inside the window.

use std::{ops::Range, rc::Rc};

use gpui::{
    App, AppContext as _, Entity, IntoElement, ParentElement as _, Render, Styled, Window, div,
    prelude::FluentBuilder as _,
};

use crate::{
    ActiveTheme as _, StyledExt as _,
    input::{InputState, popovers::Popover},
};

/// One signature, as the popover needs to see it.
///
/// A trimmed `lsp_types::SignatureHelp`: which overload to show and which of
/// its parameters the caret is in, already decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureHint {
    /// The whole signature, e.g. `fn push(&mut self, value: T)`.
    pub label: String,
    /// The byte range inside `label` the active parameter covers, if the
    /// server said which one it is.
    pub active: Option<Range<usize>>,
    /// The signature's own documentation, already flattened to text.
    pub doc: Option<String>,
    /// Which overload this is, and how many there are (`2/3`). `None` when
    /// there is only one -- a count of one is noise.
    pub index: Option<(usize, usize)>,
}

pub struct SignaturePopover {
    editor: Entity<InputState>,
    /// The call's opening bracket, so the popover stays put while typing.
    pub(crate) anchor: Range<usize>,
    pub(crate) hint: Rc<SignatureHint>,
}

impl SignaturePopover {
    pub fn new(
        editor: Entity<InputState>,
        anchor: Range<usize>,
        hint: SignatureHint,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|_| Self {
            editor,
            anchor,
            hint: Rc::new(hint),
        })
    }

    /// Show the next overload, wrapping if the reader asked for that.
    #[allow(dead_code, reason = "wired by the host's Next Signature action")]
    pub(crate) fn cycle(&mut self, total: usize, wrap: bool) -> Option<usize> {
        let (ix, _) = self.hint.index?;
        if ix + 1 < total {
            Some(ix + 1)
        } else if wrap {
            Some(0)
        } else {
            None
        }
    }
}

impl Render for SignaturePopover {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        let hint = self.hint.clone();
        Popover::new(
            "signature-popover",
            self.editor.clone(),
            self.anchor.clone(),
            move |_, cx| {
                let label = hint.label.clone();
                // **Only the active parameter is bright.** The rest of the
                // signature is context; making it all the same weight is the
                // same as not answering "which argument am I on".
                let (before, active, after) = match hint.active.clone() {
                    Some(r) if r.end <= label.len() => (
                        label[..r.start].to_string(),
                        label[r.clone()].to_string(),
                        label[r.end..].to_string(),
                    ),
                    _ => (label.clone(), String::new(), String::new()),
                };
                div()
                    .v_flex()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(before)
                            .when(!active.is_empty(), |this| {
                                this.child(
                                    div()
                                        .font_bold()
                                        .text_color(cx.theme().foreground)
                                        .child(active),
                                )
                            })
                            .child(after),
                    )
                    .when_some(hint.index, |this, (ix, total)| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{}/{}", ix + 1, total)),
                        )
                    })
                    .when_some(hint.doc.clone(), |this, doc| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(doc),
                        )
                    })
                    .into_any_element()
            },
        )
        // Above the line: the arguments you are typing are below the caret,
        // and a popover under them covers what you are about to write.
        .prefer_above(true)
        .into_any_element()
    }
}
