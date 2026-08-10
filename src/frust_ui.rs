//! Desk-owned chrome around the portable `frust-ui` design system.

use std::sync::LazyLock;

use topcoat::{
    Result,
    router::{content::Css, route},
};

pub use ::frust_ui::components::*;

/// App chrome and page-inline behavior rules. Portable rules live in
/// [`frust_ui::DESIGN_CSS`].
pub const CHROME_CSS: &str = include_str!("frust_ui.css");

static SERVED_CSS: LazyLock<String> = LazyLock::new(|| {
    let mut css = String::with_capacity(::frust_ui::DESIGN_CSS.len() + CHROME_CSS.len());
    css.push_str(::frust_ui::DESIGN_CSS);
    css.push_str(CHROME_CSS);
    css
});

#[route(GET "/frust-ui.css")]
async fn frust_ui_css() -> Result<Css<&'static str>> {
    Ok(Css(SERVED_CSS.as_str()))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use topcoat::cookie::RouterBuilderCookieExt;
    use topcoat::router::{Body, Request, Router, RouterBuilderDiscoverExt, to_bytes};

    use super::{CHROME_CSS, SERVED_CSS};
    use crate::brand::BRAND_TOKEN_NAMES;

    const PAGES_RS: &str = include_str!("pages.rs");
    const BEFORE_SPLIT_CSS: &str = include_str!("../tests/fixtures/frust_ui_before_split.css");
    const BEFORE_SPLIT_GALLERY: &str =
        include_str!("../tests/fixtures/ui_gallery_before_split.html");

    #[test]
    fn chrome_classes_referenced_by_pages_are_defined() {
        let mut defined = defined_classes(::frust_ui::DESIGN_CSS);
        defined.extend(defined_classes(CHROME_CSS));
        let referenced = literal_classes(PAGES_RS);
        let missing = referenced.difference(&defined).cloned().collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "Desk page classes absent from the portable and chrome stylesheets: {missing:?}"
        );
    }

    #[test]
    fn chrome_tokens_referenced_are_exported_by_the_design_system() {
        let exported = ::frust_ui::DESIGN_TOKENS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let referenced = referenced_properties(CHROME_CSS);
        let missing = referenced
            .iter()
            .filter(|token| !exported.contains(token.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "Desk chrome references tokens absent from frust_ui::DESIGN_TOKENS: {missing:?}"
        );
    }

    #[test]
    fn brand_overrides_target_exported_design_tokens() {
        let exported = ::frust_ui::DESIGN_TOKENS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let missing = BRAND_TOKEN_NAMES
            .iter()
            .copied()
            .filter(|token| !exported.contains(token))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "brand settings target tokens absent from frust_ui::DESIGN_TOKENS: {missing:?}"
        );
    }

    #[test]
    fn desk_component_variant_literals_have_portable_modifiers() {
        let mut missing = Vec::new();
        for (component, function, parameter) in [
            ("fui-btn", "fui_button", "variant"),
            ("fui-alert", "fui_alert", "variant"),
            ("fui-badge", "fui_badge", "color"),
        ] {
            let defined = defined_modifiers(::frust_ui::DESIGN_CSS, component);
            for value in call_site_args(PAGES_RS, function, parameter) {
                if !defined.contains(&value) {
                    missing.push(format!(
                        "{function}({parameter}: {value:?}) has no .{component}--{value}"
                    ));
                }
            }
        }
        missing.sort();
        missing.dedup();
        assert!(missing.is_empty(), "{}", missing.join("\n"));
    }

    #[test]
    fn served_rule_set_matches_the_pre_split_stylesheet() {
        assert_eq!(
            top_level_rules(&SERVED_CSS),
            top_level_rules(BEFORE_SPLIT_CSS),
            "served CSS rule set changed across the crate split"
        );
        assert_eq!(
            SERVED_CSS.as_str(),
            BEFORE_SPLIT_CSS,
            "the current split should also preserve the original stylesheet bytes"
        );
    }

    #[tokio::test]
    async fn gallery_render_matches_the_pre_split_bytes() {
        let router = Router::builder().cookies().discover().build();
        let request = Request::builder()
            .uri("/ui-gallery")
            .body(Body::empty())
            .expect("gallery request");
        let response = router.handle(request).await;
        assert_eq!(response.status().as_u16(), 200);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("gallery bytes");
        let rendered = std::str::from_utf8(&bytes).expect("gallery UTF-8");
        assert_eq!(bytes.len(), BEFORE_SPLIT_GALLERY.len());
        assert_eq!(
            canonical_html(rendered),
            canonical_html(BEFORE_SPLIT_GALLERY),
            "gallery render changed beyond Topcoat's process-randomized attribute order"
        );
    }

    fn without_comments(css: &str) -> String {
        let mut clean = String::with_capacity(css.len());
        let mut rest = css;
        while let Some(open) = rest.find("/*") {
            clean.push_str(&rest[..open]);
            let after_open = &rest[open + 2..];
            if let Some(close) = after_open.find("*/") {
                rest = &after_open[close + 2..];
            } else {
                rest = "";
            }
        }
        clean.push_str(rest);
        clean
    }

    fn defined_classes(css: &str) -> BTreeSet<String> {
        let css = without_comments(css);
        let mut classes = BTreeSet::new();
        let mut rest = css.as_str();
        while let Some(index) = rest.find(".fui-") {
            let class = &rest[index + 1..];
            let end = class
                .find(|character: char| {
                    !(character.is_ascii_alphanumeric() || character == '-' || character == '_')
                })
                .unwrap_or(class.len());
            classes.insert(class[..end].to_owned());
            rest = &class[end..];
        }
        classes
    }

    fn literal_classes(source: &str) -> BTreeSet<String> {
        let mut classes = BTreeSet::new();
        let mut rest = source;
        while let Some(index) = rest.find("class=\"") {
            let value = &rest[index + 7..];
            let Some(end) = value.find('"') else { break };
            classes.extend(
                value[..end]
                    .split_ascii_whitespace()
                    .filter(|class| class.starts_with("fui-"))
                    .map(str::to_owned),
            );
            rest = &value[end + 1..];
        }
        classes
    }

    fn referenced_properties(css: &str) -> BTreeSet<String> {
        // Strip comments first (as `defined_classes` and `top_level_rules` do),
        // so a commented-out `var(--fui-…)` is not counted as a live reference.
        let css = without_comments(css);
        let mut properties = BTreeSet::new();
        let mut rest = css.as_str();
        while let Some(index) = rest.find("var(--fui-") {
            let property = &rest[index + 4..];
            let end = property
                .find(|character: char| {
                    character == ',' || character == ')' || character.is_whitespace()
                })
                .unwrap_or(property.len());
            properties.insert(property[..end].to_owned());
            rest = &property[end..];
        }
        properties
    }

    fn defined_modifiers(css: &str, component: &str) -> BTreeSet<String> {
        // Strip comments so a commented-out `.fui-…--modifier` is not counted.
        let css = without_comments(css);
        let needle = format!(".{component}--");
        let mut modifiers = BTreeSet::new();
        let mut rest = css.as_str();
        while let Some(index) = rest.find(&needle) {
            let value = &rest[index + needle.len()..];
            let end = value
                .find(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
                .unwrap_or(value.len());
            modifiers.insert(value[..end].to_owned());
            rest = &value[end..];
        }
        modifiers
    }

    fn call_site_args(source: &str, function: &str, parameter: &str) -> Vec<String> {
        let mut values = Vec::new();
        let needle = format!("{function}(");
        let mut from = 0;
        while let Some(index) = source[from..].find(&needle) {
            let open = from + index + needle.len();
            let mut depth = 1usize;
            let mut end = open;
            let mut quoted = false;
            let mut escaped = false;
            for (offset, character) in source[open..].char_indices() {
                if quoted {
                    if escaped {
                        escaped = false;
                    } else if character == '\\' {
                        escaped = true;
                    } else if character == '"' {
                        quoted = false;
                    }
                    continue;
                }
                match character {
                    '"' => quoted = true,
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + offset;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(value) = outer_literal_arg(&source[open..end], parameter) {
                values.push(value);
            }
            from = open;
        }
        values
    }

    fn outer_literal_arg(arguments: &str, parameter: &str) -> Option<String> {
        let needle = format!("{parameter}: \"");
        let mut depth = 0usize;
        let mut quoted = false;
        let mut escaped = false;
        for (index, character) in arguments.char_indices() {
            if quoted {
                if escaped {
                    escaped = false;
                } else if character == '\\' {
                    escaped = true;
                } else if character == '"' {
                    quoted = false;
                }
                continue;
            }
            match character {
                '"' => quoted = true,
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ if depth == 0 && arguments[index..].starts_with(&needle) => {
                    let after = &arguments[index + needle.len()..];
                    let mut escaped = false;
                    let end = after.char_indices().find_map(|(offset, character)| {
                        if escaped {
                            escaped = false;
                            None
                        } else if character == '\\' {
                            escaped = true;
                            None
                        } else if character == '"' {
                            Some(offset)
                        } else {
                            None
                        }
                    })?;
                    return Some(after[..end].to_owned());
                }
                _ => {}
            }
        }
        None
    }

    fn top_level_rules(css: &str) -> BTreeMap<String, usize> {
        let css = without_comments(css);
        let mut rules = BTreeMap::new();
        let mut depth = 0usize;
        let mut start = 0usize;
        // Track string literals so a brace inside a declaration value (e.g.
        // `content: "}"`) is not read as structure. `saturating_sub` also keeps a
        // stray closing brace from underflowing `depth` in a debug build; a truly
        // unbalanced stylesheet is still reported by the final assertion. Escaped
        // quotes inside strings are not handled — the served CSS contains none.
        let mut string: Option<char> = None;
        for (index, character) in css.char_indices() {
            if let Some(quote) = string {
                if character == quote {
                    string = None;
                }
                continue;
            }
            match character {
                '"' | '\'' => string = Some(character),
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        let rule = css[start..=index]
                            .split_ascii_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ");
                        if !rule.is_empty() {
                            *rules.entry(rule).or_insert(0) += 1;
                        }
                        start = index + 1;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(depth, 0, "unbalanced CSS braces");
        rules
    }

    fn canonical_html(html: &str) -> String {
        let mut canonical = String::with_capacity(html.len());
        let mut rest = html;
        while let Some(open) = rest.find('<') {
            canonical.push_str(&rest[..open]);
            let tag = &rest[open + 1..];
            let Some(close) = tag.find('>') else {
                canonical.push_str(&rest[open..]);
                return canonical;
            };
            let contents = &tag[..close];
            if contents.starts_with(['/', '!', '?']) {
                canonical.push('<');
                canonical.push_str(contents);
                canonical.push('>');
            } else {
                let mut fields = Vec::new();
                let mut start = 0usize;
                let mut quoted = false;
                for (index, character) in contents.char_indices() {
                    if character == '"' {
                        quoted = !quoted;
                    } else if character.is_ascii_whitespace() && !quoted {
                        if start < index {
                            fields.push(&contents[start..index]);
                        }
                        start = index + character.len_utf8();
                    }
                }
                if start < contents.len() {
                    fields.push(&contents[start..]);
                }
                let tag_name = fields.first().copied().unwrap_or("");
                let mut attributes = fields.into_iter().skip(1).collect::<Vec<_>>();
                attributes.sort_unstable();
                canonical.push('<');
                canonical.push_str(tag_name);
                for attribute in attributes {
                    canonical.push(' ');
                    canonical.push_str(attribute);
                }
                canonical.push('>');
            }
            rest = &tag[close + 1..];
        }
        canonical.push_str(rest);
        canonical
    }
}
