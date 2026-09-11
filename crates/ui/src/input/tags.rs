//! Closing JSX/TSX tags.
//!
//! Pure and table-free: the tag name is read straight off the line, the same
//! way an editor does it before any parser has run. Zed calls this
//! `jsx_tag_auto_close`.

/// The closing tag for the element that `before` just opened, if any.
///
/// `before` is the line up to and including the `>` that was typed. Returns
/// `None` for a closing tag (`</p>`), a self-closing one (`<br />`), a
/// fragment (`<>`), and anything that is not a tag at all.
pub fn closing_tag(before: &str) -> Option<String> {
    let before = before.strip_suffix('>')?;
    if before.ends_with('/') {
        return None;
    }
    let open = before.rfind('<')?;
    // **A tag never follows an identifier.** `.ts` and `.tsx` arrive under the
    // same language name, so without this `Array<Foo>` would grow a closing
    // tag. Markup shows up after a space, `(`, `=`, `return`, or the start of
    // the line; a generic shows up glued to the name in front of it.
    if let Some(prev) = before[..open].chars().next_back() {
        if prev.is_alphanumeric() || prev == '_' || prev == '>' {
            return None;
        }
    }
    let inner = &before[open + 1..];
    if inner.starts_with('/') {
        return None;
    }
    // The name runs until whitespace, and may carry a namespace or a member
    // path (`<Foo.Bar>`, `<svg:rect>`).
    let name: String = inner
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.' || *c == ':' || *c == '-')
        .collect();
    if name.is_empty() {
        return None;
    }
    // A tag only starts with a letter or an underscore — `<3` is not markup.
    if !name.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        return None;
    }
    Some(format!("</{name}>"))
}

/// Whether `language` is written with JSX.
pub fn is_jsx(language: &str) -> bool {
    matches!(language, "typescript" | "javascript")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_element_gets_its_closer() {
        assert_eq!(closing_tag("<div>"), Some("</div>".into()));
        assert_eq!(
            closing_tag("  return <Foo bar={1}>"),
            Some("</Foo>".into())
        );
        assert_eq!(closing_tag("<Foo.Bar>"), Some("</Foo.Bar>".into()));
        assert_eq!(closing_tag("<svg:rect>"), Some("</svg:rect>".into()));
    }

    #[test]
    fn closers_and_self_closing_tags_are_left_alone() {
        assert_eq!(closing_tag("</div>"), None);
        assert_eq!(closing_tag("<br />"), None);
        assert_eq!(closing_tag("<br/>"), None);
        assert_eq!(closing_tag("<>"), None, "フラグメント");
    }

    #[test]
    fn generics_are_not_tags() {
        assert_eq!(closing_tag("let v: Array<Foo>"), None);
        assert_eq!(closing_tag("foo<Bar>"), None);
        assert_eq!(closing_tag("Map<K,V>"), None);
        // ...but markup in the same file still works.
        assert_eq!(closing_tag("return <Foo>"), Some("</Foo>".into()));
        assert_eq!(closing_tag("  <Foo>"), Some("</Foo>".into()));
        assert_eq!(closing_tag("(<Foo>"), Some("</Foo>".into()));
    }

    #[test]
    fn things_that_are_not_tags_are_left_alone() {
        assert_eq!(closing_tag("a > b"), None);
        assert_eq!(closing_tag("if x <3>"), None, "`<3` はタグではない");
        assert_eq!(closing_tag("->"), None);
        assert_eq!(closing_tag(""), None);
    }

    #[test]
    fn only_jsx_languages() {
        assert!(is_jsx("typescript"));
        assert!(is_jsx("javascript"));
        assert!(!is_jsx("rust"));
        assert!(!is_jsx("html"), "HTML は別（閉じタグは書き手が管理する）");
    }
}
