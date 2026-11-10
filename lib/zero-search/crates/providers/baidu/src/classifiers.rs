//! Classification of `.c-result` elements — faithful port of Python
//! `baidu_parser.classifiers`.
//!
//! Ad signals (visible 广告/廣告 leaf label, or EC ad markup classes)
//! take priority over the `tpl` template map. Unknown or missing
//! templates are reported as unknown — never guessed into organic.

use crate::dom::{self, NRef};
use scraper::node::Node;
use std::collections::HashSet;

pub const ORGANIC_TPLS: &[&str] = &["www_index", "www_specific", "www_kv"];

pub fn card_kind_of(tpl: &str) -> Option<&'static str> {
    Some(match tpl {
        "weather_forecast_new_san" => "weather",
        "sg_kg_entity_san" => "knowledge_graph",
        "bk_polysemy_san" => "baike",
        "fy_sg_dict_new_san" => "dictionary",
        "wenda_generate" => "ai_answer",
        "vid_recommend" => "video",
        "live_converge_san" => "live",
        "tieba_forumcommentxml_san" => "forum",
        "note_lead" => "note",
        "guanfanghao_san" => "official_account",
        _ => return None,
    })
}

pub const RELATED_TPLS: &[&str] = &["recommend_list_san"];
pub const AD_LABEL_TEXTS: &[&str] = &["广告", "廣告"];
pub const AD_CLASS_TOKENS: &[&str] = &["ec_wise_ad", "ec_ad_results"];

fn class_tokens(el: NRef<'_>) -> HashSet<String> {
    let mut tokens = HashSet::new();
    for node in dom::iter_incl(el) {
        if let Node::Element(_) = node.value() {
            for t in dom::class_attr(node).split_whitespace() {
                tokens.insert(t.to_string());
            }
        }
    }
    tokens
}

fn has_element_children(node: NRef<'_>) -> bool {
    node.children()
        .any(|c| matches!(c.value(), Node::Element(_)))
}

/// Direct text of an element (lxml `node.text`: text before the
/// first child; for a leaf element that is all of its text).
fn direct_text(node: NRef<'_>) -> String {
    let mut out = String::new();
    for child in node.children() {
        match child.value() {
            Node::Text(t) => out.push_str(&t.text),
            _ => break,
        }
    }
    // Element with no children at all: its own text nodes are the
    // children handled above. (lxml stores it as .text.)
    out
}

/// (is_ad, evidence) from structural signals only.
pub fn has_ad_signals(el: NRef<'_>) -> (bool, String) {
    // 1) Visible ad label: a leaf element whose entire text is 广告/廣告.
    for node in dom::iter_incl(el) {
        if !matches!(node.value(), Node::Element(_)) {
            continue;
        }
        if !has_element_children(node) {
            // Python: (node.text or "").strip() — for a leaf element
            // all text is direct text; a leaf that only holds a
            // comment child has len(node) > 0 in lxml, so also
            // require no children of any kind beyond text.
            let has_non_text_child = node.children().any(|c| !matches!(c.value(), Node::Text(_)));
            if has_non_text_child {
                continue;
            }
            let text = direct_text(node);
            let text = text.trim();
            if AD_LABEL_TEXTS.contains(&text) {
                return (true, format!("visible ad label '{}'", text));
            }
        }
    }
    // 2) EC ad markup classes anywhere inside the result element.
    for token in class_tokens(el) {
        if AD_CLASS_TOKENS.contains(&token.as_str())
            || token.starts_with("ec_")
            || token.starts_with("ec-")
        {
            return (true, format!("ec ad class '{}'", token));
        }
    }
    (false, String::new())
}

/// (category, detail): ("organic", tpl) | ("ad", evidence) |
/// ("card", card_kind) | ("related", tpl) | ("unknown", reason).
pub fn classify(el: NRef<'_>) -> (String, String) {
    let (ad, evidence) = has_ad_signals(el);
    if ad {
        return ("ad".to_string(), evidence);
    }
    let tpl = dom::attr(el, "tpl");
    match tpl.as_deref() {
        Some(t) if ORGANIC_TPLS.contains(&t) => ("organic".to_string(), t.to_string()),
        Some(t) => {
            if let Some(kind) = card_kind_of(t) {
                return ("card".to_string(), kind.to_string());
            }
            if RELATED_TPLS.contains(&t) {
                return ("related".to_string(), t.to_string());
            }
            ("unknown".to_string(), format!("unrecognised tpl '{}'", t))
        }
        None => ("unknown".to_string(), "missing tpl attribute".to_string()),
    }
}
