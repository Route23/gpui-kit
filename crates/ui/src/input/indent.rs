use gpui::{
    Bounds, Context, EntityInputHandler as _, Hsla, Path, PathBuilder, Pixels, Point, SharedString,
    TextRun, TextStyle, Window, point, px,
};
use ropey::RopeSlice;

use crate::{
    ActiveTheme as _, RopeExt,
    input::{
        Indent, IndentInline, InputState, LastLayout, Outdent, OutdentInline, element::TextElement,
        mode::{GuideBackground, GuideColoring, InputMode},
    },
};

#[derive(Debug, Copy, Clone)]
pub struct TabSize {
    /// Default is 2
    pub tab_size: usize,
    /// Set true to use `\t` as tab indent, default is false
    pub hard_tabs: bool,
    /// How wide one level of indent is, when that differs from how wide a tab
    /// character is drawn (VS Code's `editor.indentSize`).
    ///
    /// `0` means "the same as `tab_size`", which is the usual case. Only the
    /// indent the editor *inserts* changes; a `\t` already in the file is
    /// still counted as `tab_size` columns, because that is what it looks
    /// like on screen.
    pub indent_size: usize,
}

impl Default for TabSize {
    fn default() -> Self {
        Self {
            tab_size: 2,
            hard_tabs: false,
            indent_size: 0,
        }
    }
}

impl TabSize {
    /// How many columns one level of indent is worth.
    #[inline]
    pub fn indent_unit(&self) -> usize {
        if self.indent_size == 0 {
            self.tab_size
        } else {
            self.indent_size
        }
    }

    pub(super) fn to_string(&self) -> SharedString {
        if self.hard_tabs {
            "\t".into()
        } else {
            " ".repeat(self.indent_unit()).into()
        }
    }

    /// Count the indent size of the line in spaces.
    pub fn indent_count(&self, line: &RopeSlice) -> usize {
        let mut count = 0;
        for ch in line.chars() {
            match ch {
                '\t' => count += self.tab_size,
                ' ' => count += 1,
                _ => break,
            }
        }

        count
    }
}

impl InputMode {
    #[inline]
    pub(super) fn is_indentable(&self) -> bool {
        match self {
            InputMode::PlainText { multi_line, .. } | InputMode::CodeEditor { multi_line, .. } => {
                *multi_line
            }
            _ => false,
        }
    }

    #[inline]
    pub(super) fn has_indent_guides(&self) -> bool {
        match self {
            InputMode::CodeEditor {
                indent_guides,
                multi_line,
                ..
            } => *indent_guides && *multi_line,
            _ => false,
        }
    }

    #[inline]
    pub(super) fn tab_size(&self) -> TabSize {
        match self {
            InputMode::PlainText { tab, .. } => *tab,
            InputMode::CodeEditor { tab, .. } => *tab,
            _ => TabSize::default(),
        }
    }
}

impl TextElement {
    /// Measure the indent width in pixels for given column count.
    pub(super) fn measure_indent_width(&self, style: &TextStyle, column: usize, window: &Window) -> Pixels {
        let font_size = style.font_size.to_pixels(window.rem_size());
        let layout = window.text_system().shape_line(
            SharedString::from(" ".repeat(column)),
            font_size,
            &[TextRun {
                len: column,
                font: style.font(),
                color: Hsla::default(),
                background_color: None,
                strikethrough: None,
                underline: None,
            }],
            None,
        );

        layout.width
    }

    /// The colour cycle for `IndentAware`, by depth.
    ///
    /// Six steps, then it repeats -- past that the nesting is the problem,
    /// not the colour. The same cycle is used for the background band at a
    /// much lower alpha, so a level's guide and its band match.
    fn guide_color(depth: usize, coloring: GuideColoring, cx: &gpui::App) -> Hsla {
        match coloring {
            GuideColoring::Disabled => cx.theme().border.opacity(0.85),
            GuideColoring::Fixed => cx.theme().accent_foreground.opacity(0.5),
            GuideColoring::IndentAware => {
                // Hue wheel in sixths, starting at the theme's accent so the
                // first level still looks like it belongs to the theme.
                let base = cx.theme().accent_foreground;
                let step = (depth % 6) as f32 / 6.;
                Hsla {
                    h: (base.h + step) % 1.,
                    s: 0.45,
                    l: base.l,
                    a: 0.55,
                }
            }
        }
    }

    /// Indent guides, grouped by (thickness, colour) so each group is one
    /// stroked path, plus the bands drawn behind the text.
    ///
    /// **A `PathBuilder` carries one stroke width**, so the active guide has
    /// to be its own path whether or not its colour differs -- which is why
    /// this returns a list instead of the single path it used to.
    pub(super) fn layout_indent_guides(
        &self,
        state: &InputState,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
        text_style: &TextStyle,
        window: &mut Window,
        cx: &gpui::App,
    ) -> (Vec<(Path<Pixels>, Hsla)>, Vec<(Bounds<Pixels>, Hsla)>) {
        let empty = (vec![], vec![]);
        if !state.mode.has_indent_guides() {
            return empty;
        }

        let indent_width =
            self.measure_indent_width(text_style, state.mode.tab_size().tab_size, window);

        let tab_size = state.mode.tab_size();
        let line_height = last_layout.line_height;
        let visible_range = last_layout.visible_range.clone();
        let coloring = state.mode.indent_guide_coloring();
        let background = state.mode.indent_guide_background();
        let (width, active_width) = state.mode.indent_guide_widths();
        let highlight_active = state.mode.indent_guide_active();

        // Which depth the caret sits in, counted the same way the guides are.
        let active_depth = highlight_active.then(|| {
            let row = state.text.offset_to_point(state.cursor()).row;
            let line = state.text.slice_line(row);
            tab_size.indent_count(&line) / tab_size.tab_size.max(1)
        });

        // depth -> the segments drawn at that depth.
        let mut segments: Vec<(usize, Point<Pixels>)> = vec![];
        let mut bands: Vec<(Bounds<Pixels>, Hsla)> = vec![];
        let mut offset_y = last_layout.visible_top;
        let mut last_indents: Vec<(usize, Pixels)> = vec![];
        for ix in visible_range {
            let line = state.text.slice_line(ix);
            let Some(line_layout) = last_layout.line(ix) else {
                continue;
            };
            // Hidden by a fold: no height, and no guide of its own -- the rows
            // inside a fold should not shape the guides drawn below it.
            if line_layout.wrapped_lines.is_empty() {
                continue;
            }

            let mut current_indents = vec![];
            if line.len() > 0 {
                let indent_count = tab_size.indent_count(&line);
                for (depth, offset) in (0..indent_count).step_by(tab_size.tab_size).enumerate() {
                    let x = if indent_count > 0 {
                        indent_width * offset as f32 / tab_size.tab_size as f32
                    } else {
                        px(0.)
                    };
                    current_indents.push((depth, x + last_layout.line_number_width));
                }
            } else {
                current_indents = last_indents.clone();
            }

            let height = line_layout.wrapped_lines.len() * line_height;
            for (depth, x) in &current_indents {
                segments.push((*depth, point(*x, offset_y)));
                if background == GuideBackground::IndentAware {
                    let color = Self::guide_color(*depth, GuideColoring::IndentAware, cx);
                    bands.push((
                        Bounds::new(
                            point(*x + bounds.origin.x, offset_y + bounds.origin.y),
                            gpui::size(indent_width, height),
                        ),
                        color.opacity(0.06),
                    ));
                }
            }

            offset_y += height + last_layout.extra_height(ix);
            last_indents = current_indents;
        }

        // Group by (thickness, colour). The key is cheap to compare and there
        // are at most a handful of groups on a screen.
        let mut groups: Vec<(f32, Hsla, PathBuilder)> = vec![];
        for (depth, pos) in segments {
            let is_active = active_depth == Some(depth);
            let w = if is_active { active_width } else { width };
            let color = if is_active && highlight_active {
                Self::guide_color(depth, coloring, cx).opacity(1.0)
            } else {
                Self::guide_color(depth, coloring, cx)
            };
            let slot = groups
                .iter_mut()
                .position(|(gw, gc, _)| *gw == w && *gc == color)
                .unwrap_or_else(|| {
                    groups.push((w, color, PathBuilder::stroke(px(w))));
                    groups.len() - 1
                });
            let b = &mut groups[slot].2;
            b.move_to(pos);
            b.line_to(point(pos.x, pos.y + line_height));
        }

        let mut paths = vec![];
        for (_, color, mut builder) in groups {
            builder.translate(bounds.origin);
            if let Ok(path) = builder.build() {
                paths.push((path, color));
            }
        }
        (paths, bands)
    }

}

impl InputState {
    /// Set whether to show indent guides in code editor mode, default is true.
    ///
    /// Only for [`InputMode::CodeEditor`] mode.
    pub fn indent_guides(mut self, indent_guides: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            indent_guides: l, ..
        } = &mut self.mode
        {
            *l = indent_guides;
        }
        self
    }

    /// Set indent guides in code editor mode.
    ///
    /// Only for [`InputMode::CodeEditor`] mode.
    pub fn set_indent_guides(
        &mut self,
        indent_guides: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            indent_guides: l, ..
        } = &mut self.mode
        {
            *l = indent_guides;
        }
        cx.notify();
    }

    /// Set the tab size after the input was built.
    ///
    /// The builder is not enough for a host that reads the indentation out of
    /// the file it just opened: rebuilding the state to change the width would
    /// throw away the undo history and the scroll position.
    ///
    /// Only for [`InputMode::PlainText`] and [`InputMode::CodeEditor`] mode
    /// with multi_line.
    pub fn set_tab_size(&mut self, tab: TabSize, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_multi_line() || self.mode.is_code_editor());
        match &mut self.mode {
            InputMode::PlainText { tab: t, .. } => *t = tab,
            InputMode::CodeEditor { tab: t, .. } => *t = tab,
            _ => return,
        }
        cx.notify();
    }

    /// Set the tab size for the input.
    ///
    /// Only for [`InputMode::PlainText`] and [`InputMode::CodeEditor`] mode with multi_line.
    pub fn tab_size(mut self, tab: TabSize) -> Self {
        debug_assert!(self.mode.is_multi_line() || self.mode.is_code_editor());
        match &mut self.mode {
            InputMode::PlainText { tab: t, .. } => *t = tab,
            InputMode::CodeEditor { tab: t, .. } => *t = tab,
            _ => {}
        }
        self
    }

    pub(super) fn indent_inline(
        &mut self,
        _: &IndentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // First, try to accept inline completion if present
        if self.accept_inline_completion(window, cx) {
            return;
        }
        // Tab inside a list marker indents the whole item, the way Markdown
        // editors do (#248, Zed's `indent_list_on_tab`).
        if self.tab_indents_list() {
            self.indent(true, window, cx);
            return;
        }
        self.indent(false, window, cx);
    }

    pub(super) fn indent_block(&mut self, _: &Indent, window: &mut Window, cx: &mut Context<Self>) {
        self.indent(true, window, cx);
    }

    pub(super) fn outdent_inline(
        &mut self,
        _: &OutdentInline,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.outdent(false, window, cx);
    }

    pub(super) fn outdent_block(
        &mut self,
        _: &Outdent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.outdent(true, window, cx);
    }

    pub(super) fn indent(&mut self, block: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_indentable() {
            cx.propagate();
            return;
        };

        let tab_indent = self.mode.tab_size().to_string();
        let selected_range = self.selected_range;
        let mut added_len = 0;
        let is_selected = !self.selected_range.is_empty();

        if is_selected || block {
            let start_offset = self.start_of_line_of_selection(window, cx);
            let mut offset = start_offset;

            let selected_text = self
                .text_for_range(
                    self.range_to_utf16(&(offset..selected_range.end)),
                    &mut None,
                    window,
                    cx,
                )
                .unwrap_or("".into());

            for line in selected_text.split('\n') {
                self.replace_text_in_range_silent(
                    Some(self.range_to_utf16(&(offset..offset))),
                    &tab_indent,
                    window,
                    cx,
                );
                added_len += tab_indent.len();
                // +1 for "\n", the `\r` is included in the `line`.
                offset += line.len() + tab_indent.len() + 1;
            }

            if is_selected {
                self.selected_range = (start_offset..selected_range.end + added_len).into();
            } else {
                self.selected_range =
                    (selected_range.start + added_len..selected_range.end + added_len).into();
            }
        } else {
            // Selected none
            let offset = self.selected_range.start;
            self.replace_text_in_range_silent(
                Some(self.range_to_utf16(&(offset..offset))),
                &tab_indent,
                window,
                cx,
            );
            added_len = tab_indent.len();

            self.selected_range =
                (selected_range.start + added_len..selected_range.end + added_len).into();
        }
    }

    pub(super) fn outdent(&mut self, block: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.mode.is_indentable() {
            cx.propagate();
            return;
        };

        let tab_indent = self.mode.tab_size().to_string();
        let selected_range = self.selected_range;
        let mut removed_len = 0;
        let is_selected = !self.selected_range.is_empty();

        if is_selected || block {
            let start_offset = self.start_of_line_of_selection(window, cx);
            let mut offset = start_offset;

            let selected_text = self
                .text_for_range(
                    self.range_to_utf16(&(offset..selected_range.end)),
                    &mut None,
                    window,
                    cx,
                )
                .unwrap_or("".into());

            for line in selected_text.split('\n') {
                if line.starts_with(tab_indent.as_ref()) {
                    self.replace_text_in_range_silent(
                        Some(self.range_to_utf16(&(offset..offset + tab_indent.len()))),
                        "",
                        window,
                        cx,
                    );
                    removed_len += tab_indent.len();

                    // +1 for "\n"
                    offset += line.len().saturating_sub(tab_indent.len()) + 1;
                } else {
                    offset += line.len() + 1;
                }
            }

            if is_selected {
                self.selected_range =
                    (start_offset..selected_range.end.saturating_sub(removed_len)).into();
            } else {
                self.selected_range = (selected_range.start.saturating_sub(removed_len)
                    ..selected_range.end.saturating_sub(removed_len))
                    .into();
            }
        } else {
            // Selected none
            let start_offset = self.selected_range.start;
            let offset = self.start_of_line_of_selection(window, cx);
            let offset = self.offset_from_utf16(self.offset_to_utf16(offset));
            // FIXME: To improve performance
            if self
                .text
                .slice(offset..self.text.len())
                .to_string()
                .starts_with(tab_indent.as_ref())
            {
                self.replace_text_in_range_silent(
                    Some(self.range_to_utf16(&(offset..offset + tab_indent.len()))),
                    "",
                    window,
                    cx,
                );
                removed_len = tab_indent.len();
                let new_offset = start_offset.saturating_sub(removed_len);
                self.selected_range = (new_offset..new_offset).into();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ropey::RopeSlice;

    use super::TabSize;

    #[test]
    fn test_tab_size() {
        let tab = TabSize {
            tab_size: 2,
            hard_tabs: false,
    indent_size: 0,
        };
        assert_eq!(tab.to_string(), "  ");
        let tab = TabSize {
            tab_size: 4,
            hard_tabs: false,
    indent_size: 0,
        };
        assert_eq!(tab.to_string(), "    ");

        let tab = TabSize {
            tab_size: 2,
            hard_tabs: true,
    indent_size: 0,
        };
        assert_eq!(tab.to_string(), "\t");
        let tab = TabSize {
            tab_size: 4,
            hard_tabs: true,
    indent_size: 0,
        };
        assert_eq!(tab.to_string(), "\t");
    }

    #[test]
    fn test_tab_size_indent_count() {
        let tab = TabSize {
            tab_size: 4,
            hard_tabs: false,
    indent_size: 0,
        };
        assert_eq!(tab.indent_count(&RopeSlice::from("abc")), 0);
        assert_eq!(tab.indent_count(&RopeSlice::from("  abc")), 2);
        assert_eq!(tab.indent_count(&RopeSlice::from("    abc")), 4);
        assert_eq!(tab.indent_count(&RopeSlice::from("\tabc")), 4);
        assert_eq!(tab.indent_count(&RopeSlice::from("  \tabc")), 6);
        assert_eq!(tab.indent_count(&RopeSlice::from(" \t abc  ")), 6);
        assert_eq!(tab.indent_count(&RopeSlice::from("abc")), 0);
    }
}
