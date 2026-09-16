//! Drawing whitespace as visible marks (VS Code's `editor.renderWhitespace`).
//!
//! The decision of *which* byte offsets get a mark is a pure function
//! ([`marks_for_line`]) so it can be unit tested; the pixel positions come from
//! the already-shaped lines, the same way [`super::indent`] draws indent guides.

use std::ops::Range;

use gpui::{Bounds, Pixels, Point, Window, point, px};

use crate::{
    ActiveTheme as _, RopeExt,
    input::{InputState, LastLayout, element::TextElement, mode::RenderWhitespace},
};

/// A drawn whitespace character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WsKind {
    /// A space: drawn as a small centered dot.
    Space,
    /// A tab: drawn as an arrow spanning the tab's own advance.
    Tab,
    /// A control character: drawn as a filled box, because the glyph itself
    /// is usually blank and there is nothing else to see.
    Control,
    /// A character that is invisible or looks like ASCII without being it:
    /// drawn as a box behind it, so the character stays readable.
    Suspicious,
}

/// Is this an invisible or unusual space?
///
/// A plain space and a tab are **not** here -- those are
/// [`super::mode::RenderWhitespace`]'s job, and marking them twice would put
/// two boxes on one character.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        // No-break space, ogham space, the en/em quad family, narrow and
        // medium spaces, the zero-width family, line/paragraph separators,
        // word joiner, and the byte-order mark.
        '\u{00a0}' | '\u{1680}' | '\u{2000}'..='\u{200f}'
            | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}'
            | '\u{2060}' | '\u{3000}' | '\u{feff}'
    )
}

/// Does this look like an ASCII character without being one?
///
/// **Deliberately a short, hand-picked list.** VS Code ships a generated
/// confusables table and the machinery to regenerate it; this is the set that
/// actually bites in practice, and it can grow when something new does.
fn is_ambiguous(c: char) -> bool {
    matches!(
        c,
        // Full-width forms: `Ａ`..`ｚ`, `！`..`～`, and the ideographic space,
        // which is the one that costs Japanese readers the most time.
        '\u{3000}' | '\u{ff01}'..='\u{ff5e}'
            // Cyrillic letters drawn like Latin ones.
            | 'а' | 'в' | 'е' | 'к' | 'м' | 'н' | 'о' | 'р' | 'с' | 'т' | 'у' | 'х'
            | 'А' | 'В' | 'Е' | 'К' | 'М' | 'Н' | 'О' | 'Р' | 'С' | 'Т' | 'У' | 'Х'
            // Greek letters drawn like Latin ones.
            | 'ο' | 'ν' | 'Ο' | 'Α' | 'Β' | 'Ε' | 'Ζ' | 'Η' | 'Ι' | 'Κ' | 'Μ'
            | 'Ν' | 'Ρ' | 'Τ' | 'Υ' | 'Χ'
            // Quotation marks and dashes that get pasted in from prose.
            | '\u{2018}' | '\u{2019}' | '\u{201c}' | '\u{201d}'
            | '\u{2010}'..='\u{2015}' | '\u{2212}'
    )
}

/// Which characters of `line` get a box that is not about whitespace.
///
/// Pure so it can be unit tested. `allowed` is the reader's "never mark
/// these" list, taken as a plain string because that is how it is written in
/// a settings file.
pub(super) fn extra_marks_for_line(
    line: &str,
    control: bool,
    unicode: super::mode::UnicodeHighlight,
    allowed: &str,
) -> Vec<Mark> {
    if !control && !unicode.is_visible() {
        return vec![];
    }
    let end = line.strip_suffix('\r').map(str::len).unwrap_or(line.len());
    let mut marks = vec![];
    for (offset, ch) in line[..end].char_indices() {
        if allowed.contains(ch) {
            continue;
        }
        // A control character is drawn even when it is also "invisible" --
        // it is the more specific thing to say.
        let kind = if control && is_control(ch) {
            WsKind::Control
        } else if unicode.wants_invisible() && is_invisible(ch) {
            WsKind::Suspicious
        } else if unicode.wants_ambiguous() && is_ambiguous(ch) {
            WsKind::Suspicious
        } else {
            continue;
        };
        marks.push(Mark { offset, kind });
    }
    marks
}

/// Is this a control character worth showing?
///
/// **`\t` and `\r` are not.** The tab has its own mark and the carriage
/// return belongs to the line ending -- boxing either would put a mark on
/// every line of an ordinary file.
fn is_control(c: char) -> bool {
    (c.is_control() && c != '\t' && c != '\r' && c != '\n')
        || matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// One mark to draw: the byte offset within the line, and what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mark {
    pub(super) offset: usize,
    pub(super) kind: WsKind,
}

/// Which whitespace characters of `line` get a mark.
///
/// - `line` is one buffer row **without** its `\n` (a trailing `\r` is treated
///   as part of the line ending, not as whitespace to mark).
/// - `selection` is the selection's byte range **local to this line**, and is
///   only consulted by [`RenderWhitespace::Selection`].
pub(super) fn marks_for_line(
    line: &str,
    mode: RenderWhitespace,
    selection: Option<Range<usize>>,
) -> Vec<Mark> {
    if !mode.is_visible() {
        return vec![];
    }

    // `\r` belongs to the line ending. Marking it would put a dot past the end
    // of every line of a CRLF file.
    let end = line.strip_suffix('\r').map(str::len).unwrap_or(line.len());

    // Where the trailing whitespace run starts.
    let trailing_start = line[..end]
        .rfind(|c: char| c != ' ' && c != '\t')
        .map(|ix| ix + line[ix..].chars().next().map(char::len_utf8).unwrap_or(1))
        .unwrap_or(0);

    let bytes = line.as_bytes();
    let mut marks = vec![];
    for (offset, ch) in line[..end].char_indices() {
        let kind = match ch {
            ' ' => WsKind::Space,
            '\t' => WsKind::Tab,
            _ => continue,
        };

        let keep = match mode {
            RenderWhitespace::None => false,
            RenderWhitespace::All => true,
            RenderWhitespace::Trailing => offset >= trailing_start,
            RenderWhitespace::Selection => selection
                .as_ref()
                .is_some_and(|s| offset >= s.start && offset < s.end),
            // Everything except a *single* space between two words. Tabs are
            // always drawn; so are leading, trailing and repeated spaces.
            RenderWhitespace::Boundary => {
                kind == WsKind::Tab
                    || offset >= trailing_start
                    || offset == 0
                    || bytes.get(offset - 1) == Some(&b' ')
                    || bytes.get(offset + 1) == Some(&b' ')
            }
        };

        if keep {
            marks.push(Mark { offset, kind });
        }
    }

    marks
}

/// A mark placed in the pane, ready to paint.
pub(super) struct PlacedMark {
    pub(super) origin: Point<Pixels>,
    /// The advance of the character, used to size the tab arrow.
    pub(super) width: Pixels,
    pub(super) kind: WsKind,
}

impl TextElement {
    /// Place a whitespace mark on every character [`marks_for_line`] selects.
    ///
    /// Returns positions relative to the window, already offset by the gutter
    /// and by `bounds.origin` — the same contract as
    /// [`TextElement::layout_indent_guides`].
    pub(super) fn layout_whitespaces(
        &self,
        state: &InputState,
        bounds: &Bounds<Pixels>,
        last_layout: &LastLayout,
    ) -> Vec<PlacedMark> {
        let mode = state.mode.render_whitespace();
        let control = state.mode.render_control_characters();
        let unicode = state.mode.unicode_highlight();
        let allowed = state.mode.unicode_allowed();
        if !mode.is_visible() && !control && !unicode.is_visible() {
            return vec![];
        }

        let selection: Range<usize> = state.selected_range.into();
        // Only the whitespace marks follow the selection; a suspicious
        // character is suspicious wherever it is.
        let ws_off = mode.follows_selection() && selection.is_empty();

        let line_height = last_layout.line_height;
        let mut placed = vec![];
        let mut offset_y = last_layout.visible_top;
        // Byte offset of the first visible row, walked forward per row so the
        // selection can be cut down to a line-local range.
        let mut row_offset = last_layout.visible_range_offset.start;

        for row in last_layout.visible_range.clone() {
            let line = state.text.slice_line(row).to_string();
            let Some(line_layout) = last_layout.line(row) else {
                row_offset += line.len() + 1;
                continue;
            };

            let local_selection = mode.follows_selection().then(|| {
                selection.start.saturating_sub(row_offset)
                    ..selection.end.saturating_sub(row_offset).min(line.len())
            });

            let ws = if ws_off {
                vec![]
            } else {
                marks_for_line(&line, mode, local_selection)
            };
            let extra = extra_marks_for_line(&line, control, unicode, &allowed);
            for mark in ws.into_iter().chain(extra) {
                let Some(pos) = line_layout.position_for_index(mark.offset, line_height) else {
                    continue;
                };
                // A tab is shaped as a single glyph with whatever advance the
                // font gives it — there are no tab stops here, so the width has
                // to be measured rather than derived from `tab_size`.
                let next = line_layout
                    .position_for_index(mark.offset + 1, line_height)
                    .filter(|next| next.y == pos.y);
                let width = next.map(|next| next.x - pos.x).unwrap_or(px(0.));

                placed.push(PlacedMark {
                    origin: bounds.origin
                        + point(pos.x + last_layout.line_number_width, pos.y + offset_y),
                    width,
                    kind: mark.kind,
                });
            }

            offset_y += line_layout.wrapped_lines.len() as f32 * line_height;
            row_offset += line.len() + 1;
        }

        placed
    }

    /// Paint the marks collected by [`TextElement::layout_whitespaces`].
    pub(super) fn paint_whitespaces(
        marks: &[PlacedMark],
        line_height: Pixels,
        map: &super::mode::WhitespaceMap,
        text_style: &gpui::TextStyle,
        window: &mut Window,
        cx: &mut gpui::App,
    ) {
        if marks.is_empty() {
            return;
        }

        let color = cx.theme().muted_foreground.opacity(0.4);
        let middle = line_height / 2.;

        for mark in marks {
            // A character the reader chose is shaped and drawn in place of the
            // built-in quad (Zed's `whitespace_map`). **Only when they chose
            // one** -- shaping costs a `shape_line` per mark, and the default
            // dot and arrow look the same for free.
            let glyph = match mark.kind {
                WsKind::Space => map.space,
                WsKind::Tab => map.tab,
                _ => None,
            };
            if let Some(ch) = glyph {
                let mut buf = [0u8; 4];
                let text: gpui::SharedString = ch.encode_utf8(&mut buf).to_string().into();
                let run = gpui::TextRun {
                    len: text.len(),
                    font: text_style.font(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let size = text_style.font_size.to_pixels(window.rem_size());
                let line = window.text_system().shape_line(text, size, &[run], None);
                let x = mark.origin.x + (mark.width - line.width).max(px(0.)) / 2.;
                let _ = line.paint(point(x, mark.origin.y), line_height, window, cx);
                continue;
            }
            match mark.kind {
                WsKind::Space => {
                    let size = px(2.);
                    let x = mark.origin.x + (mark.width - size) / 2.;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(x, mark.origin.y + middle - size / 2.),
                            gpui::size(size, size),
                        ),
                        color,
                    ));
                }
                WsKind::Control | WsKind::Suspicious => {
                    // A box the width of the character. Control characters
                    // usually shape to nothing, so the box is given a floor
                    // wide enough to notice.
                    let w = mark.width.max(px(3.));
                    let h = line_height * 0.7;
                    let fill = if mark.kind == WsKind::Control {
                        cx.theme().danger.opacity(0.35)
                    } else {
                        cx.theme().warning.opacity(0.35)
                    };
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(mark.origin.x, mark.origin.y + (line_height - h) / 2.),
                            gpui::size(w, h),
                        ),
                        fill,
                    ));
                }
                WsKind::Tab => {
                    // A horizontal bar with a short head, drawn as two quads so
                    // no glyph shaping is needed per tab.
                    let thickness = px(1.);
                    let inset = px(2.);
                    let len = (mark.width - inset * 2.).max(px(3.));
                    let y = mark.origin.y + middle;
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(mark.origin.x + inset, y),
                            gpui::size(len, thickness),
                        ),
                        color,
                    ));
                    let head = px(3.).min(len);
                    window.paint_quad(gpui::fill(
                        Bounds::new(
                            point(mark.origin.x + inset + len - head, y - head + thickness),
                            gpui::size(thickness, head * 2. - thickness),
                        ),
                        color,
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mark, WsKind, extra_marks_for_line, marks_for_line};
    use crate::input::mode::{RenderWhitespace, UnicodeHighlight};

    #[track_caller]
    fn extra(line: &str, control: bool, unicode: UnicodeHighlight) -> Vec<WsKind> {
        extra_marks_for_line(line, control, unicode, "")
            .into_iter()
            .map(|m| m.kind)
            .collect()
    }

    /// Off by default: an ordinary line of code gets nothing.
    #[test]
    fn nothing_is_marked_by_default() {
        assert!(extra("let x = 1;", false, UnicodeHighlight::None).is_empty());
        // Even with everything on, plain ASCII is plain ASCII.
        assert!(extra("let x = 1;", true, UnicodeHighlight::All).is_empty());
    }

    /// The ideographic space is the one that costs the most time.
    #[test]
    fn an_ideographic_space_is_marked() {
        let line = "let\u{3000}x = 1;";
        assert_eq!(
            extra(line, false, UnicodeHighlight::Ambiguous),
            vec![WsKind::Suspicious]
        );
        // It is a space as well, so "invisible" catches it too -- a reader
        // who picks either one expects to see it.
        assert_eq!(
            extra(line, false, UnicodeHighlight::Invisible),
            vec![WsKind::Suspicious]
        );
    }

    /// An ordinary space and tab belong to `render_whitespace`, not here --
    /// marking them twice would put two boxes on one character.
    #[test]
    fn ordinary_whitespace_is_left_alone() {
        assert!(extra("a \tb", true, UnicodeHighlight::All).is_empty());
    }

    #[test]
    fn a_cyrillic_lookalike_is_ambiguous_not_invisible() {
        let line = "let \u{0440}ath = 1;"; // Cyrillic `р`
        assert_eq!(
            extra(line, false, UnicodeHighlight::Ambiguous),
            vec![WsKind::Suspicious]
        );
        assert!(extra(line, false, UnicodeHighlight::Invisible).is_empty());
    }

    #[test]
    fn a_zero_width_space_is_invisible_not_ambiguous() {
        let line = "a\u{200b}b";
        assert_eq!(
            extra(line, false, UnicodeHighlight::Invisible),
            vec![WsKind::Suspicious]
        );
        assert!(extra(line, false, UnicodeHighlight::Ambiguous).is_empty());
    }

    /// Control characters are their own kind, and win over "invisible".
    #[test]
    fn a_control_character_is_its_own_mark() {
        let line = "a\u{7}b";
        assert_eq!(extra(line, true, UnicodeHighlight::None), vec![WsKind::Control]);
        assert!(extra(line, false, UnicodeHighlight::All).is_empty(), "off means off");

        // A bidi override is both a control character and a marking risk.
        let bidi = "a\u{202e}b";
        assert_eq!(extra(bidi, true, UnicodeHighlight::All), vec![WsKind::Control]);
    }

    /// `\r` belongs to the line ending; `\t` has its own mark.
    #[test]
    fn the_line_ending_is_not_a_control_character() {
        assert!(extra("let x = 1;\r", true, UnicodeHighlight::None).is_empty());
        assert!(extra("\tlet x = 1;", true, UnicodeHighlight::None).is_empty());
    }

    /// The reader's allow list wins over everything.
    #[test]
    fn an_allowed_character_is_never_marked() {
        let line = "let\u{3000}x = \u{a0}1;";
        assert_eq!(extra(line, true, UnicodeHighlight::All).len(), 2);
        let marks = extra_marks_for_line(line, true, UnicodeHighlight::All, "\u{3000}");
        assert_eq!(marks.len(), 1, "許した字は数えない");
    }

    /// Offsets are byte positions, so a multi-byte character before the mark
    /// does not shift it.
    #[test]
    fn offsets_are_bytes() {
        let line = "あい\u{3000}う";
        let marks = extra_marks_for_line(line, false, UnicodeHighlight::Ambiguous, "");
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].offset, 6, "あい で 6 バイト");
    }

    #[track_caller]
    fn offsets(line: &str, mode: RenderWhitespace) -> Vec<usize> {
        marks_for_line(line, mode, None)
            .into_iter()
            .map(|m| m.offset)
            .collect()
    }

    #[test]
    fn none_draws_nothing() {
        assert!(offsets("  a  b  ", RenderWhitespace::None).is_empty());
    }

    #[test]
    fn all_draws_every_space_and_tab() {
        assert_eq!(offsets("  a b", RenderWhitespace::All), vec![0, 1, 3]);
        assert_eq!(
            marks_for_line("\ta", RenderWhitespace::All, None),
            vec![Mark {
                offset: 0,
                kind: WsKind::Tab
            }]
        );
    }

    #[test]
    fn trailing_draws_only_the_run_after_the_last_word() {
        assert_eq!(offsets("  a b  ", RenderWhitespace::Trailing), vec![5, 6]);
        // A line that is nothing but whitespace is entirely trailing.
        assert_eq!(offsets("   ", RenderWhitespace::Trailing), vec![0, 1, 2]);
        assert!(offsets("ab", RenderWhitespace::Trailing).is_empty());
    }

    #[test]
    fn boundary_skips_a_single_space_between_words() {
        // "  a b  c" -> leading 2, the single space at 3 is skipped, the
        // trailing-ish double space at 5,6 is drawn.
        assert_eq!(offsets("  a b  c", RenderWhitespace::Boundary), vec![0, 1, 5, 6]);
        // Tabs are always drawn, even between words.
        assert_eq!(offsets("a\tb", RenderWhitespace::Boundary), vec![1]);
        // Trailing single space is drawn.
        assert_eq!(offsets("a b ", RenderWhitespace::Boundary), vec![3]);
    }

    #[test]
    fn selection_limits_the_marks_to_the_range() {
        let marks = marks_for_line("a b c d", RenderWhitespace::Selection, Some(1..4));
        assert_eq!(marks.iter().map(|m| m.offset).collect::<Vec<_>>(), vec![1, 3]);
        // No selection on this line -> nothing.
        assert!(marks_for_line("a b", RenderWhitespace::Selection, None).is_empty());
    }

    #[test]
    fn carriage_return_is_not_marked() {
        // `\r` belongs to the line ending; marking it would put a dot past the
        // end of every line of a CRLF file.
        assert_eq!(offsets("a \r", RenderWhitespace::All), vec![1]);
        assert_eq!(offsets("a \r", RenderWhitespace::Trailing), vec![1]);
    }
}
