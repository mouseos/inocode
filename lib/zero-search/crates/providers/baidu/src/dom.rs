//! Minimal DOM helpers over `scraper` (html5ever), reproducing the
//! lxml traversal semantics the Python parser relies on:
//!
//! * document-order (pre-order) traversal including the node itself
//!   (lxml `el.iter()`),
//! * `element_text`: descendant text concatenated in document order,
//!   skipping comments, `script`/`style` subtrees and any subtree
//!   whose class attribute contains `cosc-tts`, with whitespace runs
//!   (including full-width spaces) collapsed to a single space.

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::ElementRef;

pub type NRef<'a> = NodeRef<'a, Node>;

pub fn as_element<'a>(node: NRef<'a>) -> Option<ElementRef<'a>> {
    ElementRef::wrap(node)
}

pub fn tag_name(node: NRef<'_>) -> Option<String> {
    match node.value() {
        Node::Element(e) => Some(e.name().to_string()),
        _ => None,
    }
}

pub fn attr(node: NRef<'_>, name: &str) -> Option<String> {
    match node.value() {
        Node::Element(e) => e.attr(name).map(|s| s.to_string()),
        _ => None,
    }
}

pub fn class_attr(node: NRef<'_>) -> String {
    attr(node, "class").unwrap_or_default()
}

pub fn has_class_token(node: NRef<'_>, token: &str) -> bool {
    class_attr(node).split_whitespace().any(|t| t == token)
}

/// Class attribute *contains* the substring (lxml
/// `contains(@class,"...")` semantics — not a token match).
pub fn class_contains(node: NRef<'_>, needle: &str) -> bool {
    class_attr(node).contains(needle)
}

/// Pre-order traversal including `node` itself (lxml `iter()`).
pub fn iter_incl<'a>(node: NRef<'a>) -> Vec<NRef<'a>> {
    let mut out = vec![node];
    out.extend(node.descendants());
    out
}

/// Descendant *elements* of `node` (excluding itself), document order.
pub fn descendant_elements<'a>(node: NRef<'a>) -> Vec<NRef<'a>> {
    node.descendants()
        .filter(|n| matches!(n.value(), Node::Element(_)))
        .collect()
}

/// All elements in the document (root = tree root node).
pub fn all_elements<'a>(root: NRef<'a>) -> Vec<NRef<'a>> {
    iter_incl(root)
        .into_iter()
        .filter(|n| matches!(n.value(), Node::Element(_)))
        .collect()
}

/// True when `node` is `ancestor` itself or one of its descendants
/// (lxml `node in ancestor.iter()`).
pub fn is_within(node: NRef<'_>, ancestor: NRef<'_>) -> bool {
    let aid = ancestor.id();
    let mut cur = Some(node);
    while let Some(n) = cur {
        if n.id() == aid {
            return true;
        }
        cur = n.parent();
    }
    false
}

/// Python `normalize_text`: collapse every run of whitespace
/// (`\s` plus full-width space) to one ASCII space and strip.
pub fn normalize_text(value: Option<&str>) -> Option<String> {
    let value = value?;
    let mut out = String::new();
    let mut in_ws = false;
    for ch in value.chars() {
        if ch.is_whitespace() || ch == '\u{3000}' {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(ch);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn collect_text(node: NRef<'_>, parts: &mut Vec<String>, skip_special: bool) {
    match node.value() {
        Node::Text(t) => parts.push(t.text.to_string()),
        Node::Element(e) => {
            if skip_special {
                if e.name() == "script" || e.name() == "style" {
                    return;
                }
                if e.attr("class")
                    .map(|c| c.contains("cosc-tts"))
                    .unwrap_or(false)
                {
                    return;
                }
            }
            for child in node.children() {
                collect_text(child, parts, skip_special);
            }
        }
        _ => {
            for child in node.children() {
                collect_text(child, parts, skip_special);
            }
        }
    }
}

/// Python `extractors.element_text`: visible text, skipping
/// comments / scripts / styles / TTS controls, normalised.
pub fn element_text(node: NRef<'_>) -> Option<String> {
    let mut parts = Vec::new();
    collect_text(node, &mut parts, true);
    normalize_text(Some(&parts.join("")))
}

/// Raw concatenation of all descendant text (lxml `itertext()`).
pub fn raw_text(node: NRef<'_>) -> String {
    let mut parts = Vec::new();
    collect_text(node, &mut parts, false);
    parts.join("")
}

/// First comment node (incl. self, document order) whose stripped
/// text starts with `s-data:` — the SSR JSON comment.
pub fn sdata_json(node: NRef<'_>) -> serde_json::Value {
    for n in iter_incl(node) {
        if let Node::Comment(c) = n.value() {
            let text = c.comment.trim();
            if let Some(rest) = text.strip_prefix("s-data:") {
                return serde_json::from_str(rest)
                    .ok()
                    .filter(|v: &serde_json::Value| v.is_object())
                    .unwrap_or_else(|| serde_json::json!({}));
            }
        }
    }
    serde_json::json!({})
}

/// Parse a JSON-valued attribute into an object ({} on any failure).
pub fn parse_json_attr(node: NRef<'_>, name: &str) -> serde_json::Value {
    let raw = match attr(node, name) {
        Some(r) if !r.is_empty() => r,
        _ => return serde_json::json!({}),
    };
    serde_json::from_str(&raw)
        .ok()
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// First descendant element with `data-module == name`.
pub fn find_module<'a>(node: NRef<'a>, name: &str) -> Option<NRef<'a>> {
    descendant_elements(node)
        .into_iter()
        .find(|n| attr(*n, "data-module").as_deref() == Some(name))
}
