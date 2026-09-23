//! A text input field that allows the user to enter text.
//!
//! Based on the `Input` example from the `gpui` crate.
//! https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/input.rs
use anyhow::Result;
use gpui::{
    Action, App, AppContext, Bounds, ClipboardItem, Context, Entity, EntityInputHandler,
    EventEmitter, FocusHandle, Focusable, InteractiveElement as _, IntoElement, KeyBinding,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _,
    Pixels, Point, Render, ScrollHandle, ScrollWheelEvent, SharedString, Styled as _, Subscription,
    Task, UTF16Selection, Window, actions, div, point, prelude::FluentBuilder as _, px,
};
use ropey::{Rope, RopeSlice};
use serde::Deserialize;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;
use sum_tree::Bias;

use super::{
    blink_cursor::BlinkCursor,
    brackets::{self, AutoClose, AutoCloseEdit, BracketGuides, MatchBrackets},
    caret::{self, CaretAnimation, CursorBlinking, CursorStyle, SurroundingLinesStyle},
    comment::{self, EnterComment},
    tags,
    change::Change,
    element::TextElement,
    mask_pattern::MaskPattern,
    mode::{
        FoldingControls, InlayModifier, InputMode, LineHighlight, LineNumbers,
        RenderWhitespace, WrapAt,
    },
    number_input,
    text_wrapper::TextWrapper,
};
use crate::Size;
use crate::actions::{SelectDown, SelectLeft, SelectRight, SelectUp};
use crate::input::movement::MoveDirection;
use crate::input::{
    HoverDefinition, Lsp, Position,
    element::RIGHT_MARGIN,
    popovers::{ContextMenu, DiagnosticPopover, HoverPopover, MouseContextMenu},
    search::{self, SearchPanel},
    text_wrapper::LineLayout,
};
use crate::input::{InlineCompletion, RopeExt as _, Selection};
use crate::{Root, history::History};
use crate::{highlighter::DiagnosticSet, input::text_wrapper::LineItem};

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = input, no_json)]
pub struct Enter {
    /// Is confirm with secondary.
    pub secondary: bool,
}

actions!(
    input,
    [
        Backspace,
        Delete,
        DeleteToBeginningOfLine,
        DeleteToEndOfLine,
        DeleteToPreviousWordStart,
        DeleteToNextWordEnd,
        Indent,
        Outdent,
        IndentInline,
        OutdentInline,
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        MoveHome,
        MoveEnd,
        MovePageUp,
        MovePageDown,
        SelectAll,
        SelectToStartOfLine,
        SelectToEndOfLine,
        SelectToStart,
        SelectToEnd,
        SelectToPreviousWordStart,
        SelectToNextWordEnd,
        ShowCharacterPalette,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        MoveToStartOfLine,
        MoveToEndOfLine,
        MoveToStart,
        MoveToEnd,
        MoveToPreviousWord,
        MoveToNextWord,
        Escape,
        ToggleCodeActions,
        Search,
        GoToDefinition,
        ExpandSelection,
        ShrinkSelection,
        ToggleFold,
        FoldAll,
        UnfoldAll,
        ToggleComment,
        ToggleBlockComment,
    ]
);

#[derive(Clone)]
pub enum InputEvent {
    Change,
    PressEnter { secondary: bool },
    Focus,
    Blur,
    /// ⌘ + wheel asked for a bigger or smaller font (#252).
    ///
    /// Positive steps mean bigger. **`InputState` does not own the font
    /// size** -- the host does, and in a multi-pane editor it is the one that
    /// knows whether "bigger" means this pane or all of them.
    ZoomDelta { steps: i32 },
    /// Text was pasted from the clipboard, covering `range` (byte offsets).
    ///
    /// [`InputEvent::Change`] alone cannot tell a paste from typing, and a
    /// listener that wants to act on the pasted text needs to know where it
    /// landed.
    Pasted { range: Range<usize> },
    /// A "go to definition" landed somewhere this input cannot take you.
    ///
    /// Only emitted when [`InputState::definitions_open_externally`] is set.
    /// `go_to_definition` otherwise moves the caret inside this buffer --
    /// which is wrong for a target in **another file**, and is why a host
    /// that owns more than one editor has to be the one to decide.
    OpenLocation {
        /// The target, as the server gave it (`file://…`, `https://…`).
        uri: String,
        /// 0-based, as LSP counts.
        line: u32,
        /// 0-based, in UTF-16 code units, as LSP counts.
        character: u32,
    },
    /// A link the host handed over with [`InputState::set_link_ranges`] was
    /// clicked, covering `range` (byte offsets).
    LinkClicked { range: Range<usize> },
}

pub(super) const CONTEXT: &str = "Input";

pub(crate) fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, Some(CONTEXT)),
        KeyBinding::new("delete", Delete, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-backspace", DeleteToBeginningOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-delete", DeleteToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-backspace", DeleteToPreviousWordStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-delete", DeleteToNextWordEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-delete", DeleteToNextWordEnd, Some(CONTEXT)),
        KeyBinding::new("enter", Enter { secondary: false }, Some(CONTEXT)),
        KeyBinding::new("secondary-enter", Enter { secondary: true }, Some(CONTEXT)),
        KeyBinding::new("escape", Escape, Some(CONTEXT)),
        KeyBinding::new("up", MoveUp, Some(CONTEXT)),
        KeyBinding::new("down", MoveDown, Some(CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(CONTEXT)),
        KeyBinding::new("pageup", MovePageUp, Some(CONTEXT)),
        KeyBinding::new("pagedown", MovePageDown, Some(CONTEXT)),
        KeyBinding::new("tab", IndentInline, Some(CONTEXT)),
        KeyBinding::new("shift-tab", OutdentInline, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-]", Indent, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-]", Indent, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-[", Outdent, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-[", Outdent, Some(CONTEXT)),
        KeyBinding::new("shift-left", SelectLeft, Some(CONTEXT)),
        KeyBinding::new("shift-right", SelectRight, Some(CONTEXT)),
        KeyBinding::new("shift-up", SelectUp, Some(CONTEXT)),
        KeyBinding::new("shift-down", SelectDown, Some(CONTEXT)),
        KeyBinding::new("home", MoveHome, Some(CONTEXT)),
        KeyBinding::new("end", MoveEnd, Some(CONTEXT)),
        KeyBinding::new("shift-home", SelectToStartOfLine, Some(CONTEXT)),
        KeyBinding::new("shift-end", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift-a", SelectToStartOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-shift-e", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("shift-cmd-left", SelectToStartOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("shift-cmd-right", SelectToEndOfLine, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-left", SelectToPreviousWordStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-right", SelectToNextWordEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-a", SelectAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-a", SelectAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-c", Copy, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-c", Copy, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-x", Cut, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-x", Cut, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-v", Paste, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-v", Paste, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-a", MoveHome, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-left", MoveHome, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("ctrl-e", MoveEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-right", MoveEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-z", Undo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-z", Redo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-up", MoveToStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-down", MoveToEnd, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-left", MoveToPreviousWord, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("alt-right", MoveToNextWord, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-left", MoveToPreviousWord, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-right", MoveToNextWord, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-up", SelectToStart, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-down", SelectToEnd, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-z", Undo, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-y", Redo, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        // `cmd-[` is Outdent, so folding takes the alt variants.
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-[", ToggleFold, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-[", ToggleFold, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-shift-[", FoldAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-shift-[", FoldAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-shift-]", UnfoldAll, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-shift-]", UnfoldAll, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-/", ToggleComment, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-/", ToggleComment, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-/", ToggleBlockComment, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-/", ToggleBlockComment, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-cmd-right", ExpandSelection, Some(CONTEXT)),
        KeyBinding::new("ctrl-shift-cmd-left", ShrinkSelection, Some(CONTEXT)),
        KeyBinding::new("cmd-.", ToggleCodeActions, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-.", ToggleCodeActions, Some(CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-f", Search, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-f", Search, Some(CONTEXT)),
    ]);

    search::init(cx);
    number_input::init(cx);
}

/// A caret slide in flight.
///
/// `from` and `to` are in text coordinates (no scroll offset), so a slide
/// survives scrolling without turning into a drift.
#[derive(Clone, Copy)]
pub(super) struct CaretSlide {
    from: Bounds<Pixels>,
    to: Bounds<Pixels>,
    start: Instant,
}

/// How long a smooth jump takes (#252).
const SCROLL_SLIDE: std::time::Duration = std::time::Duration::from_millis(120);

#[derive(Clone)]
pub(super) struct LastLayout {
    /// The visible range (no wrap) of lines in the viewport, the value is row (0-based) index.
    pub(super) visible_range: Range<usize>,
    /// The first visible line top position in scroll viewport.
    pub(super) visible_top: Pixels,
    /// The range of byte offset of the visible lines.
    pub(super) visible_range_offset: Range<usize>,
    /// The last layout lines (Only have visible lines).
    pub(super) lines: Rc<Vec<LineLayout>>,
    /// The line_height of text layout, this will change will InputElement painted.
    pub(super) line_height: Pixels,
    /// The wrap width of text layout, this will change will InputElement painted.
    pub(super) wrap_width: Option<Pixels>,
    /// The line number area width of text layout, if not line number, this will be 0px.
    pub(super) line_number_width: Pixels,
    /// The cursor position (top, left) in pixels.
    pub(super) cursor_bounds: Option<Bounds<Pixels>>,
    /// Rows inserted below each visible row, in the same order as `lines`.
    ///
    /// The painting walks step through `lines` (shaped, wrapped) and never see
    /// a `LineItem`, so this is where they ask. Empty while nothing is
    /// inserted, which is the normal case.
    pub(super) extra_rows: Rc<Vec<usize>>,
}

impl LastLayout {
    /// Get the line layout for the given row (0-based).
    ///
    /// 0 is the viewport first visible line.
    ///
    /// Returns None if the row is out of range.
    pub(crate) fn line(&self, row: usize) -> Option<&LineLayout> {
        if row < self.visible_range.start || row >= self.visible_range.end {
            return None;
        }

        self.lines.get(row.saturating_sub(self.visible_range.start))
    }

    /// The height of what is inserted **below** `row`, 0 for a row with nothing
    /// under it (and for a row outside the viewport).
    pub(crate) fn extra_height(&self, row: usize) -> Pixels {
        if row < self.visible_range.start {
            return px(0.);
        }
        let rows = self
            .extra_rows
            .get(row - self.visible_range.start)
            .copied()
            .unwrap_or(0);
        rows as f32 * self.line_height
    }
}

/// InputState to keep editing state of the [`super::Input`].
pub struct InputState {
    pub(super) focus_handle: FocusHandle,
    pub(super) mode: InputMode,
    pub(super) text: Rope,
    pub(super) text_wrapper: TextWrapper,
    pub(super) history: History<Change>,
    pub(super) blink_cursor: Entity<BlinkCursor>,
    /// The caret slide in flight, if any (`editor.cursorSmoothCaretAnimation`).
    ///
    /// Lives here rather than in the element because the element is rebuilt
    /// every frame. The rectangles are in **text coordinates** -- no scroll
    /// offset -- so scrolling never looks like the caret moved.
    pub(super) caret_slide: Option<CaretSlide>,
    /// Whether the last caret move was asked for (a key or a click) rather
    /// than the result of an edit pushing it along. Read by
    /// [`CaretAnimation::Explicit`].
    pub(super) caret_moved_explicitly: bool,
    pub(super) loading: bool,
    /// Range in UTF-8 length for the selected text.
    ///
    /// - "Hello 世界💝" = 16
    /// - "💝" = 4
    pub(super) selected_range: Selection,
    pub(super) search_panel: Option<Entity<SearchPanel>>,
    pub(super) searchable: bool,
    /// Range for save the selected word, use to keep word range when drag move.
    pub(super) selected_word_range: Option<Selection>,
    pub(super) selection_reversed: bool,
    /// The marked range is the temporary insert text on IME typing.
    pub(super) ime_marked_range: Option<Selection>,
    pub(super) last_layout: Option<LastLayout>,
    pub(super) last_cursor: Option<usize>,
    /// The input container bounds
    pub(super) input_bounds: Bounds<Pixels>,
    /// The text bounds
    pub(super) last_bounds: Option<Bounds<Pixels>>,
    pub(super) last_selected_range: Option<Selection>,
    pub(super) selecting: bool,
    pub(super) size: Size,
    pub(super) disabled: bool,
    pub(super) masked: bool,
    pub(super) clean_on_escape: bool,
    pub(super) soft_wrap: bool,
    /// Rows that head a folded region, sorted.
    ///
    /// Only the **headers** are stored; which rows they hide is recomputed from
    /// the text every time, so a stale header can only fold the wrong block --
    /// never hide bytes with no way to reveal them.
    pub(super) folded_rows: Vec<usize>,
    /// Fold ranges handed in from outside (a language server), instead of the
    /// indentation rule. `None` = read them off the indentation.
    pub(super) supplied_folds: Option<Vec<crate::input::FoldRange>>,
    /// Inlay hints to draw at the end of a row, keyed by row.
    ///
    /// Kept out of [`InputMode`] on purpose: the mode is already a large
    /// variant, and this grows with the file rather than with the settings.
    /// Sorted by row so the paint loop can look a row up quickly.
    pub(super) inlay_rows: Vec<crate::input::InlayRow>,
    /// Diagnostics to draw at the end of a row, keyed by row.
    ///
    /// Built from the same list the squiggles are built from, so the two
    /// cannot disagree. Sorted by row for the paint loop.
    pub(super) inline_diagnostics: Vec<crate::input::InlineDiagnostic>,
    /// Change marks beside the line numbers (dopamine #280).
    pub(super) gutter_marks: Vec<crate::input::GutterMark>,
    /// Whole-row and byte-range backgrounds (dopamine #244).
    pub(super) row_backgrounds: Vec<(usize, gpui::Hsla)>,
    pub(super) range_backgrounds: Vec<(std::ops::Range<usize>, gpui::Hsla)>,
    /// The row the pointer is over in the fold gutter, for
    /// [`FoldingControls::MouseOver`].
    pub(super) hovered_gutter_row: Option<usize>,
    pub(super) pattern: Option<regex::Regex>,
    pub(super) validate: Option<Box<dyn Fn(&str, &mut Context<Self>) -> bool + 'static>>,
    pub(crate) scroll_handle: ScrollHandle,
    /// The deferred scroll offset to apply on next layout.
    pub(crate) deferred_scroll_offset: Option<Point<Pixels>>,
    /// The size of the scrollable content.
    pub(crate) scroll_size: gpui::Size<Pixels>,

    /// The mask pattern for formatting the input text
    pub(crate) mask_pattern: MaskPattern,
    pub(super) placeholder: SharedString,

    /// Popover
    diagnostic_popover: Option<Entity<DiagnosticPopover>>,
    /// Completion/CodeAction context menu
    pub(super) context_menu: Option<ContextMenu>,
    pub(super) mouse_context_menu: Entity<MouseContextMenu>,
    /// A flag to indicate if we are currently inserting a completion item.
    pub(super) completion_inserting: bool,
    pub(super) hover_popover: Option<Entity<HoverPopover>>,
    /// The parameter hints for the call the caret is in (#246).
    pub(super) signature_popover: Option<Entity<crate::input::popovers::SignaturePopover>>,
    /// Waiting out `quickSuggestionsDelay` before opening the menu (#246).
    pub(super) _suggest_delay_task: Task<Result<()>>,
    /// Whether the pointer is over the hover popover.
    ///
    /// The popover occludes, so the editor's own `mouse_move` stops firing the
    /// moment the pointer lands on it -- this cannot be observed from here.
    /// The popover reports it instead.
    pub(super) hover_popover_hovered: bool,
    /// Whether a hide is already on the clock.
    pub(super) hover_hiding: bool,
    /// The LSP definitions locations for "Go to Definition" feature.
    pub(super) hover_definition: HoverDefinition,
    /// Where the selection was before each `ExpandSelection` (#253).
    pub(super) expand_stack: Vec<Range<usize>>,
    /// Ranges the host said may be clicked (#256). See `links.rs`.
    pub(super) link_ranges: Vec<Range<usize>>,
    /// What a language server said each span of text is (#388).
    ///
    /// Sorted by `range.start` and non-overlapping; `semantic_spans_in` leans
    /// on that to find the visible slice by binary search.
    pub(super) semantic_spans: Vec<super::SemanticSpan>,
    /// The one the pointer is on **with the modifier held**, if any.
    pub(super) link_hover: Option<Range<usize>>,

    pub lsp: Lsp,

    /// A flag to indicate if we have a pending update to the text.
    ///
    /// If true, will call some update (for example LSP, Syntax Highlight) before render.
    _pending_update: bool,
    /// The caret moved, so the occurrence highlights are stale (#253).
    ///
    /// Asking needs a `&mut Window`, and `move_to` has none -- so the ask is
    /// left for the next frame, which is also a free debounce for a drag.
    pub(super) pending_highlight: bool,
    /// A scroll slide in flight: (from, to, when it started) (#252).
    pub(super) scroll_slide: Option<(Point<Pixels>, Point<Pixels>, std::time::Instant)>,
    /// Dropping this stops the slide.
    pub(super) scroll_slide_task: Option<gpui::Task<()>>,
    /// Indent the editor itself put on a line, still untouched (#248).
    ///
    /// Set when Enter writes an indent and nothing else; taken back when the
    /// caret leaves that line, if `trim_auto_whitespace` is on.
    pub(super) auto_ws: Option<Range<usize>>,
    /// A flag to indicate if we should ignore the next completion event.
    pub(super) silent_replace_text: bool,

    /// To remember the horizontal column (x-coordinate) of the cursor position for keep column for move up/down.
    ///
    /// The first element is the x-coordinate (Pixels), preferred to use this.
    /// The second element is the column (usize), fallback to use this.
    pub(super) preferred_column: Option<(Pixels, usize)>,
    _subscriptions: Vec<Subscription>,

    pub(super) _context_menu_task: Task<Result<()>>,
    pub(super) inline_completion: InlineCompletion,
}

impl EventEmitter<InputEvent> for InputState {}

impl InputState {
    /// Create a Input state with default [`InputMode::SingleLine`] mode.
    ///
    /// See also: [`Self::multi_line`], [`Self::auto_grow`] to set other mode.
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle().tab_stop(true);
        let blink_cursor = cx.new(|_| BlinkCursor::new());
        let history = History::new().group_interval(std::time::Duration::from_secs(1));

        let _subscriptions = vec![
            // Observe the blink cursor to repaint the view when it changes.
            cx.observe(&blink_cursor, |_, _, cx| cx.notify()),
            // Blink the cursor when the window is active, pause when it's not.
            cx.observe_window_activation(window, |input, window, cx| {
                if window.is_window_active() {
                    let focus_handle = input.focus_handle.clone();
                    if focus_handle.is_focused(window) {
                        let blinking = input.mode.cursor_blinking();
                        input.blink_cursor.update(cx, |blink_cursor, cx| {
                            blink_cursor.start(blinking, cx);
                        });
                    }
                }
            }),
            cx.on_focus(&focus_handle, window, Self::on_focus),
            cx.on_blur(&focus_handle, window, Self::on_blur),
        ];

        let text_style = window.text_style();
        let mouse_context_menu = MouseContextMenu::new(cx.entity(), window, cx);

        Self {
            focus_handle: focus_handle.clone(),
            text: "".into(),
            text_wrapper: TextWrapper::new(text_style.font(), window.rem_size(), None),
            blink_cursor,
            caret_slide: None,
            caret_moved_explicitly: false,
            history,
            selected_range: Selection::default(),
            search_panel: None,
            searchable: false,
            selected_word_range: None,
            selection_reversed: false,
            ime_marked_range: None,
            input_bounds: Bounds::default(),
            selecting: false,
            disabled: false,
            masked: false,
            clean_on_escape: false,
            soft_wrap: true,
            folded_rows: Vec::new(),
            supplied_folds: None,
            inlay_rows: Vec::new(),
            inline_diagnostics: Vec::new(),
            gutter_marks: Vec::new(),
            row_backgrounds: Vec::new(),
            range_backgrounds: Vec::new(),
            hovered_gutter_row: None,
            loading: false,
            pattern: None,
            validate: None,
            mode: InputMode::default(),
            last_layout: None,
            last_bounds: None,
            last_selected_range: None,
            last_cursor: None,
            scroll_handle: ScrollHandle::new(),
            scroll_size: gpui::size(px(0.), px(0.)),
            deferred_scroll_offset: None,
            preferred_column: None,
            placeholder: SharedString::default(),
            mask_pattern: MaskPattern::default(),
            lsp: Lsp::default(),
            diagnostic_popover: None,
            context_menu: None,
            mouse_context_menu,
            completion_inserting: false,
            hover_popover: None,
            signature_popover: None,
            _suggest_delay_task: Task::ready(Ok(())),
            hover_popover_hovered: false,
            hover_hiding: false,
            hover_definition: HoverDefinition::default(),
            expand_stack: Vec::new(),
            link_ranges: Vec::new(),
            semantic_spans: Vec::new(),
            link_hover: None,
            silent_replace_text: false,
            size: Size::default(),
            _subscriptions,
            _context_menu_task: Task::ready(Ok(())),
            _pending_update: false,
            pending_highlight: false,
            scroll_slide: None,
            scroll_slide_task: None,
            auto_ws: None,
            inline_completion: InlineCompletion::default(),
        }
    }

    /// Set Input to use multi line mode.
    ///
    /// Default rows is 2.
    pub fn multi_line(mut self, multi_line: bool) -> Self {
        self.mode = self.mode.multi_line(multi_line);
        self
    }

    /// Set Input to use [`InputMode::AutoGrow`] mode with min, max rows limit.
    pub fn auto_grow(mut self, min_rows: usize, max_rows: usize) -> Self {
        self.mode = InputMode::auto_grow(min_rows, max_rows);
        self
    }

    /// Set Input to use [`InputMode::CodeEditor`] mode.
    ///
    /// Default options:
    ///
    /// - line_number: true
    /// - tab_size: 2
    /// - hard_tabs: false
    /// - height: 100%
    /// - multi_line: true
    /// - indent_guides: true
    ///
    /// If `highlighter` is None, will use the default highlighter.
    ///
    /// Code Editor aim for help used to simple code editing or display, not a full-featured code editor.
    ///
    /// ## Features
    ///
    /// - Syntax Highlighting
    /// - Auto Indent
    /// - Line Number
    /// - Large Text support, up to 50K lines.
    pub fn code_editor(mut self, language: impl Into<SharedString>) -> Self {
        let language: SharedString = language.into();
        self.mode = InputMode::code_editor(language);
        self.searchable = true;
        self
    }

    /// Set this input is searchable, default is false (Default true for Code Editor).
    pub fn searchable(mut self, searchable: bool) -> Self {
        debug_assert!(self.mode.is_multi_line());
        self.searchable = searchable;
        self
    }

    /// Set placeholder
    pub fn placeholder(mut self, placeholder: impl Into<SharedString>) -> Self {
        self.placeholder = placeholder.into();
        self
    }

    /// Set the line number gutter, only for [`InputMode::CodeEditor`] mode.
    ///
    /// Takes a [`LineNumbers`], or a `bool` for the on/off case.
    pub fn line_number(mut self, line_number: impl Into<LineNumbers>) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { line_number: l, .. } = &mut self.mode {
            *l = line_number.into();
        }
        self
    }

    /// Set the line number gutter, only for [`InputMode::CodeEditor`] mode.
    pub fn set_line_number(
        &mut self,
        line_number: impl Into<LineNumbers>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { line_number: l, .. } = &mut self.mode {
            *l = line_number.into();
        }
        cx.notify();
    }

    /// Set the smallest width of the line number gutter in digits, only for
    /// [`InputMode::CodeEditor`] mode.
    ///
    /// The gutter never shrinks below what the last line number needs, so this
    /// only widens it.
    pub fn min_line_number_digits(mut self, digits: usize) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            min_line_number_digits: d,
            ..
        } = &mut self.mode
        {
            *d = digits;
        }
        self
    }

    /// Number the empty line a trailing newline creates, only for
    /// [`InputMode::CodeEditor`] mode.
    ///
    /// Turning this off leaves that row blank in the gutter. The row itself
    /// stays, so the caret can still be placed on it.
    pub fn render_final_newline(mut self, render: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            render_final_newline: r,
            ..
        } = &mut self.mode
        {
            *r = render;
        }
        self
    }

    /// Draw a band behind a folded row, only for [`InputMode::CodeEditor`].
    pub fn fold_highlight(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { fold_highlight, .. } = &mut self.mode {
            *fold_highlight = on;
        }
        self
    }

    /// Cap how many regions can be folded at once, only for
    /// [`InputMode::CodeEditor`].
    pub fn max_fold_regions(mut self, max: u32) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            max_fold_regions, ..
        } = &mut self.mode
        {
            *max_fold_regions = max;
        }
        self
    }

    /// Unfold a folded row when it is clicked past the end of its text, only
    /// for [`InputMode::CodeEditor`].
    pub fn unfold_on_click_after_end_of_line(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            unfold_on_click_after_end_of_line,
            ..
        } = &mut self.mode
        {
            *unfold_on_click_after_end_of_line = on;
        }
        self
    }

    /// Show the hover popover at all, only for [`InputMode::CodeEditor`].
    pub fn hover(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { hover, .. } = &mut self.mode {
            *hover = on;
        }
        self
    }

    /// How long the pointer rests before the hover popover appears, in
    /// milliseconds. Only for [`InputMode::CodeEditor`].
    /// How ⌘F looks for what was typed into it (#241).
    ///
    /// Read when the panel opens, so a change lands on the next ⌘F.
    pub fn search_options(mut self, options: crate::input::SearchOptions) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { search_options, .. } = &mut self.mode {
            *search_options = options;
        }
        self
    }

    /// How the selection grows and what a double-click selects (#253).
    pub fn selection_expansion(
        mut self,
        double_click_block: bool,
        subwords: bool,
        whitespace: bool,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            double_click_selects_block,
            smart_select_subwords,
            smart_select_whitespace,
            ..
        } = &mut self.mode
        {
            *double_click_selects_block = double_click_block;
            *smart_select_subwords = subwords;
            *smart_select_whitespace = whitespace;
        }
        self
    }

    /// How far past the text the view may scroll, and how much context is
    /// kept beside the caret (#252).
    pub fn scroll_bounds(
        mut self,
        beyond_last_line: super::mode::ScrollBeyondLastLine,
        beyond_last_column: u16,
        horizontal_margin: u16,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            scroll_beyond_last_line,
            scroll_beyond_last_column,
            horizontal_scroll_margin,
            ..
        } = &mut self.mode
        {
            *scroll_beyond_last_line = beyond_last_line;
            *scroll_beyond_last_column = beyond_last_column;
            *horizontal_scroll_margin = horizontal_margin;
        }
        self
    }

    /// When the completion menu opens by itself, and how it is taken (#246).
    pub fn suggest_behaviour(mut self, b: super::mode::SuggestBehaviour) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            quick_suggestions,
            quick_suggestions_delay,
            accept_suggestion_on_enter,
            accept_suggestion_on_commit_character,
            tab_completion,
            suggest_insert_mode,
            suggest_selection,
            ..
        } = &mut self.mode
        {
            *quick_suggestions = b.quick;
            *quick_suggestions_delay = b.delay;
            *accept_suggestion_on_enter = b.on_enter;
            *accept_suggestion_on_commit_character = b.on_commit_character;
            *tab_completion = b.tab;
            *suggest_insert_mode = b.insert_mode;
            *suggest_selection = b.selection;
        }
        self
    }

    /// How the completion menu looks (#246).
    pub fn suggest_style(mut self, style: super::mode::SuggestStyle) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            suggest_font_size,
            suggest_line_height,
            suggest_kind_display,
            suggest_show_inline_details,
            suggest_detail_alignment,
            suggest_show_status_bar,
            suggest_preview,
            suggest_scrollbar,
            ..
        } = &mut self.mode
        {
            *suggest_font_size = style.font_size;
            *suggest_line_height = style.line_height;
            *suggest_kind_display = style.kind;
            *suggest_show_inline_details = style.inline_details;
            *suggest_detail_alignment = style.alignment;
            *suggest_show_status_bar = style.status_bar;
            *suggest_preview = style.preview;
            *suggest_scrollbar = style.scrollbar;
        }
        self
    }

    /// The APCA contrast floor for text on a highlight background
    /// (dopamine #276 / ADR-0109).
    ///
    /// **`0` keeps the old behaviour** -- text on a highlight is slammed to
    /// black or white, which reads fine but loses the syntax colour. Anything
    /// above keeps the colour and only moves its lightness far enough to clear
    /// the floor (Zed's `minimum_contrast_for_highlights`, whose default is 45).
    pub fn minimum_contrast(mut self, lc: f32) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { minimum_contrast, .. } = &mut self.mode {
            *minimum_contrast = lc.max(0.);
        }
        self
    }

    /// The parameter hints shown inside a call (#246).
    pub fn parameter_hints(mut self, on: bool, cycle: bool, after_edits: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            parameter_hints,
            parameter_hints_cycle,
            signature_help_after_edits,
            ..
        } = &mut self.mode
        {
            *parameter_hints = on;
            *parameter_hints_cycle = cycle;
            *signature_help_after_edits = after_edits;
        }
        self
    }

    /// How the wheel and a jump behave (#252).
    ///
    /// Sensitivities are percentages; 100 leaves the wheel alone.
    pub fn scroll_motion(
        mut self,
        smooth: bool,
        predominant_axis: bool,
        sensitivity: u16,
        fast_sensitivity: u16,
        wheel_zoom: bool,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            smooth_scrolling,
            scroll_predominant_axis,
            scroll_sensitivity,
            fast_scroll_sensitivity,
            mouse_wheel_zoom,
            ..
        } = &mut self.mode
        {
            *smooth_scrolling = smooth;
            *scroll_predominant_axis = predominant_axis;
            *scroll_sensitivity = sensitivity;
            *fast_scroll_sensitivity = fast_sensitivity;
            *mouse_wheel_zoom = wheel_zoom;
        }
        self
    }

    /// How the scrollbar looks (#252). `size_px` of 0 keeps the built-in width.
    pub fn scrollbar_style(
        mut self,
        show: Option<crate::scroll::ScrollbarShow>,
        vertical: bool,
        horizontal: bool,
        size_px: u16,
        border: bool,
        by_page: bool,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            scrollbar_show,
            scrollbar_vertical,
            scrollbar_horizontal,
            scrollbar_size,
            scrollbar_border,
            scrollbar_scroll_by_page,
            ..
        } = &mut self.mode
        {
            *scrollbar_show = show;
            *scrollbar_vertical = vertical;
            *scrollbar_horizontal = horizontal;
            *scrollbar_size = size_px;
            *scrollbar_border = border;
            *scrollbar_scroll_by_page = by_page;
        }
        self
    }

    /// Which marks the scrollbar track carries (#252).
    pub fn scrollbar_marks(mut self, marks: super::ScrollbarMarks) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { scrollbar_marks, .. } = &mut self.mode {
            *scrollbar_marks = marks;
        }
        self
    }

    /// The headers pinned above the text (#252).
    pub fn sticky_scroll(
        mut self,
        on: bool,
        max_lines: u16,
        model: super::mode::StickyModel,
        with_editor: bool,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            sticky_scroll,
            sticky_scroll_max_lines,
            sticky_scroll_model,
            sticky_scroll_with_editor,
            ..
        } = &mut self.mode
        {
            *sticky_scroll = on;
            *sticky_scroll_max_lines = max_lines;
            *sticky_scroll_model = model;
            *sticky_scroll_with_editor = with_editor;
        }
        self
    }

    /// How indent guides look (#248).
    ///
    /// `width` and `active_width` are in px; the caller is expected to clamp
    /// them to something sane (1--10 here).
    pub fn indent_guide_style(
        mut self,
        active: bool,
        width: f32,
        active_width: f32,
        coloring: super::mode::GuideColoring,
        background: super::mode::GuideBackground,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            indent_guide_active,
            indent_guide_width,
            indent_guide_active_width,
            indent_guide_coloring,
            indent_guide_background,
            ..
        } = &mut self.mode
        {
            *indent_guide_active = active;
            *indent_guide_width = width;
            *indent_guide_active_width = active_width;
            *indent_guide_coloring = coloring;
            *indent_guide_background = background;
        }
        self
    }

    /// What a new line inherits, and what a paste is re-indented to (#248).
    pub fn auto_indent(
        mut self,
        mode: super::mode::AutoIndent,
        on_paste: bool,
        on_paste_in_string: bool,
        trim_auto: bool,
        trim_on_delete: bool,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            auto_indent,
            auto_indent_on_paste,
            auto_indent_on_paste_in_string,
            trim_auto_whitespace,
            trim_whitespace_on_delete,
            ..
        } = &mut self.mode
        {
            *auto_indent = mode;
            *auto_indent_on_paste = on_paste;
            *auto_indent_on_paste_in_string = on_paste_in_string;
            *trim_auto_whitespace = trim_auto;
            *trim_whitespace_on_delete = trim_on_delete;
        }
        self
    }

    /// How the caret and backspace treat runs of leading spaces (#248).
    pub fn tab_stops(mut self, use_tab_stops: bool, sticky: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            use_tab_stops: u,
            sticky_tab_stops: s,
            ..
        } = &mut self.mode
        {
            *u = use_tab_stops;
            *s = sticky;
        }
        self
    }

    /// Whether a Markdown list carries onto the next line, and whether Tab
    /// indents the item (#248).
    pub fn lists(mut self, on_newline: bool, indent_on_tab: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            list_on_newline,
            indent_list_on_tab,
            ..
        } = &mut self.mode
        {
            *list_on_newline = on_newline;
            *indent_list_on_tab = indent_on_tab;
        }
        self
    }

    /// The characters whitespace marks are drawn with (#248).
    ///
    /// `None` on either keeps the built-in quad for that kind.
    pub fn whitespace_map(mut self, space: Option<char>, tab: Option<char>) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { whitespace_map, .. } = &mut self.mode {
            *whitespace_map = super::mode::WhitespaceMap { space, tab };
        }
        self
    }

    /// Which characters never join a word (#253, VS Code's `wordSeparators`).
    pub fn word_separators(mut self, separators: impl Into<std::rc::Rc<str>>) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            word_separators, ..
        } = &mut self.mode
        {
            *word_separators = separators.into();
        }
        self
    }

    /// Draw a box on control characters (#255).
    pub fn render_control_characters(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            render_control_characters,
            ..
        } = &mut self.mode
        {
            *render_control_characters = on;
        }
        self
    }

    /// Draw a box on invisible or ASCII-lookalike characters (#255).
    ///
    /// `allowed` is the reader's "never mark these" list, as a plain string.
    pub fn unicode_highlight(
        mut self,
        highlight: crate::input::UnicodeHighlight,
        allowed: impl Into<std::rc::Rc<str>>,
    ) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            unicode_highlight,
            unicode_allowed,
            ..
        } = &mut self.mode
        {
            *unicode_highlight = highlight;
            *unicode_allowed = allowed.into();
        }
        self
    }

    /// How a diagnostic's tags change the text it covers (#255).
    ///
    /// `fade` is `None` to leave "unnecessary" code at full strength.
    pub fn diagnostic_tag_style(mut self, fade: Option<f32>, strike: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            show_unused,
            unused_fade,
            show_deprecated,
            ..
        } = &mut self.mode
        {
            *show_unused = fade.is_some();
            *unused_fade = fade.unwrap_or(0.).clamp(0., 1.);
            *show_deprecated = strike;
        }
        self
    }

    /// Report where a definition landed instead of going there (#256).
    ///
    /// A host that owns more than one editor has to decide: the target may
    /// be in another file, or be an `https://` URL it would rather open in
    /// its own browser than hand to the operating system. Listen for
    /// [`InputEvent::OpenLocation`].
    pub fn definitions_open_externally(mut self, externally: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            definitions_open_externally,
            ..
        } = &mut self.mode
        {
            *definitions_open_externally = externally;
        }
        self
    }

    /// How ⌘F behaves once something has been found (#241).
    pub fn search_behavior(mut self, behavior: crate::input::SearchBehavior) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            search_behavior, ..
        } = &mut self.mode
        {
            *search_behavior = behavior;
        }
        self
    }

    /// Whether ⌘F starts with the selected text in the search field (#241).
    pub fn search_seed_from_selection(mut self, seed: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            search_seed_from_selection,
            ..
        } = &mut self.mode
        {
            *search_seed_from_selection = seed;
        }
        self
    }

    /// Set how long the pointer rests before the hover popover appears.
    pub fn hover_delay(mut self, ms: u16) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { hover_delay, .. } = &mut self.mode {
            *hover_delay = ms;
        }
        self
    }

    /// How long after the pointer leaves a symbol the hover popover goes away,
    /// in milliseconds. **Zero leaves it up.** Only for
    /// [`InputMode::CodeEditor`].
    pub fn hover_hiding_delay(mut self, ms: u16) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            hover_hiding_delay, ..
        } = &mut self.mode
        {
            *hover_hiding_delay = ms;
        }
        self
    }

    /// Keep the hover popover up while the pointer is over it, only for
    /// [`InputMode::CodeEditor`].
    pub fn hover_sticky(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { hover_sticky, .. } = &mut self.mode {
            *hover_sticky = on;
        }
        self
    }

    /// Prefer the space above the line for the hover popover, only for
    /// [`InputMode::CodeEditor`].
    pub fn hover_above(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { hover_above, .. } = &mut self.mode {
            *hover_above = on;
        }
        self
    }

    /// Draw inlay hints in this font, only for [`InputMode::CodeEditor`].
    ///
    /// `None` -- or an empty name -- takes the editor's own font.
    pub fn inlay_hint_font_family(mut self, font: Option<impl Into<SharedString>>) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            inlay_hint_font_family,
            ..
        } = &mut self.mode
        {
            *inlay_hint_font_family = font
                .map(Into::into)
                .filter(|f: &SharedString| !f.trim().is_empty());
        }
        self
    }

    /// Draw inlay hints at this size in px, only for
    /// [`InputMode::CodeEditor`].
    ///
    /// `0` draws them at 90% of the editor's text, the way VS Code's
    /// `editor.inlayHints.fontSize` reads `0`.
    pub fn inlay_hint_font_size(mut self, size: f32) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            inlay_hint_font_size,
            ..
        } = &mut self.mode
        {
            *inlay_hint_font_size = size.max(0.);
        }
        self
    }

    /// Mark every other run of the selected text, only for
    /// [`InputMode::CodeEditor`].
    ///
    /// `max_len` is in characters; a longer selection is left alone.
    /// `multiline` decides whether a selection that spans rows counts.
    pub fn selection_highlight(mut self, on: bool, max_len: u16, multiline: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            selection_highlight,
            selection_highlight_max_len,
            selection_highlight_multiline,
            ..
        } = &mut self.mode
        {
            *selection_highlight = on;
            *selection_highlight_max_len = max_len;
            *selection_highlight_multiline = multiline;
        }
        self
    }

    /// Round the corners of the selection, only for
    /// [`InputMode::CodeEditor`].
    pub fn rounded_selection(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            rounded_selection, ..
        } = &mut self.mode
        {
            *rounded_selection = on;
        }
        self
    }

    /// Copy the caret's row when nothing is selected, only for
    /// [`InputMode::CodeEditor`].
    ///
    /// Cutting takes the row away too, the way VS Code's
    /// `editor.emptySelectionClipboard` does.
    pub fn empty_selection_clipboard(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            empty_selection_clipboard,
            ..
        } = &mut self.mode
        {
            *empty_selection_clipboard = on;
        }
        self
    }

    /// Mark out the row the caret is on this way, only for
    /// [`InputMode::CodeEditor`].
    pub fn line_highlight(mut self, how: LineHighlight) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { line_highlight, .. } = &mut self.mode {
            *line_highlight = how;
        }
        self
    }

    /// Only mark out the caret's row while the editor has focus, only for
    /// [`InputMode::CodeEditor`].
    pub fn line_highlight_focused_only(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            line_highlight_focused_only,
            ..
        } = &mut self.mode
        {
            *line_highlight_focused_only = on;
        }
        self
    }

    /// Leave this much blank space above the first row and below the last, in
    /// px, only for [`InputMode::CodeEditor`].
    ///
    /// The space is part of the content: it scrolls away, and every offset the
    /// editor measures starts below it.
    pub fn editor_padding(mut self, top: u16, bottom: u16) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            padding_top,
            padding_bottom,
            ..
        } = &mut self.mode
        {
            *padding_top = top;
            *padding_bottom = bottom;
        }
        self
    }

    /// Keep this many columns clear between the code and an end-of-row
    /// diagnostic, only for [`InputMode::CodeEditor`].
    pub fn inline_diagnostic_padding(mut self, columns: u8) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            inline_diagnostic_padding,
            ..
        } = &mut self.mode
        {
            *inline_diagnostic_padding = columns;
        }
        self
    }

    /// Start an end-of-row diagnostic at this column at the earliest, only for
    /// [`InputMode::CodeEditor`].
    ///
    /// Lines that reach past it push their diagnostic further right; short
    /// lines line up here instead of following the code.
    pub fn inline_diagnostic_min_column(mut self, column: u16) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            inline_diagnostic_min_column,
            ..
        } = &mut self.mode
        {
            *inline_diagnostic_min_column = column;
        }
        self
    }

    /// Show the supplied inlay hints with no key held, only for
    /// [`InputMode::CodeEditor`].
    ///
    /// `false` with an [`InlayModifier`] set means "hidden until the key is
    /// held"; `false` with no modifier means they never show.
    pub fn inlay_hints_on(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { inlay_hints_on, .. } = &mut self.mode {
            *inlay_hints_on = on;
        }
        self
    }

    /// Draw a chip behind each inlay hint, only for
    /// [`InputMode::CodeEditor`].
    pub fn inlay_hint_background(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            inlay_hint_background,
            ..
        } = &mut self.mode
        {
            *inlay_hint_background = on;
        }
        self
    }

    /// Flip inlay hints while this modifier is held, only for
    /// [`InputMode::CodeEditor`].
    pub fn inlay_hint_modifier(mut self, modifier: InlayModifier) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            inlay_hint_modifier,
            ..
        } = &mut self.mode
        {
            *inlay_hint_modifier = modifier;
        }
        self
    }

    /// Set where soft wrapping breaks a long line, only for
    /// [`InputMode::CodeEditor`] mode.
    ///
    /// Only consulted while soft wrap is on.
    pub fn wrap_at(mut self, wrap_at: WrapAt) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { wrap_at: w, .. } = &mut self.mode {
            *w = wrap_at;
        }
        self
    }

    /// Draw a vertical ruler at each of these columns, only for
    /// [`InputMode::CodeEditor`] mode.
    ///
    /// A column past the right edge of the editor is not drawn.
    pub fn rulers(mut self, columns: Vec<usize>) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { rulers, .. } = &mut self.mode {
            *rulers = columns.into();
        }
        self
    }

    /// Enable folding, only for [`InputMode::CodeEditor`] mode.
    pub fn folding(mut self, folding: bool) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor { folding: f, .. } = &mut self.mode {
            *f = folding;
        }
        self
    }

    /// Enable folding, only for [`InputMode::CodeEditor`] mode.
    /// Set when the fold chevrons are visible, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn folding_controls(mut self, controls: FoldingControls) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            folding_controls: c,
            ..
        } = &mut self.mode
        {
            *c = controls;
        }
        self
    }

    /// Set which auto-closing behaviours are on, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn auto_close(mut self, auto_close: impl Into<AutoClose>) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { auto_close: a, .. } = &mut self.mode {
            *a = auto_close.into();
        }
        self
    }

    /// Set which auto-closing behaviours are on, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_auto_close(
        &mut self,
        auto_close: impl Into<AutoClose>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor { auto_close: a, .. } = &mut self.mode {
            *a = auto_close.into();
        }
        cx.notify();
    }

    /// Set when the matching bracket is outlined, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn match_brackets(mut self, match_brackets: impl Into<MatchBrackets>) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { match_brackets: m, .. } = &mut self.mode {
            *m = match_brackets.into();
        }
        self
    }

    /// Colour bracket pairs by nesting depth, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn bracket_colors(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { bracket_colors, .. } = &mut self.mode {
            *bracket_colors = on;
        }
        self
    }

    /// Count each bracket kind's depth on its own, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn bracket_colors_per_type(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            bracket_colors_per_type,
            ..
        } = &mut self.mode
        {
            *bracket_colors_per_type = on;
        }
        self
    }

    /// Count each bracket kind's depth on its own, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_bracket_colors_per_type(
        &mut self,
        on: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
            bracket_colors_per_type,
            ..
        } = &mut self.mode
        {
            *bracket_colors_per_type = on;
        }
        cx.notify();
    }

    /// Put a stub at each end of the bracket guides, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn bracket_guides_horizontal(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            bracket_guides_horizontal,
            ..
        } = &mut self.mode
        {
            *bracket_guides_horizontal = on;
        }
        self
    }

    /// Put a stub at each end of the bracket guides, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_bracket_guides_horizontal(
        &mut self,
        on: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
            bracket_guides_horizontal,
            ..
        } = &mut self.mode
        {
            *bracket_guides_horizontal = on;
        }
        cx.notify();
    }

    /// Draw the pair the caret is in stronger than the rest, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn highlight_active_bracket_pair(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            highlight_active_bracket_pair,
            ..
        } = &mut self.mode
        {
            *highlight_active_bracket_pair = on;
        }
        self
    }

    /// Draw the pair the caret is in stronger than the rest, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_highlight_active_bracket_pair(
        &mut self,
        on: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
            highlight_active_bracket_pair,
            ..
        } = &mut self.mode
        {
            *highlight_active_bracket_pair = on;
        }
        cx.notify();
    }

    /// Draw a guide line down each bracket pair, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn bracket_guides(mut self, guides: impl Into<BracketGuides>) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { bracket_guides, .. } = &mut self.mode {
            *bracket_guides = guides.into();
        }
        self
    }

    /// Put a space after the comment token, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn comment_insert_space(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            comment_insert_space,
            ..
        } = &mut self.mode
        {
            *comment_insert_space = on;
        }
        self
    }

    /// Carry a line comment onto the next line on Enter, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn comment_on_newline(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            comment_on_newline, ..
        } = &mut self.mode
        {
            *comment_on_newline = on;
        }
        self
    }

    /// The shape of the caret, only for [`InputMode::CodeEditor`] mode.
    pub fn cursor_style(mut self, style: CursorStyle) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { cursor_style, .. } = &mut self.mode {
            *cursor_style = style;
        }
        self
    }

    /// The shape of the caret, only for [`InputMode::CodeEditor`] mode.
    pub fn set_cursor_style(&mut self, style: CursorStyle, _: &mut Window, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor { cursor_style, .. } = &mut self.mode {
            *cursor_style = style;
        }
        cx.notify();
    }

    /// How the caret blinks, only for [`InputMode::CodeEditor`] mode.
    pub fn cursor_blinking(mut self, blinking: CursorBlinking) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            cursor_blinking, ..
        } = &mut self.mode
        {
            *cursor_blinking = blinking;
        }
        self
    }

    /// How the caret blinks, only for [`InputMode::CodeEditor`] mode.
    pub fn set_cursor_blinking(
        &mut self,
        blinking: CursorBlinking,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
            cursor_blinking, ..
        } = &mut self.mode
        {
            *cursor_blinking = blinking;
        }
        // The timer only runs for `Blink`, so the style has to be handed over.
        let blinking = self.mode.cursor_blinking();
        self.blink_cursor
            .update(cx, |cursor, cx| cursor.start(blinking, cx));
        cx.notify();
    }

    /// The width of a line caret in px (`0` keeps the built-in 1.5px), only
    /// for [`InputMode::CodeEditor`] mode.
    pub fn cursor_width(mut self, width: u8) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { cursor_width, .. } = &mut self.mode {
            *cursor_width = width;
        }
        self
    }

    /// The width of a line caret in px, only for [`InputMode::CodeEditor`] mode.
    pub fn set_cursor_width(&mut self, width: u8, _: &mut Window, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor { cursor_width, .. } = &mut self.mode {
            *cursor_width = width;
        }
        cx.notify();
    }

    /// The height of a line caret as a percent of the line (`0` is auto),
    /// only for [`InputMode::CodeEditor`] mode.
    pub fn cursor_height(mut self, percent: u8) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { cursor_height, .. } = &mut self.mode {
            *cursor_height = percent;
        }
        self
    }

    /// The height of a line caret as a percent of the line, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_cursor_height(&mut self, percent: u8, _: &mut Window, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor { cursor_height, .. } = &mut self.mode {
            *cursor_height = percent;
        }
        cx.notify();
    }

    /// Slide the caret between positions, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn caret_animation(mut self, animation: CaretAnimation) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            caret_animation, ..
        } = &mut self.mode
        {
            *caret_animation = animation;
        }
        self
    }

    /// Slide the caret between positions, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_caret_animation(
        &mut self,
        animation: CaretAnimation,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
            caret_animation, ..
        } = &mut self.mode
        {
            *caret_animation = animation;
        }
        self.caret_slide = None;
        cx.notify();
    }

    /// How many rows to keep above and below the caret, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn cursor_surrounding_lines(mut self, rows: u8) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            cursor_surrounding_lines,
            ..
        } = &mut self.mode
        {
            *cursor_surrounding_lines = rows;
        }
        self
    }

    /// How many rows to keep above and below the caret, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_cursor_surrounding_lines(
        &mut self,
        rows: u8,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
            cursor_surrounding_lines,
            ..
        } = &mut self.mode
        {
            *cursor_surrounding_lines = rows;
        }
        cx.notify();
    }

    /// When those rows are enforced, only for [`InputMode::CodeEditor`] mode.
    pub fn surrounding_lines_style(mut self, style: SurroundingLinesStyle) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            surrounding_lines_style,
            ..
        } = &mut self.mode
        {
            *surrounding_lines_style = style;
        }
        self
    }

    /// When those rows are enforced, only for [`InputMode::CodeEditor`] mode.
    pub fn set_surrounding_lines_style(
        &mut self,
        style: SurroundingLinesStyle,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor {
            surrounding_lines_style,
            ..
        } = &mut self.mode
        {
            *surrounding_lines_style = style;
        }
        cx.notify();
    }

    /// Scroll to keep those rows when the caret is placed by a click, only
    /// for [`InputMode::CodeEditor`] mode.
    pub fn autoscroll_on_clicks(mut self, on: bool) -> Self {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            autoscroll_on_clicks,
            ..
        } = &mut self.mode
        {
            *autoscroll_on_clicks = on;
        }
        self
    }

    /// Scroll to keep those rows when the caret is placed by a click, only
    /// for [`InputMode::CodeEditor`] mode.
    pub fn set_autoscroll_on_clicks(&mut self, on: bool, _: &mut Window, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor {
            autoscroll_on_clicks,
            ..
        } = &mut self.mode
        {
            *autoscroll_on_clicks = on;
        }
        cx.notify();
    }

    /// Carry a line comment onto the next line on Enter, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_comment_on_newline(&mut self, on: bool, _: &mut Window, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor {
            comment_on_newline, ..
        } = &mut self.mode
        {
            *comment_on_newline = on;
        }
        cx.notify();
    }

    /// Put a space after the comment token, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_comment_insert_space(&mut self, on: bool, _: &mut Window, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor {
            comment_insert_space,
            ..
        } = &mut self.mode
        {
            *comment_insert_space = on;
        }
        cx.notify();
    }

    /// Draw a guide line down each bracket pair, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_bracket_guides(
        &mut self,
        guides: impl Into<BracketGuides>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor { bracket_guides, .. } = &mut self.mode {
            *bracket_guides = guides.into();
        }
        cx.notify();
    }

    /// Colour bracket pairs by nesting depth, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_bracket_colors(&mut self, on: bool, _: &mut Window, cx: &mut Context<Self>) {
        if let InputMode::CodeEditor { bracket_colors, .. } = &mut self.mode {
            *bracket_colors = on;
        }
        cx.notify();
    }

    /// Set when the matching bracket is outlined, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_match_brackets(
        &mut self,
        match_brackets: impl Into<MatchBrackets>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let InputMode::CodeEditor { match_brackets: m, .. } = &mut self.mode {
            *m = match_brackets.into();
        }
        cx.notify();
    }

    /// Set when the fold chevrons are visible, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_folding_controls(
        &mut self,
        controls: FoldingControls,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            folding_controls: c,
            ..
        } = &mut self.mode
        {
            *c = controls;
        }
        cx.notify();
    }

    pub fn set_folding(&mut self, folding: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor { folding: f, .. } = &mut self.mode {
            *f = folding;
        }
        self.update_folds();
        cx.notify();
    }

    /// Set which whitespace characters are drawn, only for
    /// [`InputMode::CodeEditor`] mode.
    ///
    /// Takes a [`RenderWhitespace`], or a `bool` for the all/nothing case.
    pub fn render_whitespace(mut self, render_whitespace: impl Into<RenderWhitespace>) -> Self {
        debug_assert!(self.mode.is_code_editor() && self.mode.is_multi_line());
        if let InputMode::CodeEditor {
            render_whitespace: w,
            ..
        } = &mut self.mode
        {
            *w = render_whitespace.into();
        }
        self
    }

    /// Set which whitespace characters are drawn, only for
    /// [`InputMode::CodeEditor`] mode.
    pub fn set_render_whitespace(
        &mut self,
        render_whitespace: impl Into<RenderWhitespace>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(self.mode.is_code_editor());
        if let InputMode::CodeEditor {
            render_whitespace: w,
            ..
        } = &mut self.mode
        {
            *w = render_whitespace.into();
        }
        cx.notify();
    }

    /// Set the number of rows for the multi-line Textarea.
    ///
    /// This is only used when `multi_line` is set to true.
    ///
    /// default: 2
    pub fn rows(mut self, rows: usize) -> Self {
        match &mut self.mode {
            InputMode::PlainText { rows: r, .. } | InputMode::CodeEditor { rows: r, .. } => {
                *r = rows
            }
            InputMode::AutoGrow {
                max_rows: max_r,
                rows: r,
                ..
            } => {
                *r = rows;
                *max_r = rows;
            }
        }
        self
    }

    /// Set highlighter language for for [`InputMode::CodeEditor`] mode.
    pub fn set_highlighter(
        &mut self,
        new_language: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        match &mut self.mode {
            InputMode::CodeEditor {
                language,
                highlighter,
                ..
            } => {
                *language = new_language.into();
                *highlighter.borrow_mut() = None;
            }
            _ => {}
        }
        cx.notify();
    }

    fn reset_highlighter(&mut self, cx: &mut Context<Self>) {
        match &mut self.mode {
            InputMode::CodeEditor { highlighter, .. } => {
                *highlighter.borrow_mut() = None;
            }
            _ => {}
        }
        cx.notify();
    }

    #[inline]
    pub fn diagnostics(&self) -> Option<&DiagnosticSet> {
        self.mode.diagnostics()
    }

    #[inline]
    pub fn diagnostics_mut(&mut self) -> Option<&mut DiagnosticSet> {
        self.mode.diagnostics_mut()
    }

    /// Set placeholder
    pub fn set_placeholder(
        &mut self,
        placeholder: impl Into<SharedString>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.placeholder = placeholder.into();
        cx.notify();
    }

    /// Find which line and sub-line the given offset belongs to, along with the position within that sub-line.
    ///
    /// Returns:
    ///
    /// - The index of the line (zero-based) containing the offset.
    /// - The index of the sub-line (zero-based) within the line containing the offset.
    /// - The position of the offset.
    #[allow(unused)]
    pub(super) fn line_and_position_for_offset(
        &self,
        offset: usize,
    ) -> (usize, usize, Option<Point<Pixels>>) {
        let Some(last_layout) = &self.last_layout else {
            return (0, 0, None);
        };
        let line_height = last_layout.line_height;

        let mut prev_lines_offset = last_layout.visible_range_offset.start;
        let mut y_offset = last_layout.visible_top;
        for (line_index, line) in last_layout.lines.iter().enumerate() {
            let local_offset = offset.saturating_sub(prev_lines_offset);
            if let Some(pos) = line.position_for_index(local_offset, line_height) {
                let sub_line_index = (pos.y / line_height) as usize;
                let adjusted_pos = point(pos.x + last_layout.line_number_width, pos.y + y_offset);
                return (line_index, sub_line_index, Some(adjusted_pos));
            }

            y_offset += line.size(line_height).height;
            prev_lines_offset += line.len() + 1;
        }
        (0, 0, None)
    }

    /// Set the text of the input field.
    ///
    /// And the selection_range will be reset to 0..0.
    pub fn set_value(
        &mut self,
        value: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.history.ignore = true;
        let was_disabled = self.disabled;
        self.disabled = false;
        self.replace_text(value, window, cx);
        self.disabled = was_disabled;
        self.history.ignore = false;

        // Ensure cursor to start when set text
        if self.mode.is_single_line() {
            self.selected_range = (self.text.len()..self.text.len()).into();
        } else {
            self.selected_range.clear();
        }

        if self.mode.is_code_editor() {
            self._pending_update = true;
            self.lsp.reset();
        }

        // Move scroll to top
        self.scroll_handle.set_offset(point(px(0.), px(0.)));

        cx.notify();
    }

    /// Insert text at the current cursor position.
    ///
    /// And the cursor will be moved to the end of inserted text.
    pub fn insert(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        let range_utf16 = self.range_to_utf16(&(self.cursor()..self.cursor()));
        self.replace_text_in_range_silent(Some(range_utf16), &text, window, cx);
        self.selected_range = (self.selected_range.end..self.selected_range.end).into();
    }

    /// Replace text at the current cursor position.
    ///
    /// And the cursor will be moved to the end of replaced text.
    pub fn replace(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        self.replace_text_in_range_silent(None, &text, window, cx);
        self.selected_range = (self.selected_range.end..self.selected_range.end).into();
    }

    fn replace_text(
        &mut self,
        text: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text: SharedString = text.into();
        let range = 0..self.text.chars().map(|c| c.len_utf16()).sum();
        self.replace_text_in_range_silent(Some(range), &text, window, cx);
        self.reset_highlighter(cx);
    }

    /// Set with disabled mode.
    ///
    /// See also: [`Self::set_disabled`], [`Self::is_disabled`].
    #[allow(unused)]
    pub(crate) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Set with password masked state.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn masked(mut self, masked: bool) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.masked = masked;
        self
    }

    /// Set the password masked state of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn set_masked(&mut self, masked: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_single_line());
        self.masked = masked;
        cx.notify();
    }

    /// Set true to clear the input by pressing Escape key.
    pub fn clean_on_escape(mut self) -> Self {
        self.clean_on_escape = true;
        self
    }

    /// Set the soft wrap mode for multi-line input, default is true.
    pub fn soft_wrap(mut self, wrap: bool) -> Self {
        debug_assert!(self.mode.is_multi_line());
        self.soft_wrap = wrap;
        self
    }

    /// Update the soft wrap mode for multi-line input, default is true.
    pub fn set_soft_wrap(&mut self, wrap: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_multi_line());
        self.soft_wrap = wrap;
        if wrap {
            let wrap_width = self
                .last_layout
                .as_ref()
                .and_then(|b| b.wrap_width)
                .unwrap_or(self.input_bounds.size.width);

            self.text_wrapper.set_wrap_width(Some(wrap_width), cx);

            // Reset scroll to left 0
            let mut offset = self.scroll_handle.offset();
            offset.x = px(0.);
            self.scroll_handle.set_offset(offset);
        } else {
            self.text_wrapper.set_wrap_width(None, cx);
        }
        cx.notify();
    }

    /// Set the regular expression pattern of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn pattern(mut self, pattern: regex::Regex) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.pattern = Some(pattern);
        self
    }

    /// Set the regular expression pattern of the input field with reference.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn set_pattern(
        &mut self,
        pattern: regex::Regex,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        debug_assert!(self.mode.is_single_line());
        self.pattern = Some(pattern);
    }

    /// Set the validation function of the input field.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn validate(mut self, f: impl Fn(&str, &mut Context<Self>) -> bool + 'static) -> Self {
        debug_assert!(self.mode.is_single_line());
        self.validate = Some(Box::new(f));
        self
    }

    /// Set true to show spinner at the input right.
    ///
    /// Only for [`InputMode::SingleLine`] mode.
    pub fn set_loading(&mut self, loading: bool, _: &mut Window, cx: &mut Context<Self>) {
        debug_assert!(self.mode.is_single_line());
        self.loading = loading;
        cx.notify();
    }

    /// Set the default value of the input field.
    pub fn default_value(mut self, value: impl Into<SharedString>) -> Self {
        let text: SharedString = value.into();
        self.text = Rope::from(text.as_str());
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        self.text_wrapper.set_default_text(&self.text);
        // A new document: the old headers mean nothing.
        self.folded_rows.clear();
        self.update_folds();
        self._pending_update = true;
        self
    }

    /// Return the value of the input field.
    pub fn value(&self) -> SharedString {
        SharedString::new(self.text.to_string())
    }

    /// Return the value without mask.
    pub fn unmask_value(&self) -> SharedString {
        self.mask_pattern.unmask(&self.text.to_string()).into()
    }

    /// Return the text [`Rope`] of the input field.
    pub fn text(&self) -> &Rope {
        &self.text
    }

    /// Return the (0-based) [`Position`] of the cursor.
    pub fn cursor_position(&self) -> Position {
        let offset = self.cursor();
        self.text.offset_to_position(offset)
    }

    /// Set (0-based) [`Position`] of the cursor.
    ///
    /// This will move the cursor to the specified line and column, and update the selection range.
    pub fn set_cursor_position(
        &mut self,
        position: impl Into<Position>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let position: Position = position.into();
        let offset = self.text.position_to_offset(&position);

        self.move_to(offset, None, cx);
        self.update_preferred_column();
        self.focus(window, cx);
    }

    /// Put the cursor at a (0-based) [`Position`] **without taking focus**.
    ///
    /// [`InputState::set_cursor_position`] focuses the input, which is right
    /// when a jump was asked for and wrong when a pane is being put back the
    /// way it was: restoring three background editors must not steal the
    /// window from the one in front.
    pub fn set_cursor_position_quietly(
        &mut self,
        position: impl Into<Position>,
        cx: &mut Context<Self>,
    ) {
        let position: Position = position.into();
        let offset = self.text.position_to_offset(&position);

        self.move_to(offset, None, cx);
        self.update_preferred_column();
    }

    /// The buffer row at the top of the viewport, as of the last frame.
    ///
    /// `None` before the first layout. **Rows, not pixels** -- a remembered
    /// pixel offset means something else after a font size or a wrap width
    /// changes, a row does not.
    pub fn first_visible_row(&self) -> Option<usize> {
        self.last_layout
            .as_ref()
            .map(|l| l.visible_range.start)
    }

    /// Scroll so `row` sits at the top of the viewport.
    ///
    /// Goes through `deferred_scroll_offset`, so it can be called before the
    /// first layout -- which is exactly when a restored position arrives.
    pub fn scroll_to_row(&mut self, row: usize, cx: &mut Context<Self>) {
        // **Kill any slide already in flight** (#404). `set_cursor_position`
        // reveals the caret through `scroll_to`, and with smooth scrolling on
        // (#252) that hands a 120ms ease to a background task which rewrites
        // `deferred_scroll_offset` every 8ms. It outlives this call, so the
        // absolute offset parked below shows for exactly one frame and is then
        // walked back to wherever the caret reveal was heading -- the jumped-to
        // row ends up at the bottom edge instead of the top.
        //
        // This is the explicit "put this row here" path, so a caret reveal
        // queued a moment ago is stale by definition.
        self.scroll_slide = None;
        self.scroll_slide_task = None;
        let line_height = self
            .last_layout
            .as_ref()
            .map_or(px(0.), |l| l.line_height);
        // Starts below the pad, like `scroll_to` and the element's own walk
        // (#255 / ADR-0090). Seeding at zero lands the row `padding_top` low.
        let mut y = self.mode.padding_top();
        for line in self.text_wrapper.lines.iter().take(row) {
            y += line.height(line_height);
        }
        // **Keep whatever horizontal reveal is already pending.** Called
        // straight after `move_to`, reading `scroll_handle` instead would
        // throw away the x that just brought the caret's column into view.
        let mut offset = self
            .deferred_scroll_offset
            .unwrap_or_else(|| self.scroll_handle.offset());
        offset.y = -y;
        self.deferred_scroll_offset = Some(offset);
        cx.notify();
    }

    /// Scroll so `row` sits in the middle of the viewport.
    ///
    /// Goes through [`InputState::scroll_to_row`] rather than opening a
    /// second scrolling path -- `scroll_to` is shared with caret movement,
    /// clicks and session restore, and must keep meaning "just enough".
    ///
    /// `visible_range` counts **buffer rows**, the same unit as `row`, so a
    /// softly wrapped file corrects itself: fewer rows fit, so half of them
    /// is a smaller number.
    pub fn scroll_row_to_center(&mut self, row: usize, cx: &mut Context<Self>) {
        let half = self
            .last_layout
            .as_ref()
            .map_or(0, |l| l.visible_range.len() / 2);
        self.scroll_to_row(row.saturating_sub(half), cx);
    }

    /// Focus the input field.
    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_handle.focus(window);
        let blinking = self.mode.cursor_blinking();
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.start(blinking, cx);
        });
    }

    pub(super) fn select_left(&mut self, _: &SelectLeft, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.previous_boundary(self.cursor()), cx);
    }

    pub(super) fn select_right(&mut self, _: &SelectRight, _: &mut Window, cx: &mut Context<Self>) {
        self.select_to(self.next_boundary(self.cursor()), cx);
    }

    pub(super) fn select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_single_line() {
            return;
        }
        let offset = self.start_of_line().saturating_sub(1);
        self.select_to(self.previous_boundary(offset), cx);
    }

    pub(super) fn select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_single_line() {
            return;
        }
        let offset = (self.end_of_line() + 1).min(self.text.len());
        self.select_to(self.next_boundary(offset), cx);
    }

    pub(super) fn select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        self.selected_range = (0..self.text.len()).into();
        cx.notify();
    }

    pub(super) fn select_to_start(
        &mut self,
        _: &SelectToStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_to(0, cx);
    }

    pub(super) fn select_to_end(
        &mut self,
        _: &SelectToEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let end = self.text.len();
        self.select_to(end, cx);
    }

    pub(super) fn select_to_start_of_line(
        &mut self,
        _: &SelectToStartOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.start_of_line();
        self.select_to(offset, cx);
    }

    pub(super) fn select_to_end_of_line(
        &mut self,
        _: &SelectToEndOfLine,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.end_of_line();
        self.select_to(offset, cx);
    }

    pub(super) fn select_to_previous_word(
        &mut self,
        _: &SelectToPreviousWordStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.previous_start_of_word();
        self.select_to(offset, cx);
    }

    pub(super) fn select_to_next_word(
        &mut self,
        _: &SelectToNextWordEnd,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let offset = self.next_end_of_word();
        self.select_to(offset, cx);
    }

    /// Return the start offset of the previous word.
    ///
    /// **Uses the same idea of "word" as double-click** (`selection::
    /// word_range`). These were two different answers before: this walk asked
    /// `unicode-segmentation` while a double-click asked a table that counted
    /// only ASCII, so ⌥← and a double-click disagreed about `café` and about
    /// every Japanese word.
    ///
    /// It also **copied the whole left half of the document into a `String`
    /// on every press** to do it. Now it steps by words through a window.
    pub(super) fn previous_start_of_word(&mut self) -> usize {
        let mut offset = self.selected_range.start;
        offset = self.offset_from_utf16(self.offset_to_utf16(offset));
        // Step back over whatever sits immediately behind the caret, then
        // keep stepping while it is blank -- ⌥← should land on a word, not
        // in the gap before one.
        loop {
            if offset == 0 {
                return 0;
            }
            let Some(prev) = self.text.chars_at(offset).reversed().next() else {
                return 0;
            };
            let before = offset - prev.len_utf8();
            let Some(range) = self.word_at_offset(before) else {
                return before;
            };
            if !self.text.slice(range.clone()).to_string().trim().is_empty() {
                return range.start;
            }
            offset = range.start;
        }
    }

    /// Return the next end offset of the next word.
    ///
    /// The mirror of [`InputState::previous_start_of_word`]; same reasons.
    pub(super) fn next_end_of_word(&mut self) -> usize {
        let len = self.text.len();
        let mut offset = self.cursor();
        offset = self.offset_from_utf16(self.offset_to_utf16(offset));
        loop {
            if offset >= len {
                return len;
            }
            let Some(range) = self.word_at_offset(offset) else {
                return len;
            };
            if !self.text.slice(range.clone()).to_string().trim().is_empty() {
                return range.end;
            }
            if range.end <= offset {
                return len;
            }
            offset = range.end;
        }
    }

    /// Get start of line byte offset of cursor
    pub(super) fn start_of_line(&self) -> usize {
        if self.mode.is_single_line() {
            return 0;
        }

        let row = self.text.offset_to_point(self.cursor()).row;
        self.text.line_start_offset(row)
    }

    /// Get end of line byte offset of cursor
    pub(super) fn end_of_line(&self) -> usize {
        if self.mode.is_single_line() {
            return self.text.len();
        }

        let row = self.text.offset_to_point(self.cursor()).row;
        self.text.line_end_offset(row)
    }

    /// Get start line of selection start or end (The min value).
    ///
    /// This is means is always get the first line of selection.
    pub(super) fn start_of_line_of_selection(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> usize {
        if self.mode.is_single_line() {
            return 0;
        }

        let mut offset =
            self.previous_boundary(self.selected_range.start.min(self.selected_range.end));
        if self.text.char_at(offset) == Some('\r') {
            offset += 1;
        }

        let line = self
            .text_for_range(self.range_to_utf16(&(0..offset + 1)), &mut None, window, cx)
            .unwrap_or_default()
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        line
    }

    /// Get indent string of next line.
    ///
    /// To get current and next line indent, to return more depth one.
    pub(super) fn indent_of_next_line(&mut self) -> String {
        if self.mode.is_single_line() {
            return "".into();
        }
        // `AutoIndent::None` starts every line at column zero (#248).
        if self.mode.auto_indent() == super::mode::AutoIndent::None {
            return "".into();
        }

        let mut current_indent = String::new();
        let mut next_indent = String::new();
        let current_line_start_pos = self.start_of_line();
        let next_line_start_pos = self.end_of_line();
        for c in self.text.slice(current_line_start_pos..).chars() {
            if !c.is_whitespace() {
                break;
            }
            if c == '\n' || c == '\r' {
                break;
            }
            current_indent.push(c);
        }

        for c in self.text.slice(next_line_start_pos..).chars() {
            if !c.is_whitespace() {
                break;
            }
            if c == '\n' || c == '\r' {
                break;
            }
            next_indent.push(c);
        }

        let base = if next_indent.len() > current_indent.len() {
            next_indent
        } else {
            current_indent
        };

        // `Brackets`: a line that leaves a bracket open puts the next one a
        // level deeper. **The syntax tree is not consulted** -- see
        // `auto_indent.rs` for why.
        if self.mode.auto_indent() != super::mode::AutoIndent::Brackets {
            return base;
        }
        let line_start = self.start_of_line();
        let before = self.text.slice(line_start..self.cursor()).to_string();
        let pairs = super::brackets::pairs_for(self.mode.language_name());
        if super::auto_indent::opens_block(&before, &pairs) {
            return base + &self.mode.tab_size().to_string();
        }
        base
    }

    /// What auto-closing wants to do for this insertion, if anything.
    ///
    /// Returns `None` for every path that is not a person typing one character
    /// into a code editor:
    ///
    /// - `silent_replace_text` covers `insert` / `replace` / `paste` / `enter`
    ///   / the LSP, none of which should grow a bracket;
    /// - `ime_marked_range` covers composition — a CJK IME routes even plain
    ///   ASCII through `replace_and_mark_text_in_range`, so closing there would
    ///   fire mid-preedit;
    /// - an explicit `range_utf16` means the caller chose the range, not the
    ///   caret.
    fn auto_close_edit(
        &self,
        range_utf16: Option<&Range<usize>>,
        new_text: &str,
    ) -> Option<AutoCloseEdit> {
        if self.silent_replace_text || self.ime_marked_range.is_some() || range_utf16.is_some() {
            return None;
        }
        if !self.mode.is_code_editor() {
            return None;
        }
        let cfg = self.mode.auto_close();
        let mut chars = new_text.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            return None;
        }
        let language = self.mode.language_name();
        let selection: Range<usize> = self.selected_range.into();

        if selection.is_empty() {
            if cfg.overtype && brackets::is_overtype(&self.text, selection.start, ch, language) {
                return Some(AutoCloseEdit::Overtype {
                    to: selection.start + ch.len_utf8(),
                });
            }
            // The line up to and including what was just typed. Both the block
            // comment and the JSX tag need it, and neither needs the tree.
            let line_start = self.text.line_start_offset(
                self.text.offset_to_point(selection.start).row,
            );
            let mut before = self.text.slice(line_start..selection.start).to_string();
            before.push(ch);
            let next = self.text.char_at(selection.start);

            if cfg.comments {
                if let Some(close) = comment::block_open_at(&before, next, language) {
                    let space = if self.mode.comment_insert_space() { " " } else { "" };
                    let closer = format!("{space}{close}");
                    let mut text = String::with_capacity(ch.len_utf8() + closer.len());
                    text.push(ch);
                    text.push_str(&closer);
                    return Some(AutoCloseEdit::Insert {
                        caret_back: closer.len(),
                        text,
                    });
                }
            }
            if cfg.jsx_tags && ch == '>' && tags::is_jsx(language) {
                if let Some(close) = tags::closing_tag(&before) {
                    let mut text = String::with_capacity(ch.len_utf8() + close.len());
                    text.push(ch);
                    text.push_str(&close);
                    return Some(AutoCloseEdit::Insert {
                        caret_back: close.len(),
                        text,
                    });
                }
            }
            if !cfg.brackets {
                return None;
            }
            let close = brackets::close_for(&self.text, selection.start, ch, language)?;
            let mut text = String::with_capacity(ch.len_utf8() + close.len_utf8());
            text.push(ch);
            text.push(close);
            return Some(AutoCloseEdit::Insert {
                text,
                caret_back: close.len_utf8(),
            });
        }

        if !cfg.surround {
            return None;
        }
        let close = brackets::closer_of(ch, language)?;
        let inner = self.text.slice(selection).to_string();
        let mut text = String::with_capacity(ch.len_utf8() + inner.len() + close.len_utf8());
        text.push(ch);
        text.push_str(&inner);
        text.push(close);
        Some(AutoCloseEdit::Surround {
            text,
            open_len: ch.len_utf8(),
            inner_len: inner.len(),
        })
    }

    pub(super) fn backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            // Between an auto-closed pair, take both halves: leaving the closer
            // behind is never what the caret position meant.
            let offset = self.cursor();
            if self.mode.auto_close().brackets
                && brackets::is_inside_pair(&self.text, offset, self.mode.language_name())
            {
                self.selected_range =
                    (self.previous_boundary(offset)..self.next_boundary(offset)).into();
            } else if let Some(stop) = self.tab_stop_before_caret() {
                self.select_to(stop, cx)
            } else if let Some(join) = self.join_without_indent() {
                self.select_to(join, cx)
            } else {
                self.select_to(self.previous_boundary(self.cursor()), cx)
            }
        }
        self.replace_text_in_range(None, "", window, cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_range.is_empty() {
            self.select_to(self.next_boundary(self.cursor()), cx)
        }
        self.replace_text_in_range(None, "", window, cx);
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_to_beginning_of_line(
        &mut self,
        _: &DeleteToBeginningOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let mut offset = self.start_of_line();
        if offset == self.cursor() {
            offset = offset.saturating_sub(1);
        }
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(offset..self.cursor()))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let mut offset = self.end_of_line();
        if offset == self.cursor() {
            offset = (offset + 1).clamp(0, self.text.len());
        }
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(self.cursor()..offset))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_previous_word(
        &mut self,
        _: &DeleteToPreviousWordStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let offset = self.previous_start_of_word();
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(offset..self.cursor()))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(super) fn delete_next_word(
        &mut self,
        _: &DeleteToNextWordEnd,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.selected_range.is_empty() {
            self.replace_text_in_range(None, "", window, cx);
            self.pause_blink_cursor(cx);
            return;
        }

        let offset = self.next_end_of_word();
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(self.cursor()..offset))),
            "",
            window,
            cx,
        );
        self.pause_blink_cursor(cx);
    }

    pub(super) fn enter(&mut self, action: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        if self.handle_action_for_context_menu(Box::new(action.clone()), window, cx) {
            return;
        }

        // Clear inline completion on enter (user chose not to accept it)
        if self.has_inline_completion() {
            self.clear_inline_completion(cx);
        }

        if self.mode.is_multi_line() {
            // Get current line indent
            let indent = if self.mode.is_code_editor() {
                self.indent_of_next_line()
            } else {
                "".to_string()
            };

            // Carry a line comment onto the next line, and let a line that is
            // nothing but the marker be the way out of the block.
            let carry = self.comment_to_carry(window, cx);
            let mut prefix = match carry {
                Some(EnterComment::Continue { prefix }) => prefix,
                Some(EnterComment::Clear { range }) => {
                    self.replace_text_in_range_silent(
                        Some(self.range_to_utf16(&range)),
                        "",
                        window,
                        cx,
                    );
                    String::new()
                }
                None => String::new(),
            };

            // A Markdown list marker carries the same way (#248). **A comment
            // wins** -- a `// - foo` line is a comment that happens to hold a
            // dash, and continuing both would put the marker twice.
            let mut indent = indent;
            if prefix.is_empty() {
                match self.list_to_carry(window, cx) {
                    Some(super::list::EnterList::Continue { prefix: p }) => {
                        // The marker already carries the line's own indent.
                        indent = String::new();
                        prefix = p;
                    }
                    Some(super::list::EnterList::Clear { range }) => {
                        self.replace_text_in_range_silent(
                            Some(self.range_to_utf16(&range)),
                            "",
                            window,
                            cx,
                        );
                        indent = String::new();
                    }
                    None => {}
                }
            }

            // Add newline and indent
            let new_line_text = format!("\n{indent}{prefix}");
            self.replace_text_in_range_silent(None, &new_line_text, window, cx);
            // Remember indent nobody asked for, so it can be taken back.
            self.auto_ws = (!indent.is_empty() && prefix.is_empty()).then(|| {
                let end = self.cursor();
                end - indent.len()..end
            });
            self.pause_blink_cursor(cx);
        } else {
            // Single line input, just emit the event (e.g.: In a dialog to confirm).
            cx.propagate();
        }

        cx.emit(InputEvent::PressEnter {
            secondary: action.secondary,
        });
    }

    pub(super) fn clean(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.replace_text("", window, cx);
        self.selected_range = (0..0).into();
        self.scroll_to(0, None, cx);
    }

    pub(super) fn escape(&mut self, action: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        if self.handle_action_for_context_menu(Box::new(action.clone()), window, cx) {
            return;
        }

        // Clear inline completion on escape
        if self.has_inline_completion() {
            self.clear_inline_completion(cx);
            return; // Consume the escape, don't propagate
        }

        if self.ime_marked_range.is_some() {
            self.unmark_text(window, cx);
        }

        if self.clean_on_escape {
            return self.clean(window, cx);
        }

        cx.propagate();
    }

    pub(super) fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The fold gutter is checked **first**: before `selecting` is set and
        // before the caret is moved, so clicking a chevron neither jumps the
        // caret nor starts a drag-selection.
        if self.handle_fold_gutter_click(event, cx) {
            return;
        }
        // Then the row itself: clicking past the end of a folded line opens it
        // (`editor.unfoldOnClickAfterEndOfLine`), before the caret moves.
        if self.handle_click_after_end_of_line(event, cx) {
            return;
        }

        // Clear inline completion on any mouse interaction
        self.clear_inline_completion(cx);

        // If there have IME marked range and is empty (Means pressed Esc to abort IME typing)
        // Clear the marked range.
        if let Some(ime_marked_range) = &self.ime_marked_range {
            if ime_marked_range.len() == 0 {
                self.ime_marked_range = None;
            }
        }

        self.selecting = true;
        let offset = self.index_for_mouse_position(event.position);

        if self.handle_click_hover_definition(event, offset, window, cx) {
            return;
        }

        // **After definitions**, so a symbol that is both still goes to its
        // definition rather than being opened as a path.
        if event.modifiers.secondary() {
            if let Some(range) = self.link_at(offset) {
                cx.emit(InputEvent::LinkClicked { range });
                return;
            }
        }

        // Triple click to select the line.
        //
        // **This was simply missing** -- `click_count == 3` appeared nowhere,
        // so a third click behaved like a first and put the caret down.
        if event.button == MouseButton::Left && event.click_count >= 3 {
            let row = self.text.offset_to_point(offset).row;
            let start = self.text.line_start_offset(row);
            let end = if row + 1 < self.text.lines_len() {
                self.text.line_start_offset(row + 1)
            } else {
                self.text.len()
            };
            self.selected_range = (start..end).into();
            self.selected_word_range = None;
            cx.notify();
            return;
        }

        // Double click to select word -- or, next to a bracket, the block it
        // opens (`editor.doubleClickSelectsBlock`).
        if event.button == MouseButton::Left && event.click_count == 2 {
            if self.select_enclosing_block(offset, cx) {
                return;
            }
            self.select_word(offset, window, cx);
            return;
        }

        // Show Mouse context menu
        if event.button == MouseButton::Right {
            self.handle_right_click_menu(event, offset, window, cx);
            return;
        }

        if event.modifiers.shift {
            self.select_to(offset, cx);
        } else {
            self.move_to(offset, None, cx)
        }
    }

    pub(super) fn on_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        if self.selected_range.is_empty() {
            self.selection_reversed = false;
        }
        self.selecting = false;
        self.selected_word_range = None;
    }

    pub(super) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.track_fold_gutter_hover(event.position, cx);

        // Show diagnostic popover on mouse move
        let offset = self.index_for_mouse_position(event.position);
        // Links underline only while the modifier is down, like definitions.
        if self.track_link_hover(offset, event.modifiers.secondary()) {
            cx.notify();
        }
        self.handle_mouse_move(offset, event, window, cx);

        if self.mode.is_code_editor() {
            if let Some(diagnostic) = self
                .mode
                .diagnostics()
                .and_then(|set| set.for_offset(offset))
            {
                if let Some(diagnostic_popover) = self.diagnostic_popover.as_ref() {
                    if diagnostic_popover.read(cx).diagnostic.range == diagnostic.range {
                        diagnostic_popover.update(cx, |this, cx| {
                            this.show(cx);
                        });

                        return;
                    }
                }

                self.diagnostic_popover = Some(DiagnosticPopover::new(diagnostic, cx.entity(), cx));
                cx.notify();
            } else {
                if let Some(diagnostic_popover) = self.diagnostic_popover.as_mut() {
                    diagnostic_popover.update(cx, |this, cx| {
                        this.check_to_hide(event.position, cx);
                    })
                }
            }
        }
    }

    pub(super) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let line_height = self
            .last_layout
            .as_ref()
            .map(|layout| layout.line_height)
            .unwrap_or(window.line_height());
        let mut delta = event.delta.pixel_delta(line_height);

        // ⌘ + wheel is not scrolling: the host decides what "bigger" means
        // (#252). `InputState` does not own the font size.
        if self.mode.mouse_wheel_zoom() && event.modifiers.platform {
            let steps = (delta.y / line_height).round() as i32;
            if steps != 0 {
                cx.emit(InputEvent::ZoomDelta { steps });
                cx.stop_propagation();
            }
            return;
        }

        // Wheel multipliers (#252). ⌥ is the "go faster" modifier both
        // VS Code and Zed use.
        let (normal, fast) = self.mode.scroll_sensitivity();
        let factor = if event.modifiers.alt { fast } else { normal };
        if (factor - 1.).abs() > f32::EPSILON {
            delta.x *= factor;
            delta.y *= factor;
        }

        // A diagonal gesture moves on one axis only (#252). Trackpads make
        // perfectly straight gestures hard; this decides for the reader.
        if self.mode.scroll_predominant_axis() {
            if delta.y.abs() >= delta.x.abs() {
                delta.x = px(0.);
            } else {
                delta.y = px(0.);
            }
        }

        let old_offset = self.scroll_handle.offset();
        self.update_scroll_offset(Some(old_offset + delta), cx);

        // Only stop propagation if the offset actually changed
        if self.scroll_handle.offset() != old_offset {
            cx.stop_propagation();
        }

        self.diagnostic_popover = None;
    }

    pub(super) fn update_scroll_offset(
        &mut self,
        offset: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let mut offset = offset.unwrap_or(self.scroll_handle.offset());

        let safe_y_range =
            (-self.scroll_size.height + self.input_bounds.size.height).min(px(0.0))..px(0.);
        let safe_x_range =
            (-self.scroll_size.width + self.input_bounds.size.width).min(px(0.0))..px(0.);

        offset.y = if self.mode.is_single_line() {
            px(0.)
        } else {
            offset.y.clamp(safe_y_range.start, safe_y_range.end)
        };
        offset.x = offset.x.clamp(safe_x_range.start, safe_x_range.end);
        self.scroll_handle.set_offset(offset);
        cx.notify();
    }

    /// Scroll to make the given offset visible.
    ///
    /// If `direction` is Some, will keep edges at the same side.
    pub(crate) fn scroll_to(
        &mut self,
        offset: usize,
        direction: Option<MoveDirection>,
        cx: &mut Context<Self>,
    ) {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return;
        };
        let Some(bounds) = self.last_bounds.as_ref() else {
            return;
        };

        let mut scroll_offset = self.scroll_handle.offset();
        let was_offset = scroll_offset;
        let line_height = last_layout.line_height;

        let point = self.text.offset_to_point(offset);

        let row = point.row;

        // Starts below the pad, like every other walk (#255 / ADR-0090) --
        // this one is not seeded from `visible_top`.
        let mut row_offset_y = self.mode.padding_top();
        for (ix, wrap_line) in self.text_wrapper.lines.iter().enumerate() {
            if ix == row {
                break;
            }

            row_offset_y += wrap_line.height(line_height);
        }

        if let Some(line) = last_layout
            .lines
            .get(row.saturating_sub(last_layout.visible_range.start))
        {
            // Check to scroll horizontally and soft wrap lines
            if let Some(pos) = line.position_for_index(point.column, line_height) {
                let bounds_width = bounds.size.width - last_layout.line_number_width;
                let col_offset_x = pos.x;
                row_offset_y += pos.y;
                // Columns of context kept beside the caret (#252, Zed's
                // `horizontal_scroll_margin`). Zero is the old behaviour.
                let margin = RIGHT_MARGIN
                    + f32::from(self.mode.scroll_margins().1) * line_height * 0.5;
                if col_offset_x - margin < -scroll_offset.x {
                    // If the position is out of the visible area, scroll to make it visible
                    scroll_offset.x = -col_offset_x + margin;
                } else if col_offset_x + margin > -scroll_offset.x + bounds_width {
                    scroll_offset.x = -(col_offset_x - bounds_width + margin);
                }
            }
        }

        // Check if row_offset_y is out of the viewport
        // If row offset is not in the viewport, scroll to make it visible
        // How many rows to keep either side of the caret. `OnMove` only
        // enforces them while the caret is being moved with the keyboard;
        // `autoscroll_on_clicks` extends that to a click placing the caret.
        let enforced = match self.mode.surrounding_lines_style() {
            SurroundingLinesStyle::Always => true,
            SurroundingLinesStyle::OnMove => {
                direction.is_some() || self.mode.autoscroll_on_clicks()
            }
        };
        let rows = usize::from(self.mode.cursor_surrounding_lines());
        let edge_height = if enforced && rows > 0 && self.mode.is_code_editor() {
            rows * line_height
        } else {
            line_height
        };
        if row_offset_y - edge_height + line_height < -scroll_offset.y {
            // Scroll up
            scroll_offset.y = -row_offset_y + edge_height - line_height;
        } else if row_offset_y + edge_height > -scroll_offset.y + bounds.size.height {
            // Scroll down
            scroll_offset.y = -(row_offset_y - bounds.size.height + edge_height);
        }

        // Avoid necessary scroll, when it was already in the correct position.
        if direction == Some(MoveDirection::Up) {
            scroll_offset.y = scroll_offset.y.max(was_offset.y);
        } else if direction == Some(MoveDirection::Down) {
            scroll_offset.y = scroll_offset.y.min(was_offset.y);
        }

        scroll_offset.x = scroll_offset.x.min(px(0.));
        scroll_offset.y = scroll_offset.y.min(px(0.));
        // A jump slides instead of cutting (#252). **Only a jump** -- the
        // wheel already carries the trackpad's own inertia, and interpolating
        // that on top makes it feel like mud.
        if self.mode.smooth_scrolling() && scroll_offset != was_offset {
            self.start_scroll_slide(was_offset, scroll_offset, cx);
            return;
        }
        self.deferred_scroll_offset = Some(scroll_offset);
        cx.notify();
    }

    /// Slide from `from` to `to` over [`SCROLL_SLIDE`] (#252).
    ///
    /// Driven by **this input's** `notify`, not the window's -- the editor is
    /// the only thing that has to be redrawn (ADR-0040 §5).
    fn start_scroll_slide(
        &mut self,
        from: Point<Pixels>,
        to: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        self.scroll_slide = Some((from, to, std::time::Instant::now()));
        self.deferred_scroll_offset = Some(from);
        cx.notify();
        let task = cx.spawn(async move |this, cx| {
            loop {
                gpui::Timer::after(std::time::Duration::from_millis(8)).await;
                let done = this
                    .update(cx, |this, cx| {
                        let Some((from, to, started)) = this.scroll_slide else {
                            return true;
                        };
                        let t = (started.elapsed().as_secs_f32()
                            / SCROLL_SLIDE.as_secs_f32())
                        .clamp(0., 1.);
                        // Ease out: fast at the start, settling at the end.
                        let e = 1. - (1. - t) * (1. - t);
                        let at = from + (to - from) * e;
                        this.deferred_scroll_offset = Some(at);
                        if t >= 1. {
                            this.scroll_slide = None;
                        }
                        cx.notify();
                        t >= 1.
                    })
                    .unwrap_or(true);
                if done {
                    break;
                }
            }
        });
        self.scroll_slide_task = Some(task);
    }

    pub(super) fn show_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    pub(super) fn copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.copy_range() else {
            return;
        };

        let selected_text = self.text.slice(range).to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(selected_text));
    }

    /// What copying takes: the selection, or the caret's whole row.
    ///
    /// `None` when there is nothing to copy -- an empty selection with
    /// `empty_selection_clipboard` off.
    fn copy_range(&self) -> Option<Range<usize>> {
        if !self.selected_range.is_empty() {
            return Some(self.selected_range.into());
        }
        if !self.mode.empty_selection_clipboard() {
            return None;
        }
        Some(self.caret_row_range())
    }

    /// The caret's row, **including the line break that ends it**.
    ///
    /// Not `line_end_offset`: that stops before the newline, and a row pasted
    /// without its break runs into whatever it lands on. Taking the next
    /// row's start instead also keeps a CRLF whole.
    fn caret_row_range(&self) -> Range<usize> {
        let row = self.text.offset_to_point(self.cursor()).row;
        let start = self.text.line_start_offset(row);
        let end = if row + 1 < self.text.lines_len() {
            self.text.line_start_offset(row + 1)
        } else {
            self.text.len()
        };
        start..end
    }

    pub(super) fn cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        let Some(range) = self.copy_range() else {
            return;
        };

        let selected_text = self.text.slice(range.clone()).to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(selected_text));

        // **`replace_text_in_range_silent(None, ..)` takes out whatever is
        // selected**, so the row has to be selected before it can be cut.
        if self.selected_range.is_empty() {
            self.selected_range = range.into();
        }
        self.replace_text_in_range_silent(None, "", window, cx);
    }

    pub(super) fn paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(clipboard) = cx.read_from_clipboard() {
            let mut new_text = clipboard.text().unwrap_or_default();
            if !self.mode.is_multi_line() {
                new_text = new_text.replace('\n', "");
            }

            // Re-indent the block to where it landed (#248). **Before the
            // edit** -- moving text after it is in would be a second undo step.
            if let Some(fixed) = self.reindent_paste(&new_text) {
                new_text = fixed;
            }

            self.replace_text_in_range_silent(None, &new_text, window, cx);
            self.scroll_to(self.cursor(), None, cx);

            let end = self.cursor();
            let start = end.saturating_sub(new_text.len());
            cx.emit(InputEvent::Pasted { range: start..end });
        }
    }

    fn push_history(&mut self, text: &Rope, range: &Range<usize>, new_text: &str) {
        if self.history.ignore {
            return;
        }

        let old_text = text.slice(range.clone()).to_string();
        let new_range = range.start..range.start + new_text.len();

        self.history
            .push(Change::new(range.clone(), &old_text, new_range, new_text));
    }

    pub(super) fn undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.undo() {
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.new_range.into());
                self.replace_text_in_range_silent(Some(range_utf16), &change.old_text, window, cx);
            }
        }
        self.history.ignore = false;
    }

    pub(super) fn redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.history.ignore = true;
        if let Some(changes) = self.history.redo() {
            for change in changes {
                let range_utf16 = self.range_to_utf16(&change.old_range.into());
                self.replace_text_in_range_silent(Some(range_utf16), &change.new_text, window, cx);
            }
        }
        self.history.ignore = false;
    }

    /// Get byte offset of the cursor.
    ///
    /// The offset is the UTF-8 offset.
    pub fn cursor(&self) -> usize {
        if let Some(ime_marked_range) = &self.ime_marked_range {
            return ime_marked_range.end;
        }

        if self.selection_reversed {
            self.selected_range.start
        } else {
            self.selected_range.end
        }
    }

    pub(crate) fn index_for_mouse_position(&self, position: Point<Pixels>) -> usize {
        // If the text is empty, always return 0
        if self.text.len() == 0 {
            return 0;
        }

        let (Some(bounds), Some(last_layout)) =
            (self.last_bounds.as_ref(), self.last_layout.as_ref())
        else {
            return 0;
        };

        let line_height = last_layout.line_height;
        let line_number_width = last_layout.line_number_width;

        // TIP: About the IBeam cursor
        //
        // If cursor style is IBeam, the mouse mouse position is in the middle of the cursor (This is special in OS)

        // The position is relative to the bounds of the text input
        //
        // bounds.origin:
        //
        // - included the input padding.
        // - included the scroll offset.
        let inner_position = position - bounds.origin - point(line_number_width, px(0.));

        let mut index = last_layout.visible_range_offset.start;
        let mut y_offset = last_layout.visible_top;
        for (ix, line) in self
            .text_wrapper
            .lines
            .iter()
            .skip(last_layout.visible_range.start)
            .enumerate()
        {
            let line_origin = self.line_origin_with_y_offset(&mut y_offset, line, line_height);
            let pos = inner_position - line_origin;

            let Some(line_layout) = last_layout.lines.get(ix) else {
                if pos.y < line_origin.y + line_height {
                    break;
                }

                continue;
            };

            // Return offset by use closest_index_for_x if is single line mode.
            if self.mode.is_single_line() {
                index = line_layout.closest_index_for_x(pos.x);
                break;
            }

            if let Some(v) = line_layout.closest_index_for_position(pos, line_height) {
                index += v;
                break;
            } else if pos.y < px(0.) {
                break;
            }

            // +1 for `\n`
            index += line_layout.len() + 1;
        }

        let index = if index > self.text.len() {
            self.text.len()
        } else {
            index
        };

        if self.masked {
            // When is masked, the index is char index, need convert to byte index.
            self.text.char_index_to_offset(index)
        } else {
            index
        }
    }

    /// Returns a y offsetted point for the line origin.
    fn line_origin_with_y_offset(
        &self,
        y_offset: &mut Pixels,
        line: &LineItem,
        line_height: Pixels,
    ) -> Point<Pixels> {
        // NOTE: About line.wrap_boundaries.len()
        //
        // If only 1 line, the value is 0
        // If have 2 line, the value is 1
        if self.mode.is_multi_line() {
            let p = point(px(0.), *y_offset);
            *y_offset += line.height(line_height);
            p
        } else {
            point(px(0.), px(0.))
        }
    }

    /// Select the text from the current cursor position to the given offset.
    ///
    /// The offset is the UTF-8 offset.
    ///
    /// Ensure the offset use self.next_boundary or self.previous_boundary to get the correct offset.
    pub(crate) fn select_to(&mut self, offset: usize, cx: &mut Context<Self>) {
        self.caret_moved_explicitly = true;
        self.clear_inline_completion(cx);

        let offset = offset.clamp(0, self.text.len());
        if self.selection_reversed {
            self.selected_range.start = offset
        } else {
            self.selected_range.end = offset
        };

        if self.selected_range.end < self.selected_range.start {
            self.selection_reversed = !self.selection_reversed;
            self.selected_range = (self.selected_range.end..self.selected_range.start).into();
        }

        // Ensure keep word selected range
        if let Some(word_range) = self.selected_word_range.as_ref() {
            if self.selected_range.start > word_range.start {
                self.selected_range.start = word_range.start;
            }
            if self.selected_range.end < word_range.end {
                self.selected_range.end = word_range.end;
            }
        }
        if self.selected_range.is_empty() {
            self.update_preferred_column();
        }
        cx.notify()
    }

    /// Unselects the currently selected text.
    pub fn unselect(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let offset = self.cursor();
        self.selected_range = (offset..offset).into();
        cx.notify()
    }

    #[inline]
    pub(super) fn offset_from_utf16(&self, offset: usize) -> usize {
        self.text.offset_utf16_to_offset(offset)
    }

    #[inline]
    pub(super) fn offset_to_utf16(&self, offset: usize) -> usize {
        self.text.offset_to_offset_utf16(offset)
    }

    #[inline]
    pub(super) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    #[inline]
    pub(super) fn range_from_utf16(&self, range_utf16: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range_utf16.start)..self.offset_from_utf16(range_utf16.end)
    }

    pub(super) fn previous_boundary(&self, offset: usize) -> usize {
        let mut offset = self.text.clip_offset(offset.saturating_sub(1), Bias::Left);
        if let Some(ch) = self.text.char_at(offset) {
            if ch == '\r' {
                offset -= 1;
            }
        }

        offset
    }

    pub(super) fn next_boundary(&self, offset: usize) -> usize {
        let mut offset = self.text.clip_offset(offset + 1, Bias::Right);
        if let Some(ch) = self.text.char_at(offset) {
            if ch == '\r' {
                offset += 1;
            }
        }

        offset
    }

    /// Where to draw the caret this frame, and whether a slide is still in
    /// flight (so the element knows to ask for another frame).
    ///
    /// `target` is in text coordinates, without the scroll offset -- the
    /// element adds that after. Interpolating in screen coordinates would
    /// make a plain scroll look like the caret slid across the pane.
    pub(crate) fn advance_caret_slide(
        &mut self,
        target: Bounds<Pixels>,
        line_height: Pixels,
    ) -> (Bounds<Pixels>, bool) {
        let animation = self.mode.caret_animation();
        if animation == CaretAnimation::Off {
            self.caret_slide = None;
            return (target, false);
        }

        // Where the caret is being drawn right now: part way through the
        // slide in flight, or wherever the last frame put it.
        let (drawn, aiming_at) = match self.caret_slide {
            Some(slide) => {
                let t = slide.start.elapsed().as_secs_f32()
                    / caret::SLIDE_DURATION.as_secs_f32();
                (caret::slide(slide.from, slide.to, t), Some(slide.to))
            }
            None => {
                // `last_layout` is still the previous frame's here; the
                // element writes this frame's at the end of `paint`.
                let last = self
                    .last_layout
                    .as_ref()
                    .and_then(|layout| layout.cursor_bounds);
                (last.unwrap_or(target), last)
            }
        };

        if aiming_at == Some(target) {
            // Still heading for the same place.
            let Some(slide) = self.caret_slide else {
                return (target, false);
            };
            let t =
                slide.start.elapsed().as_secs_f32() / caret::SLIDE_DURATION.as_secs_f32();
            if t >= 1. {
                self.caret_slide = None;
                return (target, false);
            }
            return (caret::slide(slide.from, slide.to, t), true);
        }

        // The caret has been asked to go somewhere new.
        let explicit_only = animation == CaretAnimation::Explicit;
        if (explicit_only && !self.caret_moved_explicitly)
            || !caret::should_slide(drawn, target, line_height)
        {
            self.caret_slide = None;
            return (target, false);
        }

        // Start from where it is now, so redirecting mid-slide does not jump.
        self.caret_slide = Some(CaretSlide {
            from: drawn,
            to: target,
            start: Instant::now(),
        });
        (drawn, true)
    }

    /// Returns the true to let InputElement to render cursor, when Input is focused and current BlinkCursor is visible.
    pub(crate) fn show_cursor(&self, window: &Window, cx: &App) -> bool {
        if !(self.focus_handle.is_focused(window) || self.is_context_menu_open(cx))
            || !window.is_window_active()
        {
            return false;
        }
        match self.mode.cursor_blinking() {
            // The 500ms timer owns the on/off.
            CursorBlinking::Blink => self.blink_cursor.read(cx).visible(),
            // Solid never hides, and the fades are shaped by the element from
            // `BlinkCursor::phase` -- hiding them here would fight it.
            _ => true,
        }
    }

    fn on_focus(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let blinking = self.mode.cursor_blinking();
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.start(blinking, cx);
        });
        cx.emit(InputEvent::Focus);
    }

    fn on_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.is_context_menu_open(cx) {
            return;
        }

        // NOTE: Do not cancel select, when blur.
        // Because maybe user want to copy the selected text by AppMenuBar (will take focus handle).

        self.hover_popover = None;
        self.diagnostic_popover = None;
        self.context_menu = None;
        self.clear_inline_completion(cx);
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.stop(cx);
        });
        Root::update(window, cx, |root, _, _| {
            root.focused_input = None;
        });
        cx.emit(InputEvent::Blur);
        cx.notify();
    }

    pub(super) fn pause_blink_cursor(&mut self, cx: &mut Context<Self>) {
        self.blink_cursor.update(cx, |cursor, cx| {
            cursor.pause(cx);
        });
    }

    pub(super) fn on_key_down(&mut self, _: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.pause_blink_cursor(cx);
    }

    pub(super) fn on_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.text.len() == 0 {
            return;
        }

        if self.last_layout.is_none() {
            return;
        }

        if !self.focus_handle.is_focused(window) {
            return;
        }

        if !self.selecting {
            return;
        }

        let offset = self.index_for_mouse_position(event.position);
        self.select_to(offset, cx);
    }

    fn is_valid_input(&self, new_text: &str, cx: &mut Context<Self>) -> bool {
        if new_text.is_empty() {
            return true;
        }

        if let Some(validate) = &self.validate {
            if !validate(new_text, cx) {
                return false;
            }
        }

        if !self.mask_pattern.is_valid(new_text) {
            return false;
        }

        let Some(pattern) = &self.pattern else {
            return true;
        };

        pattern.is_match(new_text)
    }

    /// Set the mask pattern for formatting the input text.
    ///
    /// The pattern can contain:
    /// - 9: Any digit or dot
    /// - A: Any letter
    /// - *: Any character
    /// - Other characters will be treated as literal mask characters
    ///
    /// Example: "(999)999-999" for phone numbers
    pub fn mask_pattern(mut self, pattern: impl Into<MaskPattern>) -> Self {
        self.mask_pattern = pattern.into();
        if let Some(placeholder) = self.mask_pattern.placeholder() {
            self.placeholder = placeholder.into();
        }
        self
    }

    pub fn set_mask_pattern(
        &mut self,
        pattern: impl Into<MaskPattern>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mask_pattern = pattern.into();
        if let Some(placeholder) = self.mask_pattern.placeholder() {
            self.placeholder = placeholder.into();
        }
        cx.notify();
    }

    pub(super) fn set_input_bounds(&mut self, new_bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        let wrap_width_changed = self.input_bounds.size.width != new_bounds.size.width;
        self.input_bounds = new_bounds;

        // Update text_wrapper wrap_width if changed.
        if let Some(last_layout) = self.last_layout.as_ref() {
            if wrap_width_changed {
                let wrap_width = if !self.soft_wrap {
                    // None to disable wrapping (will use Pixels::MAX)
                    None
                } else {
                    last_layout.wrap_width
                };

                self.text_wrapper.set_wrap_width(wrap_width, cx);
                self.mode.update_auto_grow(&self.text_wrapper);
                cx.notify();
            }
        }
    }

    pub(super) fn selected_text(&self) -> RopeSlice<'_> {
        let range_utf16 = self.range_to_utf16(&self.selected_range.into());
        let range = self.range_from_utf16(&range_utf16);
        self.text.slice(range)
    }

    pub(crate) fn range_to_bounds(&self, range: &Range<usize>) -> Option<Bounds<Pixels>> {
        let Some(last_layout) = self.last_layout.as_ref() else {
            return None;
        };

        let Some(last_bounds) = self.last_bounds else {
            return None;
        };

        let (_, _, start_pos) = self.line_and_position_for_offset(range.start);
        let (_, _, end_pos) = self.line_and_position_for_offset(range.end);

        let Some(start_pos) = start_pos else {
            return None;
        };
        let Some(end_pos) = end_pos else {
            return None;
        };

        Some(Bounds::from_corners(
            last_bounds.origin + start_pos,
            last_bounds.origin + end_pos + point(px(0.), last_layout.line_height),
        ))
    }

    /// Replace text by [`lsp_types::Range`].
    ///
    /// See also: [`EntityInputHandler::replace_text_in_range`]
    #[allow(unused)]
    pub(crate) fn replace_text_in_lsp_range(
        &mut self,
        lsp_range: &lsp_types::Range,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let start = self.text.position_to_offset(&lsp_range.start);
        let end = self.text.position_to_offset(&lsp_range.end);
        self.replace_text_in_range_silent(
            Some(self.range_to_utf16(&(start..end))),
            new_text,
            window,
            cx,
        );
    }

    /// Replace text in range in silent.
    ///
    /// This will not trigger any UI interaction, such as auto-completion.
    pub(crate) fn replace_text_in_range_silent(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.silent_replace_text = true;
        self.replace_text_in_range(range_utf16, new_text, window, cx);
        self.silent_replace_text = false;
    }
}

impl EntityInputHandler for InputState {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.range_from_utf16(&range_utf16);
        adjusted_range.replace(self.range_to_utf16(&range));
        Some(self.text.slice(range).to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.range_to_utf16(&self.selected_range.into()),
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.ime_marked_range
            .map(|range| self.range_to_utf16(&range.into()))
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {
        self.ime_marked_range = None;
    }

    /// Replace text in range.
    ///
    /// - If the new text is invalid, it will not be replaced.
    /// - If `range_utf16` is not provided, the current selected range will be used.
    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }

        // The caret is about to be pushed along by an edit, not moved on
        // purpose. `CaretAnimation::Explicit` does not slide for this.
        self.caret_moved_explicitly = false;
        self.pause_blink_cursor(cx);

        // ── a closing bracket pulls its own line back ──────────────────────
        // `AutoIndent::Brackets`. Done first and as its own edit, so the
        // bracket itself still goes through auto-closing below.
        if range_utf16.is_none() && self.selected_range.is_empty() {
            if let Some(drop) = self.outdent_for_closer(new_text) {
                self.text.replace(drop.clone(), "");
                self.selected_range = (drop.start..drop.start).into();
            }
        }

        // ── auto-closing brackets ──────────────────────────────────────────
        // Decided before anything is written, because the whole point is to
        // change *what* gets written (and where the caret lands afterwards).
        let auto = self.auto_close_edit(range_utf16.as_ref(), new_text);
        if let Some(AutoCloseEdit::Overtype { to }) = auto {
            self.selected_range = (to..to).into();
            self.update_preferred_column();
            cx.notify();
            return;
        }
        let auto_text = auto.as_ref().map(|a| a.text().to_string());
        let new_text = auto_text.as_deref().unwrap_or(new_text);

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.ime_marked_range.map(|range| {
                let range = self.range_to_utf16(&(range.start..range.end));
                self.range_from_utf16(&range)
            }))
            .unwrap_or(self.selected_range.into());

        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        let mut new_offset = (range.start + new_text.len()).min(self.text.len());

        if self.mode.is_single_line() {
            let pending_text = self.text.to_string();
            // Check if the new text is valid
            if !self.is_valid_input(&pending_text, cx) {
                self.text = old_text;
                return;
            }

            if !self.mask_pattern.is_none() {
                let mask_text = self.mask_pattern.mask(&pending_text);
                self.text = Rope::from(mask_text.as_str());
                let new_text_len =
                    (new_text.len() + mask_text.len()).saturating_sub(pending_text.len());
                new_offset = (range.start + new_text_len).min(mask_text.len());
            }
        }

        self.push_history(&old_text, &range, &new_text);
        self.history.end_grouping();
        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        self.text_wrapper
            .update(&self.text, &range, &Rope::from(new_text), cx);
        // Folds are shifted, not dropped -- losing them on every keystroke
        // would be worse than not having the feature. `update_folds` then drops
        // any header that no longer heads a fold.
        self.shift_folds_for_edit(&old_text, &range);
        self.update_folds();
        self.mode
            .update_highlighter(&range, &self.text, &new_text, true, cx);
        self.lsp.update(&self.text, window, cx);
        self.selected_range = (new_offset..new_offset).into();
        // Put the caret back inside the pair, or keep the wrapped text selected.
        match &auto {
            Some(AutoCloseEdit::Insert { caret_back, .. }) => {
                let offset = new_offset.saturating_sub(*caret_back);
                self.selected_range = (offset..offset).into();
            }
            Some(AutoCloseEdit::Surround {
                open_len,
                inner_len,
                ..
            }) => {
                let start = range.start + open_len;
                self.selected_range = (start..start + inner_len).into();
            }
            _ => {}
        }
        self.ime_marked_range.take();
        self.update_preferred_column();
        self.update_search(cx);
        self.mode.update_auto_grow(&self.text_wrapper);
        if !self.silent_replace_text {
            self.handle_completion_trigger(&range, &new_text, window, cx);
            // Parameter hints follow the brackets (#246).
            //
            // **Look at what the text holds, not at its last character.**
            // Auto-closing turns a typed `(` into `()`, so the last character
            // is a `)` and the hints were taken away the instant they were
            // asked for.
            if new_text.contains('(') || new_text.contains(',') {
                self.request_signature_help(None, window, cx);
            } else if new_text.chars().next_back() == Some(')') {
                self.hide_signature_help(cx);
            } else if self.mode.parameter_hints_behaviour().1 {
                self.request_signature_help(None, window, cx);
            }
        }
        cx.emit(InputEvent::Change);
        cx.notify();
    }

    /// Mark text is the IME temporary insert on typing.
    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.disabled {
            return;
        }

        self.lsp.reset();

        let range = range_utf16
            .as_ref()
            .map(|range_utf16| self.range_from_utf16(range_utf16))
            .or(self.ime_marked_range.map(|range| {
                let range = self.range_to_utf16(&(range.start..range.end));
                self.range_from_utf16(&range)
            }))
            .unwrap_or(self.selected_range.into());

        let old_text = self.text.clone();
        self.text.replace(range.clone(), new_text);

        if self.mode.is_single_line() {
            let pending_text = self.text.to_string();
            if !self.is_valid_input(&pending_text, cx) {
                self.text = old_text;
                return;
            }
        }

        if let Some(diagnostics) = self.mode.diagnostics_mut() {
            diagnostics.reset(&self.text)
        }
        self.text_wrapper
            .update(&self.text, &range, &Rope::from(new_text), cx);
        self.shift_folds_for_edit(&old_text, &range);
        self.update_folds();
        self.mode
            .update_highlighter(&range, &self.text, &new_text, true, cx);
        self.lsp.update(&self.text, window, cx);
        if new_text.is_empty() {
            // Cancel selection, when cancel IME input.
            self.selected_range = (range.start..range.start).into();
            self.ime_marked_range = None;
        } else {
            self.ime_marked_range = Some((range.start..range.start + new_text.len()).into());
            self.selected_range = new_selected_range_utf16
                .as_ref()
                .map(|range_utf16| self.range_from_utf16(range_utf16))
                .map(|new_range| new_range.start + range.start..new_range.end + range.end)
                .unwrap_or_else(|| range.start + new_text.len()..range.start + new_text.len())
                .into();
        }
        self.mode.update_auto_grow(&self.text_wrapper);
        self.history.start_grouping();
        self.push_history(&old_text, &range, new_text);
        // The IME path mutates `self.text` just like `replace_text_in_range` does,
        // so it has to report the change as well. Without this, an app that
        // tracks its "unsaved" state through `InputEvent::Change` never sees the
        // text typed while an IME is active -- and with a CJK IME switched on,
        // even plain ASCII goes through here.
        //
        // The check is deliberately structural rather than `old_text != self.text`:
        // comparing two ropes is O(n) and this runs on every keystroke.
        if !range.is_empty() || !new_text.is_empty() {
            cx.emit(InputEvent::Change);
        }
        cx.notify();
    }

    /// Used to position IME candidates.
    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let last_layout = self.last_layout.as_ref()?;
        let line_height = last_layout.line_height;
        let line_number_width = last_layout.line_number_width;
        let range = self.range_from_utf16(&range_utf16);

        let mut start_origin = None;
        let mut end_origin = None;
        let line_number_origin = point(line_number_width, px(0.));
        let mut y_offset = last_layout.visible_top;
        let mut index_offset = last_layout.visible_range_offset.start;

        for line in last_layout.lines.iter() {
            if start_origin.is_some() && end_origin.is_some() {
                break;
            }

            if start_origin.is_none() {
                if let Some(p) =
                    line.position_for_index(range.start.saturating_sub(index_offset), line_height)
                {
                    start_origin = Some(p + point(px(0.), y_offset));
                }
            }

            if end_origin.is_none() {
                if let Some(p) =
                    line.position_for_index(range.end.saturating_sub(index_offset), line_height)
                {
                    end_origin = Some(p + point(px(0.), y_offset));
                }
            }

            index_offset += line.len() + 1;
            y_offset += line.size(line_height).height;
        }

        let start_origin = start_origin.unwrap_or_default();
        let mut end_origin = end_origin.unwrap_or_default();
        // Ensure at same line.
        end_origin.y = start_origin.y;

        Some(Bounds::from_corners(
            bounds.origin + line_number_origin + start_origin,
            // + line_height for show IME panel under the cursor line.
            bounds.origin + line_number_origin + point(end_origin.x, end_origin.y + line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        let last_layout = self.last_layout.as_ref()?;
        let line_height = last_layout.line_height;
        let line_point = self.last_bounds?.localize(&point)?;
        let offset = last_layout.visible_range_offset.start;

        for line in last_layout.lines.iter() {
            if let Some(utf8_index) = line.index_for_position(line_point, line_height) {
                return Some(self.offset_to_utf16(offset + utf8_index));
            }
        }

        None
    }
}

impl Focusable for InputState {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InputState {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self._pending_update {
            self.mode
                .update_highlighter(&(0..0), &self.text, "", false, cx);
            self.lsp.update(&self.text, window, cx);
            self._pending_update = false;
        }

        if self.pending_highlight {
            self.pending_highlight = false;
            let text = self.text.clone();
            let offset = self.cursor();
            self.lsp
                .update_document_highlights(&text, offset, window, cx);
        }

        div()
            .id("input-state")
            .flex_1()
            .when(self.mode.is_multi_line(), |this| this.h_full())
            .flex_grow()
            .overflow_x_hidden()
            .child(TextElement::new(cx.entity().clone()).placeholder(self.placeholder.clone()))
            .children(self.diagnostic_popover.clone())
            .children(self.context_menu.as_ref().map(|menu| menu.render()))
            .children(self.hover_popover.clone())
            .children(self.signature_popover.clone())
    }
}
