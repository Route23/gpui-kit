use aho_corasick::AhoCorasick;
use regex::{Regex, RegexBuilder};
use rust_i18n::t;
use std::{ops::Range, rc::Rc, time::Duration};

use gpui::{
    App, AppContext as _, Context, Empty, Entity, FocusHandle, Focusable, Half,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement as _, Pixels, Render, Styled,
    Subscription, Task, Window, actions, canvas, div, prelude::FluentBuilder as _,
};
use ropey::Rope;

use crate::{
    ActiveTheme, Disableable, IconName, Selectable, Sizable,
    actions::SelectUp,
    button::{Button, ButtonVariants},
    h_flex,
    input::{
        Enter, Escape, IndentInline, Input, InputEvent, InputState, RopeExt as _, Search,
        movement::MoveDirection,
    },
    label::Label,
    v_flex,
};

const CONTEXT: &'static str = "SearchPanel";

actions!(input, [Tab]);

pub(super) fn init(cx: &mut App) {
    cx.bind_keys(vec![KeyBinding::new(
        "shift-enter",
        SelectUp,
        Some(CONTEXT),
    )]);
}

/// How to look for the query.
///
/// One struct rather than a widening list of booleans on `update_query` --
/// every caller has to make the same four decisions, and a struct keeps them
/// named at the call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchOptions {
    /// Tell `Foo` from `foo`. Beats `smart_case`.
    pub case_sensitive: bool,
    /// Only count a hit when a word starts and ends there.
    pub whole_word: bool,
    /// Read the query as a regular expression.
    pub regex: bool,
    /// An all-lowercase query ignores case; one capital makes it matter.
    /// vim, ripgrep and VS Code all settled on this, so it needs no explaining.
    pub smart_case: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            case_sensitive: false,
            whole_word: false,
            regex: false,
            smart_case: true,
        }
    }
}

impl SearchOptions {
    /// Does case matter for this query?
    ///
    /// `smart_case` only speaks when the reader has not asked for case to
    /// matter -- an explicit ask always wins.
    fn case_matters(&self, query: &str) -> bool {
        if self.case_sensitive {
            return true;
        }
        self.smart_case && query.chars().any(char::is_uppercase)
    }
}

/// How the panel behaves once something has been found.
///
/// Kept apart from [`SearchOptions`] on purpose. That one says **how to
/// look** -- every field of it feeds the pattern or filters a hit. These say
/// what happens *after*, and six of the seven never reach the matcher at all.
/// Folding them in would rebuild the Aho-Corasick automaton because somebody
/// toggled "close the panel on a result".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchBehavior {
    /// Look again on every keystroke. Off means the query only runs on Enter.
    pub search_on_type: bool,
    /// How long to wait after a keystroke before looking, in milliseconds.
    /// Zero looks straight away, which is what it did before this existed.
    pub search_debounce_ms: u16,
    /// Move the caret to the current match while the reader is still typing.
    pub cursor_move_on_type: bool,
    /// Close the panel once a match has been stepped to.
    pub close_on_result: bool,
    /// Put the match in the middle of the viewport rather than merely
    /// bringing it into view.
    pub center_on_match: bool,
    /// Go back to the first match after the last one, and the other way
    /// round. Off stops at either end.
    ///
    /// Named `wrap_around` because `wrap_at` and `soft_wrap` already mean
    /// something else a few fields away on [`super::InputMode`].
    pub wrap_around: bool,
    /// With nothing selected, seed the query with the word under the caret.
    ///
    /// Read together with `search_seed_from_selection`, which is the master
    /// switch: no seeding at all means no seeding from a word either.
    pub seed_from_word: bool,
}

impl Default for SearchBehavior {
    fn default() -> Self {
        Self {
            search_on_type: true,
            search_debounce_ms: 0,
            // The one deliberate departure from what it did before: every
            // other editor puts the caret on the match, and `hide()` hands
            // focus back to the editor -- so without this, Escape throws the
            // find away and drops the reader where they started.
            cursor_move_on_type: true,
            close_on_result: false,
            center_on_match: false,
            wrap_around: true,
            seed_from_word: false,
        }
    }
}

/// A built query. Literal until the reader asks for a regular expression.
#[derive(Debug, Clone)]
enum Pattern {
    Literal(AhoCorasick),
    Regex(Regex),
}

#[derive(Debug, Clone)]
pub struct SearchMatcher {
    text: Rope,
    query: Option<Pattern>,
    options: SearchOptions,

    pub(super) matched_ranges: Rc<Vec<Range<usize>>>,
    pub(super) current_match_ix: usize,
    /// The query as last built, so an identical one can be waved through.
    ///
    /// Load-bearing: `update_matches` puts `current_match_ix` back to zero,
    /// so re-running the same query on every Enter would land on match one
    /// for ever and never advance.
    query_text: String,
    /// Go back to the first match after the last one. See
    /// [`SearchBehavior::wrap_around`].
    wrap: bool,
    /// Is in replacing mode, if true, the next update will not reset the current match index.
    replacing: bool,
    /// The reader typed a regular expression that does not parse.
    ///
    /// Kept so the panel can say so; matching simply finds nothing. Building
    /// it must never panic -- the query comes straight from a text field.
    invalid: bool,
}

/// Is this part of a word?
///
/// `alphanumeric` or `_`, matching `RopeExt::word_range` (what hover and
/// go-to-definition point at) so that "whole word" means the same thing
/// everywhere in the editor.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Does a word start at `range.start` and end at `range.end`?
fn is_whole_word(text: &str, range: &Range<usize>) -> bool {
    let before = text[..range.start].chars().next_back();
    let after = text[range.end..].chars().next();
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

impl SearchMatcher {
    pub fn new() -> Self {
        Self {
            text: "".into(),
            query: None,
            options: SearchOptions::default(),
            matched_ranges: Rc::new(Vec::new()),
            current_match_ix: 0,
            query_text: String::new(),
            wrap: true,
            replacing: false,
            invalid: false,
        }
    }

    /// Update source text and re-match
    pub(crate) fn update(&mut self, text: &Rope) {
        if self.text.eq(text) {
            return;
        }

        self.text = text.clone();
        self.update_matches();
    }

    fn update_matches(&mut self) {
        let mut new_ranges: Vec<Range<usize>> = Vec::new();
        if let Some(query) = &self.query {
            let text = self.text.to_string();
            match query {
                Pattern::Literal(ac) => {
                    // FIXME: Use stream find
                    for query_match in ac.stream_find_iter(text.as_bytes()) {
                        let query_match =
                            query_match.expect("query match for select all action");
                        new_ranges.push(query_match.range());
                    }
                }
                // A regular expression can match nothing at all (`a*`); an
                // empty hit would light up a zero-width box and never advance.
                Pattern::Regex(re) => new_ranges.extend(
                    re.find_iter(&text)
                        .map(|m| m.range())
                        .filter(|r| r.start < r.end),
                ),
            }
            if self.options.whole_word {
                new_ranges.retain(|range| is_whole_word(&text, range));
            }
        }
        self.matched_ranges = Rc::new(new_ranges);
        if !self.replacing {
            self.current_match_ix = 0;
            self.replacing = false;
        }
    }

    /// Update the search query and reset the current match index.
    ///
    /// **An unchanged query is waved through.** Rebuilding it would put
    /// `current_match_ix` back to zero, and `next`/`prev` re-run the query
    /// before stepping (they have to, in case a debounce is still pending or
    /// `search_on_type` is off) -- so without this guard, Enter would land on
    /// match one for ever.
    pub fn update_query(&mut self, query: &str, options: SearchOptions) {
        if self.query_text == query && self.options == options {
            return;
        }
        self.query_text = query.to_string();
        self.options = options;
        self.invalid = false;
        let case_matters = options.case_matters(query);

        self.query = if query.is_empty() {
            None
        } else if options.regex {
            // A half-typed expression (`fn (`) is the normal state of a text
            // field, not a fault: find nothing and say so, never panic.
            match RegexBuilder::new(query)
                .case_insensitive(!case_matters)
                .build()
            {
                Ok(re) => Some(Pattern::Regex(re)),
                Err(_) => {
                    self.invalid = true;
                    None
                }
            }
        } else {
            AhoCorasick::builder()
                .ascii_case_insensitive(!case_matters)
                .build([query])
                .ok()
                .map(Pattern::Literal)
        };
        self.update_matches();
    }

    /// The reader typed a regular expression that does not parse.
    pub fn is_invalid(&self) -> bool {
        self.invalid
    }

    /// Returns the number of matches found.
    #[allow(unused)]
    #[inline]
    fn len(&self) -> usize {
        self.matched_ranges.len()
    }

    /// Go back to the first match after the last one, or stop there.
    ///
    /// **Does not look again.** This is a knob about how to *walk* the
    /// matches, not how to find them -- throwing `matched_ranges` away here
    /// would be a bug.
    pub fn set_wrap(&mut self, wrap: bool) {
        self.wrap = wrap;
    }

    /// The match the panel is on right now, without stepping.
    fn current(&self) -> Option<Range<usize>> {
        self.matched_ranges.get(self.current_match_ix).cloned()
    }

    fn peek(&self) -> Option<Range<usize>> {
        self.matched_ranges.get(self.current_match_ix + 1).cloned()
    }

    fn label(&self) -> String {
        if self.len() == 0 {
            return "0/0".to_string();
        }
        format!("{}/{}", self.current_match_ix + 1, self.len())
    }

    /// Update the current match index based on the given offset.
    fn update_cursor_by_offset(&mut self, offset: usize) {
        for (ix, range) in self.matched_ranges.iter().enumerate() {
            self.current_match_ix = ix;
            if range.contains(&offset) || range.end >= offset {
                return;
            }
        }
    }
}

impl Iterator for SearchMatcher {
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.matched_ranges.is_empty() {
            return None;
        }

        // **Look at the edge before moving.** Returning `None` has to leave
        // the index where it was, or a refusal still walks the reader off the
        // end of the list.
        if self.current_match_ix < self.matched_ranges.len().saturating_sub(1) {
            self.current_match_ix += 1;
        } else if self.wrap {
            self.current_match_ix = 0;
        } else {
            return None;
        }

        self.matched_ranges.get(self.current_match_ix).cloned()
    }
}

impl DoubleEndedIterator for SearchMatcher {
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.matched_ranges.is_empty() {
            return None;
        }

        if self.current_match_ix == 0 {
            if !self.wrap {
                return None;
            }
            self.current_match_ix = self.matched_ranges.len();
        }

        self.current_match_ix -= 1;
        let item = self.matched_ranges[self.current_match_ix].clone();

        Some(item)
    }
}

pub(super) struct SearchPanel {
    editor: Entity<InputState>,
    search_input: Entity<InputState>,
    replace_input: Entity<InputState>,
    options: SearchOptions,
    /// The reader pressed `Aa`. From then on smart case keeps quiet -- an
    /// explicit ask outranks a guess, for as long as the panel is open.
    case_decided: bool,
    replace_mode: bool,
    matcher: SearchMatcher,
    input_width: Pixels,
    /// How the panel behaves once something has been found. Re-read from the
    /// settings every time the panel opens, like `options` beside it.
    behavior: SearchBehavior,
    /// The pending debounced look-up.
    ///
    /// **Assigning over it is the cancellation** -- the old `Task` drops and
    /// its future is never polled again (`lsp/hover.rs` does the same).
    _query_task: Task<()>,

    open: bool,
    _subscriptions: Vec<Subscription>,
}

impl InputState {
    /// Update the search matcher when text changes.
    pub(super) fn update_search(&mut self, cx: &mut App) {
        let Some(search_panel) = self.search_panel.as_ref() else {
            return;
        };

        let text = self.text.clone();
        search_panel.update(cx, |this, _| {
            this.matcher.update(&text);
        });
    }

    pub(super) fn on_action_search(
        &mut self,
        _: &Search,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.searchable {
            return;
        }

        let options = self.mode.search_options();
        let behavior = self.mode.search_behavior();
        let seed = self.mode.search_seed_from_selection();
        let search_panel = match self.search_panel.as_ref() {
            Some(panel) => panel.clone(),
            None => SearchPanel::new(cx.entity(), options, behavior, window, cx),
        };

        let text = self.text.clone();
        let editor = cx.entity();
        // A selection wins; the word under the caret is the fallback. Note
        // `word_at` gives back `""` when the caret is not on a word, which
        // `show`'s own "is there anything to seed with" check already covers.
        let selected = Rope::from(self.selected_text());
        let seed_text = if selected.len() > 0 {
            selected
        } else if behavior.seed_from_word {
            Rope::from(self.text.word_at(self.cursor()))
        } else {
            Rope::from("")
        };
        search_panel.update(cx, |this, cx| {
            this.editor = editor;
            // **Settings decide how the panel opens, every time.** A panel
            // that stayed on the last session's toggles would quietly
            // disagree with what the settings window shows.
            this.options = options;
            this.behavior = behavior;
            this.matcher.set_wrap(behavior.wrap_around);
            this.case_decided = false;
            this.matcher.update(&text);
            this.show(&seed_text, seed, window, cx);
        });
        self.search_panel = Some(search_panel);
        cx.notify();
    }
}

impl SearchPanel {
    pub fn new(
        editor: Entity<InputState>,
        options: SearchOptions,
        behavior: SearchBehavior,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let search_input = cx.new(|cx| InputState::new(window, cx));
        let replace_input = cx.new(|cx| InputState::new(window, cx));

        cx.new(|cx| {
            let _subscriptions =
                vec![
                    cx.subscribe(&search_input, |this: &mut Self, _, ev: &InputEvent, cx| {
                        // Handle search input changes
                        match ev {
                            InputEvent::Change if this.behavior.search_on_type => {
                                this.schedule_search_query(cx);
                            }
                            _ => {}
                        }
                    }),
                ];

            Self {
                editor,
                search_input,
                replace_input,
                options,
                case_decided: false,
                replace_mode: false,
                matcher: SearchMatcher::new(),
                behavior,
                _query_task: Task::ready(()),
                open: true,
                input_width: Pixels::ZERO,
                _subscriptions,
            }
        })
    }

    pub(super) fn show(
        &mut self,
        selected_text: &Rope,
        seed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open = true;
        self.search_input.read(cx).focus_handle.focus(window);

        self.search_input.update(cx, |this, cx| {
            if seed && selected_text.len() > 0 {
                // Set value will emit to update_search_query
                this.set_value(selected_text.to_string(), window, cx);
            }
            this.select_all(&super::SelectAll, window, cx);
        });
        // **Run it here too.** The `Change` above is swallowed when
        // `search_on_type` is off, and merely scheduled when a debounce is
        // set -- neither of which should stop a freshly opened panel from
        // showing what it was seeded with. Free when neither knob is set:
        // `update_query` waves an unchanged query through.
        self._query_task = Task::ready(());
        self.update_search_query(cx);
    }

    /// Look again, now or after the debounce.
    fn schedule_search_query(&mut self, cx: &mut Context<Self>) {
        let wait = Duration::from_millis(u64::from(self.behavior.search_debounce_ms));
        if wait.is_zero() {
            // What it did before this existed: look on the keystroke.
            self._query_task = Task::ready(());
            self.update_search_query(cx);
            return;
        }
        self._query_task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(wait).await;
            let _ = this.update(cx, |this: &mut Self, cx| this.update_search_query(cx));
        });
    }

    /// Bring the match list up to date before stepping, and say whether that
    /// built a **new** list.
    ///
    /// `next`/`prev` cannot assume the list matches what is in the query
    /// field: `search_on_type` may be off, or a debounce may still be
    /// pending. When the list is new the panel must land **on** the first
    /// match rather than stepping past it.
    fn run_query_if_stale(&mut self, cx: &mut Context<Self>) -> bool {
        let query = self.search_input.read(cx).value();
        if self.matcher.query_text == query.as_str() {
            return false;
        }
        self._query_task = Task::ready(());
        self.update_search_query(cx);
        true
    }

    /// Put the caret on a match and bring it into view.
    ///
    /// **The one place a match becomes a selection.** `move_to` + `select_to`
    /// is the same pair `go_to_definition` uses, and neither takes focus --
    /// which matters, because the reader is typing in the search field.
    fn reveal(
        &mut self,
        range: &Range<usize>,
        direction: Option<MoveDirection>,
        cx: &mut Context<Self>,
    ) {
        let center = self.behavior.center_on_match;
        let start = range.start;
        let end = range.end;
        self.editor.update(cx, |state, cx| {
            state.move_to(start, direction, cx);
            state.select_to(end, cx);
            if center {
                let row = state.text().offset_to_point(start).row;
                state.scroll_row_to_center(row, cx);
            }
        });
    }

    fn update_search_query(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value();
        // Smart case is a guess about what the reader meant; once they have
        // pressed `Aa` the guess is no longer wanted.
        self.options.smart_case = self.options.smart_case && !self.case_decided;
        let visible_range_offset = self
            .editor
            .read(cx)
            .last_layout
            .as_ref()
            .map(|l| l.visible_range_offset.clone());

        self.matcher.update_query(query.as_str(), self.options);

        if let Some(visible_range_offset) = visible_range_offset {
            self.matcher
                .update_cursor_by_offset(visible_range_offset.start);
        }
        if self.behavior.cursor_move_on_type {
            if let Some(range) = self.matcher.current() {
                self.reveal(&range, None, cx);
            }
        }
        cx.notify();
    }

    pub(super) fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A debounce still in flight must not look again after the panel is
        // gone -- the highlights would have nowhere to go.
        self._query_task = Task::ready(());
        self.open = false;
        self.editor.read(cx).focus_handle.focus(window);
        cx.notify();
    }

    fn on_action_prev(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.prev(window, cx);
    }

    fn on_action_next(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        self.next(window, cx);
    }

    fn on_action_escape(&mut self, _: &Escape, window: &mut Window, cx: &mut Context<Self>) {
        self.hide(window, cx);
    }

    fn on_action_tab(&mut self, _: &IndentInline, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.focus_handle(cx).focus(window);
    }

    fn prev(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fresh = self.run_query_if_stale(cx);
        let range = if fresh {
            self.matcher.current()
        } else {
            self.matcher.next_back()
        };
        self.step_to(range, MoveDirection::Up, window, cx);
    }

    fn next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let fresh = self.run_query_if_stale(cx);
        let range = if fresh {
            self.matcher.current()
        } else {
            self.matcher.next()
        };
        self.step_to(range, MoveDirection::Down, window, cx);
    }

    /// Land on a match, if there was one to land on.
    fn step_to(
        &mut self,
        range: Option<Range<usize>>,
        direction: MoveDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(range) = range else {
            // Nothing to go to -- at either end with `wrap_around` off, or no
            // matches at all. Leave the panel exactly as it was.
            return;
        };
        self.reveal(&range, Some(direction), cx);
        if self.behavior.close_on_result {
            self.hide(window, cx);
        }
    }

    pub(super) fn matcher(&self) -> Option<&SearchMatcher> {
        if !self.open {
            return None;
        }

        Some(&self.matcher)
    }

    fn replace_next(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new_text = self.replace_input.read(cx).value();
        self.matcher.replacing = true;
        if let Some(range) = self
            .matcher
            .matched_ranges
            .get(self.matcher.current_match_ix)
            .cloned()
        {
            let text_state = self.editor.clone();

            let next_range = self.matcher.peek().unwrap_or(range.clone());
            cx.spawn_in(window, async move |_, cx| {
                cx.update(|window, cx| {
                    text_state.update(cx, |state, cx| {
                        let range_utf16 = state.range_to_utf16(&range);
                        state.scroll_to(next_range.end, Some(MoveDirection::Down), cx);
                        state.replace_text_in_range_silent(
                            Some(range_utf16),
                            new_text.as_str(),
                            window,
                            cx,
                        );
                    });
                })
            })
            .detach();
        }
    }

    fn replace_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let new_text = self.replace_input.read(cx).value();
        self.matcher.replacing = true;
        let ranges = self.matcher.matched_ranges.clone();
        if ranges.is_empty() {
            return;
        }

        let editor = self.editor.clone();
        cx.spawn_in(window, async move |_, cx| {
            cx.update(|window, cx| {
                editor.update(cx, |state, cx| {
                    // Replace from the end to avoid messing up the ranges.
                    let mut rope = state.text.clone();
                    for range in ranges.iter().rev() {
                        rope.replace(range.clone(), new_text.as_str());
                    }
                    state.replace_text_in_range_silent(
                        Some(0..state.text.len()),
                        &rope.to_string(),
                        window,
                        cx,
                    );
                    state.scroll_to(0, Some(MoveDirection::Down), cx);
                });
            })
        })
        .detach();
    }
}

impl Focusable for SearchPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search_input.read(cx).focus_handle.clone()
    }
}

impl Render for SearchPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return Empty.into_any_element();
        }

        let has_matches = self.matcher.len() > 0;

        v_flex()
            .id("search-panel")
            .occlude()
            .track_focus(&self.focus_handle(cx))
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_action_prev))
            .on_action(cx.listener(Self::on_action_next))
            .on_action(cx.listener(Self::on_action_escape))
            .on_action(cx.listener(Self::on_action_tab))
            .font_family(cx.theme().font_family.clone())
            .items_center()
            .py_2()
            .px_3()
            .w_full()
            .gap_1()
            .bg(cx.theme().popover)
            .border_b_1()
            .rounded(cx.theme().radius.half())
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .gap_1()
                            .child(
                                Input::new(&self.search_input)
                                    .focus_bordered(false)
                                    .suffix(
                                        h_flex()
                                            .gap_0p5()
                                            .child(
                                                Button::new("case-sensitive")
                                                    .selected(self.options.case_sensitive)
                                                    .xsmall()
                                                    .compact()
                                                    .ghost()
                                                    .icon(IconName::CaseSensitive)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.options.case_sensitive =
                                                            !this.options.case_sensitive;
                                                        // The reader has spoken; stop guessing.
                                                        this.case_decided = true;
                                                        this.update_search_query(cx);
                                                        cx.notify();
                                                    })),
                                            )
                                            .child(
                                                Button::new("whole-word")
                                                    .selected(self.options.whole_word)
                                                    .xsmall()
                                                    .compact()
                                                    .ghost()
                                                    .icon(IconName::WholeWord)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.options.whole_word =
                                                            !this.options.whole_word;
                                                        this.update_search_query(cx);
                                                        cx.notify();
                                                    })),
                                            )
                                            .child(
                                                Button::new("regex")
                                                    .selected(self.options.regex)
                                                    .xsmall()
                                                    .compact()
                                                    .ghost()
                                                    .icon(IconName::Regex)
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.options.regex = !this.options.regex;
                                                        this.update_search_query(cx);
                                                        cx.notify();
                                                    })),
                                            ),
                                    )
                                    .small()
                                    .w_full()
                                    .shadow_none(),
                            )
                            .child(
                                canvas(
                                    {
                                        let view = cx.entity();
                                        move |bounds, _, cx| {
                                            view.update(cx, |r, _| {
                                                r.input_width = bounds.size.width
                                            })
                                        }
                                    },
                                    |_, _, _, _| {},
                                )
                                .absolute()
                                .size_full(),
                            ),
                    )
                    .child(
                        Button::new("replace-mode")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Replace)
                            .selected(self.replace_mode)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.replace_mode = !this.replace_mode;
                                if this.replace_mode {
                                    this.replace_input.read(cx).focus_handle.focus(window);
                                } else {
                                    this.search_input.read(cx).focus_handle.focus(window);
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("prev")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronLeft)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.prev(window, cx);
                            })),
                    )
                    .child(
                        Button::new("next")
                            .xsmall()
                            .ghost()
                            .icon(IconName::ChevronRight)
                            .disabled(!has_matches)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.next(window, cx);
                            })),
                    )
                    .child(
                        Label::new(self.matcher.label())
                            .when(!has_matches, |this| {
                                this.text_color(cx.theme().muted_foreground)
                            })
                            // A regular expression that does not parse reads
                            // the same as "no matches" unless it is said out
                            // loud -- the reader is mid-word, not wrong.
                            .when(self.matcher.is_invalid(), |this| {
                                this.text_color(cx.theme().danger)
                            })
                            .text_left()
                            .min_w_16(),
                    )
                    .child(div().w_7())
                    .child(
                        Button::new("close")
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_action_escape(&Escape, window, cx);
                            })),
                    ),
            )
            .when(self.replace_mode, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            Input::new(&self.replace_input)
                                .focus_bordered(false)
                                .small()
                                .w(self.input_width)
                                .shadow_none(),
                        )
                        .child(
                            Button::new("replace-one")
                                .small()
                                .label(t!("Input.Replace"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_next(window, cx);
                                })),
                        )
                        .child(
                            Button::new("replace-all")
                                .small()
                                .label(t!("Input.Replace All"))
                                .disabled(!has_matches)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.replace_all(window, cx);
                                })),
                        ),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Case never matters.
    fn insensitive() -> SearchOptions {
        SearchOptions {
            smart_case: false,
            ..SearchOptions::default()
        }
    }

    /// Case always matters.
    fn sensitive() -> SearchOptions {
        SearchOptions {
            case_sensitive: true,
            ..SearchOptions::default()
        }
    }

    #[test]
    fn test_search() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", insensitive());

        assert_eq!(matcher.len(), 3);
        let mut matches = matcher.clone();
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next(), Some(18..20));
        assert_eq!(matches.next(), Some(23..25));
        assert_eq!(matches.current_match_ix, 2);
        assert_eq!(matches.next(), Some(15..17));
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next_back(), Some(23..25));
        assert_eq!(matches.current_match_ix, 2);
        assert_eq!(matches.next_back(), Some(18..20));
        assert_eq!(matches.current_match_ix, 1);
        assert_eq!(matches.next_back(), Some(15..17));
        assert_eq!(matches.current_match_ix, 0);
        assert_eq!(matches.next_back(), Some(23..25));

        matcher.update_query("IS", sensitive());
        assert_eq!(matcher.len(), 0);
        assert_eq!(matcher.next(), None);
        assert_eq!(matcher.next_back(), None);
    }

    /// With `wrap_around` off, the ends are the ends -- and a refusal
    /// **leaves the index where it was**.
    #[test]
    fn wrap_off_stops_at_the_ends() {
        let mut m = SearchMatcher::new();
        m.update(&Rope::from("is is is"));
        m.update_query("is", insensitive());
        m.set_wrap(false);
        assert_eq!(m.len(), 3);

        assert_eq!(m.next(), Some(3..5));
        assert_eq!(m.next(), Some(6..8));
        assert_eq!(m.current_match_ix, 2);
        assert_eq!(m.next(), None, "末尾で止まる");
        assert_eq!(m.current_match_ix, 2, "断っても添字は動かない");

        assert_eq!(m.next_back(), Some(3..5));
        assert_eq!(m.next_back(), Some(0..2));
        assert_eq!(m.current_match_ix, 0);
        assert_eq!(m.next_back(), None, "先頭で止まる");
        assert_eq!(m.current_match_ix, 0, "断っても添字は動かない");
    }

    /// The default is what it did before the knob existed.
    #[test]
    fn wrap_on_is_what_it_did_before() {
        let mut m = SearchMatcher::new();
        m.update(&Rope::from("is is is"));
        m.update_query("is", insensitive());

        assert_eq!(m.next(), Some(3..5));
        assert_eq!(m.next(), Some(6..8));
        assert_eq!(m.next(), Some(0..2), "末尾の次は先頭");
        assert_eq!(m.next_back(), Some(6..8), "先頭の前は末尾");
    }

    /// `set_wrap` is about **how to walk**, not how to look -- it must not
    /// throw the match list away.
    #[test]
    fn set_wrap_does_not_look_again() {
        let mut m = SearchMatcher::new();
        m.update(&Rope::from("is is is"));
        m.update_query("is", insensitive());
        let before = Rc::as_ptr(&m.matched_ranges);
        m.set_wrap(false);
        assert!(std::ptr::eq(before, Rc::as_ptr(&m.matched_ranges)));
    }

    /// Re-running an unchanged query keeps the reader's place.
    ///
    /// `next`/`prev` re-run the query before stepping (the list may be stale
    /// when `search_on_type` is off, or a debounce still pending). Without
    /// the guard in `update_query`, that would reset `current_match_ix` and
    /// Enter would land on match one for ever.
    #[test]
    fn re_running_the_same_query_keeps_the_place() {
        let mut m = SearchMatcher::new();
        m.update(&Rope::from("is is is"));
        m.update_query("is", insensitive());
        m.next();
        m.next();
        assert_eq!(m.current_match_ix, 2);

        m.update_query("is", insensitive());
        assert_eq!(m.current_match_ix, 2, "同じ問い合わせなら場所を保つ");

        m.update_query("is", sensitive());
        assert_eq!(m.current_match_ix, 0, "探しかたが変われば数え直す");
    }

    /// Everything stays as it was, except the one departure.
    #[test]
    fn the_default_behaviour_is_todays_behaviour() {
        let b = SearchBehavior::default();
        assert!(b.search_on_type);
        assert_eq!(b.search_debounce_ms, 0);
        assert!(!b.close_on_result);
        assert!(!b.center_on_match);
        assert!(b.wrap_around);
        assert!(!b.seed_from_word);
        // The one deliberate change: every other editor puts the caret on the
        // match, and `hide()` hands focus back to the editor.
        assert!(b.cursor_move_on_type);
    }

    /// What `seed_from_word` will actually pick up.
    ///
    /// This pins `RopeExt::word_at`, not the panel -- the panel has no seam
    /// that can be reached without a `Window`.
    #[test]
    fn the_word_under_the_caret_seeds_the_query() {
        let text = Rope::from("let value = 1;");
        assert_eq!(text.word_at(5), "value", "語の途中");
        assert_eq!(text.word_at(9), "value", "語の直後");
        assert_eq!(text.word_at(10), "", "空白の上では何も拾わない");
        assert_eq!(text.word_at(text.len()), "", "末尾でも落ちない");
    }

    #[test]
    fn test_search_label() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Hello 世界 this is a Is test string."));
        matcher.update_query("Is", insensitive());
        assert_eq!(matcher.label(), "1/3");
        matcher.next();
        assert_eq!(matcher.label(), "2/3");
        matcher.next();
        assert_eq!(matcher.label(), "3/3");
        matcher.next();
        assert_eq!(matcher.label(), "1/3");

        matcher.update_query("IS", sensitive());
        assert_eq!(matcher.label(), "0/0");
    }

    /// An all-lowercase query ignores case; one capital makes it matter.
    #[test]
    fn smart_case_reads_the_query() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Foo foo FOO"));

        matcher.update_query("foo", SearchOptions::default());
        assert_eq!(matcher.len(), 3, "all lowercase should ignore case");

        matcher.update_query("Foo", SearchOptions::default());
        assert_eq!(matcher.len(), 1, "a capital should make case matter");

        // Turning it off goes back to "case never matters".
        matcher.update_query("Foo", insensitive());
        assert_eq!(matcher.len(), 3);
    }

    /// An explicit ask beats the guess.
    #[test]
    fn asking_for_case_outranks_smart_case() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("Foo foo"));
        matcher.update_query(
            "foo",
            SearchOptions {
                case_sensitive: true,
                smart_case: true,
                ..SearchOptions::default()
            },
        );
        assert_eq!(matcher.len(), 1);
    }

    #[test]
    fn whole_word_needs_both_edges() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("for format before for_"));
        let whole = SearchOptions {
            whole_word: true,
            ..SearchOptions::default()
        };

        matcher.update_query("for", SearchOptions::default());
        assert_eq!(matcher.len(), 4, "plain matching counts every run");

        matcher.update_query("for", whole);
        assert_eq!(matcher.len(), 1, "only the bare word counts");
        assert_eq!(matcher.matched_ranges.as_ref(), &vec![0..3]);
    }

    /// `_` and non-ASCII letters are word characters, like everywhere else
    /// in the editor.
    #[test]
    fn whole_word_uses_the_editor_word() {
        assert!(is_word_char('_'));
        assert!(is_word_char('日'));
        assert!(!is_word_char('-'));

        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("word 日本語 word-tail"));
        matcher.update_query(
            "word",
            SearchOptions {
                whole_word: true,
                ..SearchOptions::default()
            },
        );
        assert_eq!(matcher.len(), 2, "a hyphen ends a word, `_` does not");
    }

    #[test]
    fn a_regular_expression_matches() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("fn one() {}\nfn two() {}\n"));
        let re = SearchOptions {
            regex: true,
            ..SearchOptions::default()
        };

        matcher.update_query(r"fn \w+\(", re);
        assert_eq!(matcher.len(), 2);
        assert!(!matcher.is_invalid());

        // An expression that can match nothing must not produce empty hits.
        matcher.update_query("x*", re);
        assert_eq!(matcher.len(), 0, "zero-width matches are not matches");
    }

    /// A half-typed expression is the normal state of a text field.
    #[test]
    fn a_broken_regular_expression_finds_nothing_and_says_so() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("fn one() {}"));
        matcher.update_query(
            "fn (",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        );
        assert_eq!(matcher.len(), 0);
        assert!(matcher.is_invalid(), "the panel has nothing to show for it");
        assert_eq!(matcher.label(), "0/0");

        // Fixing it clears the mark.
        matcher.update_query(
            "fn ",
            SearchOptions {
                regex: true,
                ..SearchOptions::default()
            },
        );
        assert!(!matcher.is_invalid());
        assert_eq!(matcher.len(), 1);
    }

    /// Both at once: the expression is filtered down to whole words.
    #[test]
    fn a_regular_expression_can_also_be_whole_word() {
        let mut matcher = SearchMatcher::new();
        matcher.update(&Rope::from("for format"));
        matcher.update_query(
            "fo.",
            SearchOptions {
                regex: true,
                whole_word: true,
                ..SearchOptions::default()
            },
        );
        assert_eq!(matcher.len(), 1);
    }

    #[test]
    fn test_select_range_start() {
        let mut matcher = SearchMatcher::new();
        matcher.matched_ranges = Rc::new(vec![5..10, 15..20, 25..30]);
        matcher.update_cursor_by_offset(0);
        assert_eq!(matcher.current_match_ix, 0);

        matcher.update_cursor_by_offset(5);
        assert_eq!(matcher.current_match_ix, 0);

        matcher.update_cursor_by_offset(12);
        assert_eq!(matcher.current_match_ix, 1);

        matcher.update_cursor_by_offset(16);
        assert_eq!(matcher.current_match_ix, 1);

        matcher.update_cursor_by_offset(30);
        assert_eq!(matcher.current_match_ix, 2);

        matcher.update_cursor_by_offset(31);
        assert_eq!(matcher.current_match_ix, 2);
    }
}
