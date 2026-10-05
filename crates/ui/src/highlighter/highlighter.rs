use crate::highlighter::{HighlightTheme, LanguageRegistry};
use crate::input::RopeExt;

use anyhow::{anyhow, Context, Result};
use gpui::{HighlightStyle, SharedString};

use ropey::{ChunkCursor, Rope};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{Arc, Mutex},
    usize,
};
use sum_tree::Bias;
use tree_sitter::{
    InputEdit, Node, Parser, Point, Query, QueryCursor, QueryMatch, StreamingIterator, Tree,
};

/// A syntax highlighter that supports incremental parsing, multiline text,
/// and caching of highlight results.
#[allow(unused)]
pub struct SyntaxHighlighter {
    language: SharedString,
    /// Shared with every other highlighter of the language; see
    /// `LanguageRegistry::query`.
    query: Option<Arc<Query>>,
    injection_queries: HashMap<SharedString, Arc<Query>>,

    locals_pattern_index: usize,
    highlights_pattern_index: usize,
    // highlight_indices: Vec<Option<Highlight>>,
    non_local_variable_patterns: Vec<bool>,
    injection_content_capture_index: Option<u32>,
    injection_language_capture_index: Option<u32>,
    local_scope_capture_index: Option<u32>,
    local_def_capture_index: Option<u32>,
    local_def_value_capture_index: Option<u32>,
    local_ref_capture_index: Option<u32>,

    /// The last parsed source text.
    text: Rope,
    parser: Parser,
    /// The last parsed tree.
    tree: Option<Tree>,

    /// The capture names of `query`, by capture index. Kept as shared strings
    /// so that naming a capture is a reference count, not an allocation -- it
    /// happens once per capture per call.
    capture_names: Vec<SharedString>,
    /// The same for each of `injection_queries`.
    injection_capture_names: HashMap<SharedString, Vec<SharedString>>,

    /// What each injected region (a macro body, a `<script>`, a JSDoc comment)
    /// highlights as.
    ///
    /// The editor asks for styles one visible row at a time, and a row inside an
    /// injected region used to copy, re-parse and re-query the **whole** region
    /// on every call -- a region N rows tall cost N parses per frame (dopamine
    /// #876: 30 lines of `json!` on screen took 450 ms a frame).
    injections: Mutex<InjectionMemo>,
    /// How many injected regions were parsed, for the tests to count.
    #[cfg(test)]
    injection_parses: std::sync::atomic::AtomicUsize,

    /// What each row of the last [`Self::styles_for_rows`] highlights as.
    ///
    /// The editor draws again for reasons that have nothing to do with the
    /// text -- the caret blinks twice a second, another pane notifies -- and
    /// asks the very same question each time; a scroll asks about the same
    /// rows but one (dopamine #876).
    rows: Mutex<Option<RowsMemo>>,
    /// The last answer of [`Self::skipped_ranges`], for the same reason: the
    /// bracket colours, the bracket match and the guides each ask for the
    /// visible range in the same frame.
    skipped: Mutex<Option<(Range<usize>, Arc<Vec<Range<usize>>>)>>,
}

struct RowsMemo {
    /// Held, not just compared by address: a freed theme's address can be
    /// handed to the next one.
    theme: Arc<HighlightTheme>,
    /// By the row's byte range. Only the rows of the last call are kept, so
    /// this never holds more than a screenful.
    rows: HashMap<(usize, usize), Vec<(Range<usize>, HighlightStyle)>>,
}

/// The captures of one injected region, **relative to its start**, sorted by
/// start and never overlapping.
type InjectedCaptures = Arc<Vec<(Range<usize>, SharedString)>>;

#[derive(Default)]
struct InjectionMemo {
    /// By the injected language and the region's byte range: the answer for a
    /// region on screen, without even reading its text. Emptied whenever the
    /// text changes, since every offset may have moved.
    at: HashMap<(SharedString, usize, usize), InjectedCaptures>,
    /// By the injected language and the region's **text**. What a region
    /// highlights as depends on nothing else, so an edit somewhere else in the
    /// file does not have to parse it again -- and parsing is the expensive
    /// part: a macro body is rarely valid as a file of its own, and 100 lines
    /// of `json!` take 50 ms to recover from.
    ///
    /// The flag is whether the entry was asked for since the last edit; the
    /// ones that were not are dropped at the next one. (So two edits with no
    /// frame in between -- one per caret, say -- drop everything, and the next
    /// frame parses what is on screen once more.)
    by_text: HashMap<(SharedString, String), (InjectedCaptures, bool)>,
    /// The bytes of text `by_text` holds as keys.
    bytes: usize,
}

/// Scrolling through a long file touches one region after another without ever
/// changing the text, so the memo is emptied when it holds this many **texts**
/// (`by_text`; `at` is bounded by the regions the file has, and emptied at
/// every edit)...
///
/// Far more than a screen has: a fold keeps its whole body in the visible
/// range, and the string and comment spans are asked for over all of it. A
/// file with more regions than this in one question parses them again on every
/// question, which is what it always did.
const INJECTION_MEMO_MAX: usize = 16 * 1024;
/// ...or this much text.
const INJECTION_MEMO_MAX_BYTES: usize = 16 * 1024 * 1024;

struct TextProvider<'a>(&'a Rope);
struct ByteChunks<'a> {
    cursor: ChunkCursor<'a>,
    end: usize,
}
impl<'a> tree_sitter::TextProvider<&'a [u8]> for TextProvider<'a> {
    type I = ByteChunks<'a>;

    fn text(&mut self, node: tree_sitter::Node) -> Self::I {
        let range = node.byte_range();
        let cursor = self.0.chunk_cursor_at(range.start);

        ByteChunks {
            cursor,
            end: range.end,
        }
    }
}

impl<'a> Iterator for ByteChunks<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        let cursor = &mut self.cursor;
        let end = self.end;

        if cursor.next() && cursor.byte_offset() < end {
            Some(cursor.chunk().as_bytes())
        } else {
            None
        }
    }
}

#[derive(Debug, Default, Clone)]
struct HighlightSummary {
    count: usize,
    start: usize,
    end: usize,
    min_start: usize,
    max_end: usize,
}

/// The highlight item, the range is offset of the token in the tree.
#[derive(Debug, Default, Clone)]
struct HighlightItem {
    /// The byte range of the highlight in the text.
    range: Range<usize>,
    /// The highlight name, like `function`, `string`, `comment`, etc.
    name: SharedString,
}

impl HighlightItem {
    pub fn new(range: Range<usize>, name: impl Into<SharedString>) -> Self {
        Self {
            range,
            name: name.into(),
        }
    }
}

impl sum_tree::Item for HighlightItem {
    type Summary = HighlightSummary;
    fn summary(&self, _cx: &()) -> Self::Summary {
        HighlightSummary {
            count: 1,
            start: self.range.start,
            end: self.range.end,
            min_start: self.range.start,
            max_end: self.range.end,
        }
    }
}

impl sum_tree::Summary for HighlightSummary {
    type Context<'a> = &'a ();
    fn zero(_: Self::Context<'_>) -> Self {
        HighlightSummary {
            count: 0,
            start: usize::MIN,
            end: usize::MAX,
            min_start: usize::MAX,
            max_end: usize::MIN,
        }
    }

    fn add_summary(&mut self, other: &Self, _: Self::Context<'_>) {
        self.min_start = self.min_start.min(other.min_start);
        self.max_end = self.max_end.max(other.max_end);
        self.start = other.start;
        self.end = other.end;
        self.count += other.count;
    }
}

impl<'a> sum_tree::Dimension<'a, HighlightSummary> for usize {
    fn zero(_: &()) -> Self {
        0
    }

    fn add_summary(&mut self, _: &'a HighlightSummary, _: &()) {}
}

impl<'a> sum_tree::Dimension<'a, HighlightSummary> for Range<usize> {
    fn zero(_: &()) -> Self {
        Default::default()
    }

    fn add_summary(&mut self, summary: &'a HighlightSummary, _: &()) {
        self.start = summary.start;
        self.end = summary.end;
    }
}

impl SyntaxHighlighter {
    /// Create a new SyntaxHighlighter for HTML.
    pub fn new(lang: &str) -> Self {
        match Self::build_combined_injections_query(&lang) {
            Ok(result) => result,
            Err(err) => {
                tracing::warn!(
                    "SyntaxHighlighter init failed, fallback to use `text`, {}",
                    err
                );
                Self::build_combined_injections_query("text").unwrap()
            }
        }
    }

    /// Build the combined injections query for the given language.
    ///
    /// https://github.com/tree-sitter/tree-sitter/blob/v0.25.5/highlight/src/lib.rs#L336
    fn build_combined_injections_query(lang: &str) -> Result<Self> {
        let Some(config) = LanguageRegistry::singleton().language(&lang) else {
            return Err(anyhow!(
                "language {:?} is not registered in `LanguageRegistry`",
                lang
            ));
        };

        let mut parser = Parser::new();
        parser
            .set_language(&config.language)
            .context("parse set_language")?;

        // Concatenate the query strings, keeping track of the start offset of each section.
        let mut query_source = String::new();
        query_source.push_str(&config.injections);
        let locals_query_offset = query_source.len();
        query_source.push_str(&config.locals);
        let highlights_query_offset = query_source.len();
        query_source.push_str(&config.highlights);

        // Construct a single query by concatenating the three query strings, but record the
        // range of pattern indices that belong to each individual string.
        let query = LanguageRegistry::singleton()
            .query(&config, &query_source)
            .context("new query")?;

        let mut locals_pattern_index = 0;
        let mut highlights_pattern_index = 0;
        for i in 0..(query.pattern_count()) {
            let pattern_offset = query.start_byte_for_pattern(i);
            if pattern_offset < highlights_query_offset {
                if pattern_offset < highlights_query_offset {
                    highlights_pattern_index += 1;
                }
                if pattern_offset < locals_query_offset {
                    locals_pattern_index += 1;
                }
            }
        }

        // let Some(mut combined_injections_query) =
        //     Query::new(&config.language, &config.injections).ok()
        // else {
        //     return None;
        // };

        // let mut has_combined_queries = false;
        // for pattern_index in 0..locals_pattern_index {
        //     let settings = query.property_settings(pattern_index);
        //     if settings.iter().any(|s| &*s.key == "injection.combined") {
        //         has_combined_queries = true;
        //         query.disable_pattern(pattern_index);
        //     } else {
        //         combined_injections_query.disable_pattern(pattern_index);
        //     }
        // }
        // let combined_injections_query = if has_combined_queries {
        //     Some(combined_injections_query)
        // } else {
        //     None
        // };

        // Find all of the highlighting patterns that are disabled for nodes that
        // have been identified as local variables.
        let non_local_variable_patterns = (0..query.pattern_count())
            .map(|i| {
                query
                    .property_predicates(i)
                    .iter()
                    .any(|(prop, positive)| !*positive && prop.key.as_ref() == "local")
            })
            .collect();

        // Store the numeric ids for all of the special captures.
        let mut injection_content_capture_index = None;
        let mut injection_language_capture_index = None;
        let mut local_def_capture_index = None;
        let mut local_def_value_capture_index = None;
        let mut local_ref_capture_index = None;
        let mut local_scope_capture_index = None;
        for (i, name) in query.capture_names().iter().enumerate() {
            let i = Some(i as u32);
            match *name {
                "injection.content" => injection_content_capture_index = i,
                "injection.language" => injection_language_capture_index = i,
                "local.definition" => local_def_capture_index = i,
                "local.definition-value" => local_def_value_capture_index = i,
                "local.reference" => local_ref_capture_index = i,
                "local.scope" => local_scope_capture_index = i,
                _ => {}
            }
        }

        let shared_names = |query: &Query| -> Vec<SharedString> {
            query
                .capture_names()
                .iter()
                .map(|name| SharedString::from(name.to_string()))
                .collect()
        };
        let capture_names = shared_names(&query);

        let mut injection_queries = HashMap::new();
        let mut injection_capture_names = HashMap::new();
        for inj_language in config.injection_languages.iter() {
            if let Some(inj_config) = LanguageRegistry::singleton().language(&inj_language) {
                match LanguageRegistry::singleton().query(&inj_config, &inj_config.highlights) {
                    Ok(q) => {
                        injection_capture_names.insert(inj_config.name.clone(), shared_names(&q));
                        injection_queries.insert(inj_config.name.clone(), q);
                    }
                    Err(e) => {
                        tracing::error!(
                            "failed to build injection query for {:?}: {:?}",
                            inj_config.name,
                            e
                        );
                    }
                }
            }
        }

        // let highlight_indices = vec![None; query.capture_names().len()];

        Ok(Self {
            language: config.name.clone(),
            query: Some(query),
            injection_queries,

            locals_pattern_index,
            highlights_pattern_index,
            non_local_variable_patterns,
            injection_content_capture_index,
            injection_language_capture_index,
            local_scope_capture_index,
            local_def_capture_index,
            local_def_value_capture_index,
            local_ref_capture_index,
            text: Rope::new(),
            parser,
            tree: None,
            capture_names,
            injection_capture_names,
            injections: Mutex::new(InjectionMemo::default()),
            #[cfg(test)]
            injection_parses: std::sync::atomic::AtomicUsize::new(0),
            rows: Mutex::new(None),
            skipped: Mutex::new(None),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.text.len() == 0
    }

    /// Highlight the given text, returning a map from byte ranges to highlight captures.
    ///
    /// Uses incremental parsing by `edit` to efficiently update the highlighter's state.
    pub fn update(&mut self, edit: Option<InputEdit>, text: &Rope) {
        if self.text.eq(text) {
            return;
        }
        // Everything remembered below was computed from the old text.
        if let Ok(injections) = self.injections.get_mut() {
            injections.at.clear();
            // Keep what the last text had on screen; let go of the rest.
            injections
                .by_text
                .retain(|_, (_, asked)| std::mem::take(asked));
            injections.bytes = injections.by_text.keys().map(|(_, text)| text.len()).sum();
        }
        if let Ok(rows) = self.rows.get_mut() {
            *rows = None;
        }
        if let Ok(skipped) = self.skipped.get_mut() {
            *skipped = None;
        }

        // **Nothing to ask the tree, so no tree.** A file with no grammar of
        // its own (`.txt`, `.log`, an extension nobody knows) is "parsed" as
        // JSON with empty queries: not one capture comes out of it, and since
        // it is not JSON the parser spends its time recovering from errors --
        // 680 ms for a 5 MB log, on the main thread (dopamine #877). Without a
        // tree `match_styles` answers what it answered with one: nothing.
        if !self.has_patterns() {
            self.tree = None;
            self.text = text.clone();
            return;
        }

        // **No tree yet: there is nothing for `edit` to be an edit of.** A
        // highlighter that has parsed nothing and is handed the keystroke that
        // made it necessary -- the first one after the whole text was replaced
        // -- used to edit an empty tree at that offset and re-parse. The
        // parser then reuses the empty tree's end-of-file node and stops: a
        // tree of nothing, and no colours until the file is opened again
        // (dopamine #877). The first parse is of the whole text.
        let edit = if self.tree.is_some() { edit } else { None };
        let edit = edit.unwrap_or(InputEdit {
            start_byte: 0,
            old_end_byte: 0,
            new_end_byte: text.len(),
            start_position: Point::new(0, 0),
            old_end_position: Point::new(0, 0),
            new_end_position: Point::new(0, 0),
        });

        let mut old_tree = self
            .tree
            .take()
            .unwrap_or(self.parser.parse("", None).unwrap());
        old_tree.edit(&edit);

        let new_tree = self.parser.parse_with_options(
            &mut move |offset, _| {
                if offset >= text.len() {
                    ""
                } else {
                    let (chunk, chunk_byte_ix) = text.chunk(offset);
                    &chunk[offset - chunk_byte_ix..]
                }
            },
            Some(&old_tree),
            None,
        );

        let Some(new_tree) = new_tree else {
            return;
        };

        self.tree = Some(new_tree);
        self.text = text.clone();
    }

    /// Whether any query could capture anything.
    fn has_patterns(&self) -> bool {
        self.query
            .as_ref()
            .is_some_and(|query| query.pattern_count() > 0)
    }

    /// Match the visible ranges of nodes in the Tree for highlighting.
    ///
    /// `whole_injections` is whether an injected region that touches `range`
    /// contributes **all** of its captures, as it always used to, or only the
    /// ones that touch `range`. The styles of a row only need the latter. The
    /// string and comment spans need the former: the bracket match scans from
    /// the caret, and the caret can be off screen -- inside the same `<script>`
    /// or macro body, but outside the range the spans were asked for.
    fn match_styles(&self, range: Range<usize>, whole_injections: bool) -> Vec<HighlightItem> {
        let mut highlights = vec![];
        // The item the merge rules below compare the next capture with. It used
        // to be `highlights.last()`; it is tracked on its own now that an
        // injection only contributes the captures that touch `range`, so that
        // the rules still see the injection's **last** capture, as they always
        // did, and decide the same way.
        let mut last: Option<(Range<usize>, SharedString)> = None;
        let Some(tree) = &self.tree else {
            return highlights;
        };

        let Some(query) = &self.query else {
            return highlights;
        };

        let root_node = tree.root_node();

        let source = &self.text;
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range.clone());
        let mut matches = cursor.matches(&query, root_node, TextProvider(&source));

        while let Some(query_match) = matches.next() {
            // Ref:
            // https://github.com/tree-sitter/tree-sitter/blob/460118b4c82318b083b4d527c9c750426730f9c0/highlight/src/lib.rs#L556
            if let (Some(language_name), Some(content_node), _) =
                self.injection_for_match(None, query, query_match)
            {
                // Only the captures that touch `range`, unless asked for all of
                // them: the injected node can be hundreds of rows tall, and a
                // row's styles are about one of them. The list is sorted and
                // never overlaps, so the first one is a binary search away
                // (dopamine #876).
                let (base, styles) = self.handle_injection(&language_name, content_node);
                let first = if whole_injections {
                    0
                } else {
                    styles.partition_point(|(r, _)| base + r.end <= range.start)
                };
                for (node_range, highlight_name) in &styles[first..] {
                    if !whole_injections && base + node_range.start >= range.end {
                        break;
                    }
                    highlights.push(HighlightItem::new(
                        base + node_range.start..base + node_range.end,
                        highlight_name.clone(),
                    ));
                }
                if let Some((end, name)) = styles.last() {
                    last = Some((base + end.start..base + end.end, name.clone()));
                }

                continue;
            }

            for cap in query_match.captures {
                let node = cap.node;

                let Some(highlight_name) = self.capture_names.get(cap.index as usize) else {
                    continue;
                };

                let node_range: Range<usize> = node.start_byte()..node.end_byte();
                let highlight_name = highlight_name.clone();

                // Merge near range and same highlight name
                let last_range = last.as_ref().map(|(range, _)| range.clone()).unwrap_or(0..0);
                let last_highlight_name = last.as_ref().map(|(_, name)| name.clone());

                let item = if last_range.end <= node_range.start
                    && last_highlight_name.as_ref() == Some(&highlight_name)
                {
                    (last_range.start..node_range.end, highlight_name.clone())
                } else if last_range == node_range {
                    // case:
                    // last_range: 213..220, last_highlight_name: Some("property")
                    // last_range: 213..220, last_highlight_name: Some("string")
                    (node_range, last_highlight_name.unwrap_or(highlight_name))
                } else {
                    (node_range, highlight_name.clone())
                };
                highlights.push(HighlightItem::new(item.0.clone(), item.1.clone()));
                last = Some(item);
            }
        }

        // DO NOT REMOVE THIS PRINT, it's useful for debugging
        // for item in highlights {
        //     println!("item: {:?}", item);
        // }

        highlights
    }

    /// The captures of an injected node and the offset they are relative to.
    ///
    /// Not parsed again while [`InjectionMemo`] remembers the node's text.
    fn handle_injection(
        &self,
        injection_language: &SharedString,
        node: Node,
    ) -> (usize, InjectedCaptures) {
        // Ensure byte offsets are on char boundaries for UTF-8 safety
        let start_offset = self.text.clip_offset(node.start_byte(), Bias::Left);
        let end_offset = self.text.clip_offset(node.end_byte(), Bias::Right);

        if !self.injection_queries.contains_key(injection_language) {
            return (start_offset, Arc::new(vec![]));
        }

        let at = (injection_language.clone(), start_offset, end_offset);
        if let Ok(injections) = self.injections.lock() {
            if let Some(found) = injections.at.get(&at) {
                return (start_offset, found.clone());
            }
        }

        // FIXME: Avoid to_string.
        let content = self.text.slice(start_offset..end_offset).to_string();
        let by_text = (injection_language.clone(), content);
        if let Ok(mut injections) = self.injections.lock() {
            if let Some((found, asked)) = injections.by_text.get_mut(&by_text) {
                *asked = true;
                let found = found.clone();
                injections.at.insert(at, found.clone());
                return (start_offset, found);
            }
        }

        let found = Arc::new(self.parse_injection(injection_language, &by_text.1));
        if let Ok(mut injections) = self.injections.lock() {
            if injections.by_text.len() >= INJECTION_MEMO_MAX
                || injections.bytes + by_text.1.len() > INJECTION_MEMO_MAX_BYTES
            {
                injections.at.clear();
                injections.by_text.clear();
                injections.bytes = 0;
            }
            injections.bytes += by_text.1.len();
            injections.at.insert(at, found.clone());
            injections.by_text.insert(by_text, (found.clone(), true));
        }
        (start_offset, found)
    }

    /// TODO: Use incremental parsing to handle the injection.
    fn parse_injection(
        &self,
        injection_language: &SharedString,
        content: &str,
    ) -> Vec<(Range<usize>, SharedString)> {
        let mut cache = vec![];
        let Some(query) = &self.injection_queries.get(injection_language) else {
            return cache;
        };
        let Some(capture_names) = self.injection_capture_names.get(injection_language) else {
            return cache;
        };

        if content.len() == 0 {
            return cache;
        };

        let Some(config) = LanguageRegistry::singleton().language(injection_language) else {
            return cache;
        };
        let mut parser = Parser::new();
        if parser.set_language(&config.language).is_err() {
            return cache;
        }

        #[cfg(test)]
        self.injection_parses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let source = content.as_bytes();
        let Some(tree) = parser.parse(source, None) else {
            return cache;
        };

        let mut query_cursor = QueryCursor::new();
        let mut matches = query_cursor.matches(query, tree.root_node(), source);

        let mut last_end = 0;
        while let Some(m) = matches.next() {
            for cap in m.captures {
                let cap_node = cap.node;

                let node_range: Range<usize> = cap_node.start_byte()..cap_node.end_byte();

                if node_range.start < last_end {
                    continue;
                }
                if node_range.end > content.len() {
                    break;
                }

                if let Some(highlight_name) = capture_names.get(cap.index as usize) {
                    last_end = node_range.end;
                    cache.push((node_range, highlight_name.clone()));
                }
            }
        }

        cache
    }

    /// Ref:
    /// https://github.com/tree-sitter/tree-sitter/blob/v0.25.5/highlight/src/lib.rs#L1229
    ///
    /// Returns:
    /// - `language_name`: The language name of the injection.
    /// - `content_node`: The content node of the injection.
    /// - `include_children`: Whether to include the children of the content node.
    fn injection_for_match<'a>(
        &self,
        parent_name: Option<SharedString>,
        query: &'a Query,
        query_match: &QueryMatch<'a, 'a>,
    ) -> (Option<SharedString>, Option<Node<'a>>, bool) {
        let content_capture_index = self.injection_content_capture_index;
        // let language_capture_index = self.injection_language_capture_index;

        let mut language_name: Option<SharedString> = None;
        let mut content_node = None;

        for capture in query_match.captures {
            let index = Some(capture.index);
            if index == content_capture_index {
                content_node = Some(capture.node);
            }
        }

        let mut include_children = false;
        for prop in query.property_settings(query_match.pattern_index) {
            match prop.key.as_ref() {
                // In addition to specifying the language name via the text of a
                // captured node, it can also be hard-coded via a `#set!` predicate
                // that sets the injection.language key.
                "injection.language" => {
                    if language_name.is_none() {
                        language_name = prop
                            .value
                            .as_ref()
                            .map(std::convert::AsRef::as_ref)
                            .map(ToString::to_string)
                            .map(SharedString::from);
                    }
                }

                // Setting the `injection.self` key can be used to specify that the
                // language name should be the same as the language of the current
                // layer.
                "injection.self" => {
                    if language_name.is_none() {
                        language_name = Some(self.language.clone());
                    }
                }

                // Setting the `injection.parent` key can be used to specify that
                // the language name should be the same as the language of the
                // parent layer
                "injection.parent" => {
                    if language_name.is_none() {
                        language_name = parent_name.clone();
                    }
                }

                // By default, injections do not include the *children* of an
                // `injection.content` node - only the ranges that belong to the
                // node itself. This can be changed using a `#set!` predicate that
                // sets the `injection.include-children` key.
                "injection.include-children" => include_children = true,
                _ => {}
            }
        }

        (language_name, content_node, include_children)
    }

    /// Returns the syntax highlight styles for a range of text.
    ///
    /// The argument `range` is the range of bytes in the text to highlight.
    ///
    /// Returns a vector of tuples where each tuple contains:
    /// - A byte range relative to the text
    /// - The corresponding highlight style for that range
    ///
    /// # Example
    ///
    /// ```no_run
    /// use gpui_component::highlighter::{HighlightTheme, SyntaxHighlighter};
    /// use ropey::Rope;
    ///
    /// let code = "fn main() {\n    println!(\"Hello\");\n}";
    /// let rope = Rope::from_str(code);
    /// let mut highlighter = SyntaxHighlighter::new("rust");
    /// highlighter.update(None, &rope);
    ///
    /// let theme = HighlightTheme::default_dark();
    /// let range = 0..code.len();
    /// let styles = highlighter.styles(&range, &theme);
    /// ```
    /// The byte ranges in `range` that are string or comment text.
    ///
    /// Brackets inside them are not code — a `(` in `"a (b"` has no partner —
    /// so anything that pairs brackets has to skip these. Reuses the same
    /// query pass `styles` runs, and the result is merged and sorted.
    /// The byte ranges in `range` that are string text (strings, regexes, …), merged and
    /// sorted. Comments are left out (dopamine #247: keep trailing spaces inside strings).
    pub fn string_ranges(&self, range: &Range<usize>) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = self
            .match_styles(range.clone(), true)
            .into_iter()
            .filter(|item| item.name.starts_with("string"))
            .map(|item| item.range)
            .collect();
        out.sort_by_key(|r| r.start);
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(out.len());
        for r in out {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
    }

    pub fn skipped_ranges(&self, range: &Range<usize>) -> Vec<Range<usize>> {
        if let Ok(skipped) = self.skipped.lock() {
            if let Some((asked, found)) = skipped.as_ref() {
                if asked == range {
                    return found.as_ref().clone();
                }
            }
        }
        let found = self.find_skipped_ranges(range);
        if let Ok(mut skipped) = self.skipped.lock() {
            *skipped = Some((range.clone(), Arc::new(found.clone())));
        }
        found
    }

    fn find_skipped_ranges(&self, range: &Range<usize>) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = self
            .match_styles(range.clone(), true)
            .into_iter()
            .filter(|item| {
                item.name.starts_with("string") || item.name.starts_with("comment")
            })
            .map(|item| item.range)
            .collect();
        out.sort_by_key(|r| r.start);
        // Overlapping captures (a string inside an injection, say) would make
        // callers test the same offset twice; merge them instead.
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(out.len());
        for r in out {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        merged
    }

    pub fn styles(
        &self,
        range: &Range<usize>,
        theme: &HighlightTheme,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut styles = vec![];
        let start_offset = range.start;

        let highlights = self.match_styles(range.clone(), false);

        // let mut iter_count = 0;
        for item in highlights {
            // iter_count += 1;
            let node_range = &item.range;
            let name = &item.name;

            // A capture that does not touch `range` cannot colour it. It used
            // to be clamped to an empty range instead of dropped -- which did
            // not colour anything either, but a capture **after** `range` kept
            // its start as a boundary, so the answer ran past `range.end` in
            // unstyled pieces. With those gone every range that comes back is
            // inside `range`, which is what lets `styles_for_rows` put rows one
            // after another without merging them (dopamine #876).
            if node_range.end <= range.start || node_range.start >= range.end {
                continue;
            }

            // Avoid start larger than end
            let mut node_range = node_range.start.max(range.start)..node_range.end.min(range.end);
            if node_range.start > node_range.end {
                node_range.end = node_range.start;
            }

            styles.push((node_range, theme.style(name.as_ref()).unwrap_or_default()));
        }

        // If the matched styles is empty, return a default range.
        if styles.len() == 0 {
            return vec![(start_offset..range.end, HighlightStyle::default())];
        }

        let styles = unique_styles(&range, styles);

        // NOTE: DO NOT remove this comment, it is used for debugging.
        // for style in &styles {
        //     println!("---- style: {:?} - {:?}", style.0, style.1.color);
        // }
        // println!("--------------------------------");

        styles
    }

    /// The styles of a run of consecutive rows, in order and without gaps.
    ///
    /// Each item is a row's byte range (its line break included) and whether
    /// the row is hidden, for instance by a fold. A hidden row is not asked
    /// about: it gets one unstyled span covering its bytes, so that whoever
    /// counts runs by position still lands on the right byte after it. A fold
    /// keeps its body inside the visible range, and styling rows nobody can
    /// see is what made a folded block cost as much as the block is tall
    /// (dopamine #876).
    ///
    /// Rows never overlap, so their styles are appended rather than merged --
    /// merging the growing list against each new row is quadratic in rows.
    ///
    /// What a row highlights as is remembered until the text or the theme
    /// changes, for as long as the row keeps being asked about.
    pub fn styles_for_rows(
        &self,
        rows: Vec<(Range<usize>, bool)>,
        theme: &Arc<HighlightTheme>,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        // Taken out rather than borrowed: `styles` below takes other locks.
        let mut known = self
            .rows
            .lock()
            .ok()
            .and_then(|mut memo| memo.take())
            .filter(|memo| Arc::ptr_eq(&memo.theme, theme))
            .map(|memo| memo.rows)
            .unwrap_or_default();
        let mut kept = HashMap::with_capacity(rows.len());

        let mut styles: Vec<(Range<usize>, HighlightStyle)> = vec![];
        let mut hidden_run: Option<Range<usize>> = None;
        for (range, hidden) in rows {
            if hidden {
                match &mut hidden_run {
                    Some(run) => run.end = range.end,
                    None => hidden_run = Some(range),
                }
                continue;
            }
            if let Some(run) = hidden_run.take() {
                styles.push((run, HighlightStyle::default()));
            }
            let key = (range.start, range.end);
            let row_styles = known.remove(&key).unwrap_or_else(|| {
                // `combine_highlights` drops empty ranges; so does this.
                self.styles(&range, theme)
                    .into_iter()
                    .filter(|(range, _)| !range.is_empty())
                    .collect()
            });
            styles.extend(row_styles.iter().cloned());
            kept.insert(key, row_styles);
        }
        if let Some(run) = hidden_run {
            styles.push((run, HighlightStyle::default()));
        }

        if let Ok(mut memo) = self.rows.lock() {
            *memo = Some(RowsMemo {
                theme: theme.clone(),
                rows: kept,
            });
        }
        styles
    }
}

/// To merge intersection ranges, let the subsequent range cover
/// the previous overlapping range and split the previous range.
///
/// From:
///
/// AA
///   BBB
///    CCCCC
///      DD
///         EEEE
///
/// To:
///
/// AABCCDDCEEEE
pub(crate) fn unique_styles(
    total_range: &Range<usize>,
    styles: Vec<(Range<usize>, HighlightStyle)>,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if styles.is_empty() {
        return styles;
    }

    // For example
    //
    // from: [(6..11), (6..11), (11..17), (17..25), (16..19), (25..59))]
    // to:   [6, 11, 16, 17, 19, 25, 59]
    let mut intervals = Vec::with_capacity(styles.len() * 2 + 2);
    // End points are significant for merging decisions
    let mut significant_intervals = Vec::with_capacity(styles.len());
    intervals.push(total_range.start);
    intervals.push(total_range.end);
    for (range, _) in &styles {
        intervals.push(range.start);
        intervals.push(range.end);
        significant_intervals.push(range.end);
    }
    intervals.sort_unstable();
    intervals.dedup();
    significant_intervals.sort_unstable();
    significant_intervals.dedup();

    // The styles in the order they start. The sort is stable, so styles that
    // start together keep the order they were given in.
    let mut by_start: Vec<usize> = (0..styles.len()).collect();
    by_start.sort_by_key(|ix| styles[*ix].0.start);
    let mut next = 0;
    // The styles covering the interval at hand, **in the order they were
    // given** -- later ones go on top, and that order is what decides a tie.
    //
    // This used to be found by walking every style for every interval, which
    // is quadratic in the number of styles (a minified line, the whole buffer
    // for the minimap). Styles nest only as deep as the syntax does, so the
    // list stays a handful long (dopamine #876).
    let mut covering: Vec<usize> = Vec::new();

    let mut result = Vec::with_capacity(intervals.len().saturating_sub(1));

    // For each interval between boundaries, find the top-most style
    //
    // Result e.g.:
    //
    // [(6..11, red), (11..16, green), (16..17, blue), (17..19, red), (19..25, clean), (25..59, blue)]
    for i in 0..intervals.len().saturating_sub(1) {
        let interval = intervals[i]..intervals[i + 1];

        // Every boundary is in `intervals`, so a style that reaches past the
        // start of this interval reaches its end too.
        covering.retain(|ix| styles[*ix].0.end > interval.start);
        while next < by_start.len() && styles[by_start[next]].0.start <= interval.start {
            let ix = by_start[next];
            next += 1;
            if styles[ix].0.end > interval.start {
                let at = covering.partition_point(|other| *other < ix);
                covering.insert(at, ix);
            }
        }

        let mut top_style: Option<HighlightStyle> = None;
        for ix in &covering {
            let style = &styles[*ix].1;
            if let Some(top_style) = &mut top_style {
                merge_highlight_style(top_style, style);
            } else {
                top_style = Some(*style);
            }
        }

        if let Some(style) = top_style {
            result.push((interval, style));
        } else {
            result.push((interval, HighlightStyle::default()));
        }
    }

    // Merge adjacent ranges with the same style, but not across significant boundaries
    let mut merged: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(result.len());
    for (range, style) in result {
        if let Some((last_range, last_style)) = merged.last_mut() {
            if last_range.end == range.start
                && *last_style == style
                && significant_intervals.binary_search(&range.start).is_err()
            {
                // Merge adjacent ranges with same style, but not across significant boundaries
                last_range.end = range.end;
                continue;
            }
        }
        merged.push((range, style));
    }

    merged
}

/// Merge other style (Other on top)
fn merge_highlight_style(style: &mut HighlightStyle, other: &HighlightStyle) {
    if let Some(color) = other.color {
        style.color = Some(color);
    }
    if let Some(font_weight) = other.font_weight {
        style.font_weight = Some(font_weight);
    }
    if let Some(font_style) = other.font_style {
        style.font_style = Some(font_style);
    }
    if let Some(background_color) = other.background_color {
        style.background_color = Some(background_color);
    }
    if let Some(underline) = other.underline {
        style.underline = Some(underline);
    }
    if let Some(strikethrough) = other.strikethrough {
        style.strikethrough = Some(strikethrough);
    }
    if let Some(fade_out) = other.fade_out {
        style.fade_out = Some(fade_out);
    }
}

#[cfg(test)]
mod tests {
    use gpui::Hsla;

    use super::*;
    use crate::Colorize as _;

    fn color_style(color: Hsla) -> HighlightStyle {
        let mut style = HighlightStyle::default();
        style.color = Some(color);
        style
    }

    #[track_caller]
    fn assert_unique_styles(
        range: Range<usize>,
        left: Vec<(Range<usize>, HighlightStyle)>,
        right: Vec<(Range<usize>, HighlightStyle)>,
    ) {
        fn color_name(c: Option<Hsla>) -> String {
            match c {
                Some(c) => {
                    if c == gpui::red() {
                        "red".to_string()
                    } else if c == gpui::green() {
                        "green".to_string()
                    } else if c == gpui::blue() {
                        "blue".to_string()
                    } else {
                        c.to_hex()
                    }
                }
                None => "clean".to_string(),
            }
        }

        let left = unique_styles(&range, left);
        if left.len() != right.len() {
            println!("\n---------------------------------------------");
            for (range, style) in left.iter() {
                println!("({:?}, {})", range, color_name(style.color));
            }
            println!("---------------------------------------------");
            panic!("left {} styles, right {} styles", left.len(), right.len());
        }
        for (left, right) in left.into_iter().zip(right) {
            if left.1.color != right.1.color || left.0 != right.0 {
                panic!(
                    "\n left: ({:?}, {})\nright: ({:?}, {})\n",
                    left.0,
                    color_name(left.1.color),
                    right.0,
                    color_name(right.1.color)
                );
            }
        }
    }

    #[test]
    fn test_unique_styles() {
        let red = color_style(gpui::red());
        let green = color_style(gpui::green());
        let blue = color_style(gpui::blue());
        let clean = HighlightStyle::default();

        assert_unique_styles(
            0..65,
            vec![
                (2..10, clean),
                (2..10, clean),
                (5..11, red),
                (2..6, clean),
                (10..15, green),
                (15..30, clean),
                (29..35, blue),
                (35..40, green),
                (45..60, blue),
            ],
            vec![
                (0..5, clean),
                (5..6, red),
                (6..10, red),
                (10..11, green),
                (11..15, green),
                (15..29, clean),
                (29..30, blue),
                (30..35, blue),
                (35..40, green),
                (40..45, clean),
                (45..60, blue),
                (60..65, clean),
            ],
        );
    }

    /// `unique_styles` as it was before it became a sweep: every style tried
    /// against every interval. Kept to check the sweep against (dopamine #876).
    fn unique_styles_by_walking(
        total_range: &Range<usize>,
        styles: Vec<(Range<usize>, HighlightStyle)>,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        use std::collections::BTreeSet;

        if styles.is_empty() {
            return styles;
        }

        let mut intervals = BTreeSet::new();
        let mut significant_intervals = BTreeSet::new();
        intervals.insert(total_range.start);
        intervals.insert(total_range.end);
        for (range, _) in &styles {
            intervals.insert(range.start);
            intervals.insert(range.end);
            significant_intervals.insert(range.end);
        }

        let intervals: Vec<usize> = intervals.into_iter().collect();
        let mut result = Vec::with_capacity(intervals.len().saturating_sub(1));
        for i in 0..intervals.len().saturating_sub(1) {
            let interval = intervals[i]..intervals[i + 1];
            if interval.start >= interval.end {
                continue;
            }

            let mut top_style: Option<HighlightStyle> = None;
            for (range, style) in &styles {
                if range.start <= interval.start && interval.end <= range.end {
                    if let Some(top_style) = &mut top_style {
                        merge_highlight_style(top_style, style);
                    } else {
                        top_style = Some(*style);
                    }
                }
            }

            if let Some(style) = top_style {
                result.push((interval, style));
            } else {
                result.push((interval, HighlightStyle::default()));
            }
        }

        let mut merged: Vec<(Range<usize>, HighlightStyle)> = Vec::with_capacity(result.len());
        for (range, style) in result {
            if let Some((last_range, last_style)) = merged.last_mut() {
                if last_range.end == range.start
                    && *last_style == style
                    && !significant_intervals.contains(&range.start)
                {
                    last_range.end = range.end;
                    continue;
                }
            }
            merged.push((range, style));
        }

        merged
    }

    /// Numbers that look random and are the same on every run.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, below: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) as usize) % below.max(1)
        }
    }

    /// The order of the styles decides which one wins where they overlap, and
    /// the sweep keeps its own list of the ones in play -- so it is checked
    /// against the walk it replaced, on overlapping, nested, empty and
    /// out-of-range styles alike.
    #[test]
    fn the_sweep_gives_what_the_walk_gave() {
        let colors = [gpui::red(), gpui::green(), gpui::blue()];
        let mut dice = Dice(876);
        for round in 0..3000 {
            let start = dice.roll(40);
            let total = start..start + dice.roll(60);
            let styles: Vec<(Range<usize>, HighlightStyle)> = (0..dice.roll(14))
                .map(|_| {
                    let start = dice.roll(110);
                    let mut style = HighlightStyle::default();
                    match dice.roll(6) {
                        0 => style.font_weight = Some(gpui::FontWeight::BOLD),
                        1 => {}
                        n => style.color = Some(colors[n % colors.len()]),
                    }
                    // One in four is empty: it colours nothing, but its ends
                    // are still boundaries.
                    let len = if dice.roll(4) == 0 { 0 } else { dice.roll(30) };
                    (start..start + len, style)
                })
                .collect();

            assert_eq!(
                unique_styles(&total, styles.clone()),
                unique_styles_by_walking(&total, styles.clone()),
                "round {round}: {total:?} over {styles:?}"
            );
        }
    }

    #[cfg(feature = "tree-sitter-languages-core")]
    mod with_a_language {
        use super::*;
        use std::sync::atomic::Ordering;

        /// A macro body is an injected region: its text is parsed as Rust on
        /// its own.
        const MACRO: &str = "fn data() -> Value {\n    json!({\n        \"a\": [1, 2, 3],\n        \"b\": { \"c\": true },\n        \"d\": \"(not a bracket\",\n    })\n}\n\nfn after() -> usize {\n    // a comment\n    1 + 2\n}\n";

        fn highlighter(language: &str, text: &str) -> SyntaxHighlighter {
            let mut highlighter = SyntaxHighlighter::new(language);
            highlighter.update(None, &Rope::from(text));
            highlighter
        }

        /// Each row with its line break -- the last one counted as if it had
        /// one, the way the editor counts.
        fn rows_of(text: &str) -> Vec<Range<usize>> {
            let mut rows = vec![];
            let mut at = 0;
            for line in text.split('\n') {
                rows.push(at..at + line.len() + 1);
                at += line.len() + 1;
            }
            rows
        }

        fn row_by_row(
            highlighter: &SyntaxHighlighter,
            rows: &[Range<usize>],
            theme: &HighlightTheme,
        ) -> Vec<Vec<(Range<usize>, HighlightStyle)>> {
            rows.iter()
                .map(|row| highlighter.styles(row, theme))
                .collect()
        }

        fn shifted(
            styles: Vec<Vec<(Range<usize>, HighlightStyle)>>,
            by: usize,
        ) -> Vec<Vec<(Range<usize>, HighlightStyle)>> {
            styles
                .into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|(range, style)| (range.start + by..range.end + by, style))
                        .collect()
                })
                .collect()
        }

        /// The macro body is five rows tall. Asking about every row of the
        /// file, twice, and about the strings in it, does not parse it again.
        #[test]
        fn an_injected_region_is_parsed_once() {
            let highlighter = highlighter("rust", MACRO);
            let theme = HighlightTheme::default_dark();
            let rows = rows_of(MACRO);

            // The first row inside the body parses it...
            highlighter.styles(&rows[2], &theme);
            let parses = highlighter.injection_parses.load(Ordering::Relaxed);
            assert!(parses > 0, "the macro body is not injected in this grammar");
            // ...and nothing after that does.
            let first = row_by_row(&highlighter, &rows, &theme);
            assert_eq!(highlighter.injection_parses.load(Ordering::Relaxed), parses);

            let again = row_by_row(&highlighter, &rows, &theme);
            highlighter.skipped_ranges(&(0..MACRO.len()));
            assert_eq!(first, again);
            assert_eq!(highlighter.injection_parses.load(Ordering::Relaxed), parses);
        }

        /// What a region highlights as depends on its text alone, so typing
        /// above it only moves the answer.
        #[test]
        fn an_edit_elsewhere_moves_an_injected_region_without_parsing_it() {
            let mut highlighter = highlighter("rust", MACRO);
            let theme = HighlightTheme::default_dark();
            let before = row_by_row(&highlighter, &rows_of(MACRO), &theme);
            let parses = highlighter.injection_parses.load(Ordering::Relaxed);
            assert!(parses > 0, "the macro body is not injected in this grammar");

            let moved = format!("\n{MACRO}");
            highlighter.update(None, &Rope::from(moved.as_str()));
            let after = row_by_row(&highlighter, &rows_of(&moved)[1..], &theme);

            assert_eq!(after, shifted(before, 1));
            assert_eq!(highlighter.injection_parses.load(Ordering::Relaxed), parses);
        }

        /// Nothing remembered may outlive the text it was computed from: after
        /// an edit inside the region, the answers are the ones a highlighter
        /// that never saw the old text gives.
        #[test]
        fn an_edit_inside_an_injected_region_is_seen() {
            let mut edited = highlighter("rust", MACRO);
            let theme = HighlightTheme::default_dark();
            let rows = rows_of(MACRO);
            row_by_row(&edited, &rows, &theme);
            edited.styles_for_rows(rows.iter().map(|r| (r.clone(), false)).collect(), &theme);
            edited.skipped_ranges(&(0..MACRO.len()));

            let changed = MACRO.replace("true", "\"yes\"").replace("[1, 2, 3]", "(1)");
            assert_ne!(changed, MACRO);
            edited.update(None, &Rope::from(changed.as_str()));
            let fresh = highlighter("rust", &changed);
            let rows = rows_of(&changed);

            assert_eq!(
                row_by_row(&edited, &rows, &theme),
                row_by_row(&fresh, &rows, &theme)
            );
            let asked: Vec<(Range<usize>, bool)> =
                rows.iter().map(|r| (r.clone(), false)).collect();
            assert_eq!(
                edited.styles_for_rows(asked.clone(), &theme),
                fresh.styles_for_rows(asked, &theme)
            );
            assert_eq!(
                edited.skipped_ranges(&(0..changed.len())),
                fresh.skipped_ranges(&(0..changed.len()))
            );
        }

        /// The editor counts runs by position: the rows have to come back one
        /// after another, with no gap, no overlap and no empty range.
        #[test]
        fn rows_come_back_one_after_another() {
            for (language, text) in [
                ("rust", MACRO),
                ("html", "<p>hi</p>\n<script>\nlet a = [1, 2];\n// note\n</script>\n"),
                ("markdown", "# Title\n\nSome *text* and `code`.\n\n- item\n"),
                ("text", "just\nsome\nlines"),
            ] {
                let highlighter = highlighter(language, text);
                let theme = HighlightTheme::default_dark();
                let rows = rows_of(text);
                let expected: Vec<(Range<usize>, HighlightStyle)> = rows
                    .iter()
                    .flat_map(|row| highlighter.styles(row, &theme))
                    .filter(|(range, _)| !range.is_empty())
                    .collect();

                let asked: Vec<(Range<usize>, bool)> =
                    rows.iter().map(|r| (r.clone(), false)).collect();
                let styles = highlighter.styles_for_rows(asked.clone(), &theme);
                assert_eq!(styles, expected, "{language}");
                // And the same from what it remembers.
                assert_eq!(highlighter.styles_for_rows(asked, &theme), expected);

                let mut at = 0;
                for (range, _) in &styles {
                    assert_eq!(range.start, at, "{language}: a gap or an overlap");
                    assert!(range.end > range.start, "{language}: an empty range");
                    at = range.end;
                }
                assert_eq!(at, rows.last().unwrap().end, "{language}");
            }
        }

        /// A fold keeps its body in the visible range. Those rows are not
        /// styled; they come back as one unstyled span that keeps the rows
        /// below at the right position.
        #[test]
        fn hidden_rows_are_one_unstyled_span() {
            let highlighter = highlighter("rust", MACRO);
            let theme = HighlightTheme::default_dark();
            let rows = rows_of(MACRO);
            let shown = |row: &Range<usize>| -> Vec<(Range<usize>, HighlightStyle)> {
                highlighter
                    .styles(row, &theme)
                    .into_iter()
                    .filter(|(range, _)| !range.is_empty())
                    .collect()
            };

            // Rows 2..=4 hidden, handed over one by one...
            let asked: Vec<(Range<usize>, bool)> = rows
                .iter()
                .enumerate()
                .map(|(ix, row)| (row.clone(), (2..=4).contains(&ix)))
                .collect();
            let mut expected = vec![];
            expected.extend(shown(&rows[0]));
            expected.extend(shown(&rows[1]));
            expected.push((rows[2].start..rows[4].end, HighlightStyle::default()));
            for row in &rows[5..] {
                expected.extend(shown(row));
            }
            assert_eq!(highlighter.styles_for_rows(asked, &theme), expected);

            // ...or as the one run they are.
            let mut asked: Vec<(Range<usize>, bool)> = vec![
                (rows[0].clone(), false),
                (rows[1].clone(), false),
                (rows[2].start..rows[4].end, true),
            ];
            asked.extend(rows[5..].iter().map(|row| (row.clone(), false)));
            assert_eq!(highlighter.styles_for_rows(asked, &theme), expected);

            // Hidden to the very end.
            let asked = vec![(rows[0].clone(), false), (rows[1].start..MACRO.len() + 1, true)];
            let mut expected = shown(&rows[0]);
            expected.push((rows[1].start..MACRO.len() + 1, HighlightStyle::default()));
            assert_eq!(highlighter.styles_for_rows(asked, &theme), expected);
        }

        /// What is remembered was coloured with one theme; the next theme gets
        /// its own colours.
        #[test]
        fn the_rows_follow_the_theme() {
            let highlighter = highlighter("rust", MACRO);
            let asked: Vec<(Range<usize>, bool)> = rows_of(MACRO)
                .into_iter()
                .map(|row| (row, false))
                .collect();
            let dark = HighlightTheme::default_dark();
            let light = HighlightTheme::default_light();

            let in_dark = highlighter.styles_for_rows(asked.clone(), &dark);
            let in_light = highlighter.styles_for_rows(asked.clone(), &light);
            assert_ne!(in_dark, in_light);

            let fresh = self::highlighter("rust", MACRO);
            assert_eq!(in_light, fresh.styles_for_rows(asked.clone(), &light));
            assert_eq!(highlighter.styles_for_rows(asked, &dark), in_dark);
        }

        /// The bracket colours, the bracket match and the guides each ask for
        /// the same range in the same frame.
        #[test]
        fn the_skipped_ranges_are_remembered_per_range() {
            let highlighter = highlighter("rust", MACRO);
            let whole = 0..MACRO.len();
            let found = highlighter.skipped_ranges(&whole);
            assert!(!found.is_empty());
            assert!(found.windows(2).all(|w| w[0].end < w[1].start), "sorted and merged");
            assert_eq!(highlighter.skipped_ranges(&whole), found);

            // Another range is another question.
            let tail = MACRO.find("fn after").unwrap()..MACRO.len();
            let in_tail = highlighter.skipped_ranges(&tail);
            assert_eq!(in_tail, highlighter.find_skipped_ranges(&tail));
            assert_ne!(in_tail, found);
            assert_eq!(highlighter.skipped_ranges(&whole), found);
        }
        /// The same edit, but one that keeps every length: each row and each
        /// region sits at the very offsets it was remembered under. Whatever
        /// `update` forgot to let go of would be handed back here.
        #[test]
        fn an_edit_that_keeps_every_offset_is_seen() {
            let mut edited = highlighter("rust", MACRO);
            let theme = HighlightTheme::default_dark();
            let rows = rows_of(MACRO);
            let asked: Vec<(Range<usize>, bool)> =
                rows.iter().map(|r| (r.clone(), false)).collect();
            let whole = 0..MACRO.len();

            let styles_before = edited.styles_for_rows(asked.clone(), &theme);
            let by_row_before = row_by_row(&edited, &rows, &theme);
            let skipped_before = edited.skipped_ranges(&whole);

            // A keyword becomes a name, and a string becomes code with a
            // bracket in it -- same number of bytes, on the same rows.
            let changed = MACRO
                .replace("true", "tree")
                .replace("\"(not a bracket\"", "(not_a_bracket_)");
            assert_eq!(changed.len(), MACRO.len());
            assert_ne!(changed, MACRO);
            edited.update(None, &Rope::from(changed.as_str()));
            let fresh = highlighter("rust", &changed);

            let styles_after = edited.styles_for_rows(asked.clone(), &theme);
            let by_row_after = row_by_row(&edited, &rows, &theme);
            let skipped_after = edited.skipped_ranges(&whole);
            assert_eq!(styles_after, fresh.styles_for_rows(asked, &theme));
            assert_eq!(by_row_after, row_by_row(&fresh, &rows, &theme));
            assert_eq!(skipped_after, fresh.skipped_ranges(&whole));
            // And the answers did change, so the old ones would have been wrong.
            assert_ne!(styles_after, styles_before);
            assert_ne!(by_row_after, by_row_before);
            assert_ne!(skipped_after, skipped_before);
        }

        /// The bracket match scans from the caret, and the caret can be off
        /// screen: the spans asked for the visible range still have to cover
        /// the rest of an injected region that reaches into it, as they did
        /// when every capture of the region was collected.
        #[test]
        fn skipped_ranges_cover_the_whole_of_an_injected_region() {
            let text = "<script>\nfunction setup() {\n  const close = \"}\";\n  let x = 1;\n  let y = 2;\n}\n</script>\n";
            let highlighter = highlighter("html", text);
            let string = text.find("\"}\"").unwrap();
            // From the row after the string to the end: the string is above it.
            let visible = text.find("  let y").unwrap()..text.len();
            assert!(string + 3 <= visible.start);

            let skipped = highlighter.skipped_ranges(&visible);
            assert!(
                skipped.iter().any(|r| r.start <= string && string + 3 <= r.end),
                "the string above the range is not skipped: {skipped:?}"
            );
            // A row's styles are still only about the row.
            let theme = HighlightTheme::default_dark();
            assert!(highlighter
                .styles(&visible, &theme)
                .iter()
                .all(|(r, _)| r.start >= visible.start && r.end <= visible.end + 1));
        }
        /// Compiling the queries is most of what building a highlighter costs,
        /// and they never change: the second highlighter of a language gets
        /// the first one's (dopamine #877).
        #[test]
        fn highlighters_of_one_language_share_their_queries() {
            // Registering a language empties what is kept, and another test
            // does that -- so a pair may straddle it. Not five in a row.
            let shared = (0..5).any(|_| {
                let first = SyntaxHighlighter::new("rust");
                let second = SyntaxHighlighter::new("rust");
                let (Some(a), Some(b)) = (&first.query, &second.query) else {
                    panic!("no query for rust");
                };
                assert!(!first.injection_queries.is_empty());
                Arc::ptr_eq(a, b)
                    && first.injection_queries.iter().all(|(language, query)| {
                        Arc::ptr_eq(query, &second.injection_queries[language])
                    })
            });
            assert!(shared, "each highlighter compiled its own queries");

            // Another language is another query.
            let rust = SyntaxHighlighter::new("rust");
            let other = SyntaxHighlighter::new("javascript");
            assert!(!Arc::ptr_eq(
                rust.query.as_ref().unwrap(),
                other.query.as_ref().unwrap()
            ));
        }

        /// A file with no grammar of its own has nothing to ask a tree, so it
        /// is not parsed -- and is styled exactly as before: not at all.
        #[test]
        fn a_language_without_queries_is_not_parsed() {
            let text = "{ \"not\": json, just [text] }\nsecond line\n";
            let plain = highlighter("text", text);
            assert!(plain.tree.is_none());
            let theme = HighlightTheme::default_dark();
            for row in rows_of(text) {
                assert_eq!(
                    plain.styles(&row, &theme),
                    vec![(row.clone(), HighlightStyle::default())]
                );
            }
            assert!(plain.skipped_ranges(&(0..text.len())).is_empty());

            // One that has queries still is.
            assert!(highlighter("rust", MACRO).tree.is_some());
            assert!(highlighter("json", text).tree.is_some());
        }
        /// A highlighter that has parsed nothing has no tree for an edit to be
        /// an edit *of*. Handed one anyway -- the first keystroke after the
        /// whole text was replaced -- it used to "re-parse" an empty tree,
        /// reuse its end-of-file node, and end up with a tree of nothing: no
        /// colours until the file was opened again (dopamine #877).
        #[test]
        fn the_first_update_parses_from_scratch_whatever_edit_comes_with_it() {
            let text = Rope::from(MACRO);
            let at = MACRO.find("json!").unwrap();
            let mut edited = SyntaxHighlighter::new("rust");
            edited.update(
                Some(InputEdit {
                    start_byte: at,
                    old_end_byte: at,
                    new_end_byte: at + 1,
                    start_position: Point::new(1, 4),
                    old_end_position: Point::new(1, 4),
                    new_end_position: Point::new(1, 5),
                }),
                &text,
            );

            let fresh = highlighter("rust", MACRO);
            let theme = HighlightTheme::default_dark();
            let rows = rows_of(MACRO);
            assert_eq!(
                row_by_row(&edited, &rows, &theme),
                row_by_row(&fresh, &rows, &theme)
            );
            let tree = edited.tree.as_ref().expect("parsed");
            assert_eq!(tree.root_node().end_byte(), MACRO.len());
        }
    }
}
