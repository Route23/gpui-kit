//! Clickable ranges the host hands over (#256).
//!
//! The editor does not look for links itself. A host knows what counts as
//! one -- which URI schemes it will follow, whether a bare path should open,
//! what the working directory is -- so it hands over byte ranges with
//! [`InputState::set_link_ranges`] and hears back through
//! [`InputEvent::LinkClicked`].
//!
//! # Only while the modifier is down
//!
//! The underline appears only when the pointer is over a range **with the
//! secondary modifier held**, exactly like "go to definition"
//! (`lsp/definitions.rs`). Ordinary reading therefore looks the same as it
//! always did, which is why the host can leave this on by default.

use std::ops::Range;

use gpui::{App, Context, HighlightStyle, Hitbox, UnderlineStyle, Window, px};

use crate::{ActiveTheme, input::{InputState, element::TextElement}};

impl InputState {
    /// Hand over the ranges that may be clicked (byte offsets).
    ///
    /// **Replaces the previous set.** Passing an empty vec is how a host
    /// turns the feature off.
    pub fn set_link_ranges(&mut self, ranges: Vec<Range<usize>>, cx: &mut Context<Self>) {
        if self.link_ranges == ranges {
            return;
        }
        self.link_ranges = ranges;
        // The pointer may have been over one that is gone now.
        if !self
            .link_hover
            .as_ref()
            .is_some_and(|r| self.link_ranges.contains(r))
        {
            self.link_hover = None;
        }
        cx.notify();
    }

    /// The link covering `offset`, if there is one.
    pub(super) fn link_at(&self, offset: usize) -> Option<Range<usize>> {
        self.link_ranges
            .iter()
            .find(|r| r.contains(&offset))
            .cloned()
    }

    /// Follow the pointer: underline a link only while the modifier is down.
    ///
    /// Returns true when the highlight changed, so the caller can repaint.
    pub(super) fn track_link_hover(&mut self, offset: usize, secondary: bool) -> bool {
        let next = if secondary { self.link_at(offset) } else { None };
        if next == self.link_hover {
            return false;
        }
        self.link_hover = next;
        true
    }
}

impl TextElement {
    /// The underline under the link the pointer is on.
    pub(crate) fn layout_link_hover(&self, cx: &App) -> Option<(Range<usize>, HighlightStyle)> {
        let editor = self.state.read(cx);
        let range = editor.link_hover.clone()?;

        let mut style: HighlightStyle = cx
            .theme()
            .highlight_theme
            .link_text
            .map(Into::into)
            .unwrap_or_default();
        style.underline = Some(UnderlineStyle {
            thickness: px(1.),
            ..UnderlineStyle::default()
        });
        Some((range, style))
    }

    /// The hitbox that turns the pointer into a hand.
    pub(crate) fn layout_link_hitbox(
        &self,
        editor: &InputState,
        window: &mut Window,
        _cx: &App,
    ) -> Option<Hitbox> {
        let range = editor.link_hover.as_ref()?;
        let bounds = editor.range_to_bounds(range)?;
        Some(window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal))
    }
}
