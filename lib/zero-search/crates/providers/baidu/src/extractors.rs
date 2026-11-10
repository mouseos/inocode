//! Field extraction from one `.c-result` element — faithful port of
//! Python `baidu_parser.extractors` (DOM based, lxml semantics
//! reproduced over the scraper tree in `crate::dom`).

use crate::dom::{self, NRef};
use crate::urls as urltools;
use serde_json::Value;

pub use crate::dom::normalize_text;

fn descendants<'a>(el: NRef<'a>) -> Vec<NRef<'a>> {
    dom::descendant_elements(el)
}

fn tag_is(node: NRef<'_>, tag: &str) -> bool {
    dom::tag_name(node).as_deref() == Some(tag)
}

fn json_str(v: &Value) -> Option<String> {
    v.as_str().map(|s| s.to_string())
}

// ---------------------------------------------------------------- titles

pub fn extract_title(el: NRef<'_>) -> Option<String> {
    let h3s: Vec<NRef> = descendants(el)
        .into_iter()
        .filter(|n| tag_is(*n, "h3"))
        .collect();
    if !h3s.is_empty() {
        return dom::element_text(h3s[0]);
    }
    match dom::find_module(el, "top_title") {
        Some(top) => dom::element_text(top),
        None => None,
    }
}

// ---------------------------------------------------------------- snippet

fn summary_from(module: Option<NRef<'_>>) -> (Option<String>, Option<String>) {
    let module = match module {
        Some(m) => m,
        None => return (None, None),
    };
    let prefix_nodes: Vec<NRef> = descendants(module)
        .into_iter()
        .filter(|n| dom::class_contains(*n, "prefix-time"))
        .collect();
    let prefix_text = if prefix_nodes.is_empty() {
        None
    } else {
        dom::element_text(prefix_nodes[0])
    };
    let spans: Vec<NRef> = descendants(module)
        .into_iter()
        .filter(|n| dom::class_contains(*n, "summary-text"))
        .collect();
    if !spans.is_empty() {
        return (dom::element_text(spans[0]), prefix_text);
    }
    // Fallback: whole module text with the prefix-time part removed.
    let mut text = dom::element_text(module);
    if let (Some(t), Some(p)) = (&text, &prefix_text) {
        if t.starts_with(p.as_str()) {
            text = dom::normalize_text(Some(&t[p.len()..]));
        }
    }
    (text, prefix_text)
}

/// (snippet, relative date label shown before the snippet).
pub fn extract_snippet(el: NRef<'_>) -> (Option<String>, Option<String>) {
    let module = dom::find_module(el, "abstract");
    let (snippet, prefix) = summary_from(module);
    if snippet.is_some() {
        return (snippet, prefix);
    }
    let module2 = dom::find_module(el, "pure-summary");
    let (snippet, prefix) = summary_from(module2);
    if snippet.is_some() {
        return (snippet, prefix);
    }
    // Last resort: the SSR summaryData in the abstract module comment.
    let abstract_mod = dom::find_module(el, "abstract");
    let data = abstract_mod
        .map(dom::sdata_json)
        .unwrap_or_else(|| Value::Object(Default::default()));
    let empty = Value::Object(Default::default());
    let summary = data
        .get("summaryData")
        .filter(|v| v.is_object())
        .unwrap_or(&empty);
    let texts = summary.get("summaryText").and_then(|v| v.as_array());
    if let Some(texts) = texts {
        if !texts.is_empty() {
            if let Some(first) = json_str(&texts[0]) {
                return (dom::normalize_text(Some(&first)), prefix);
            }
            return (None, prefix);
        }
    }
    (None, prefix)
}

// ---------------------------------------------------------------- source

/// (site_name, absolute date) from the source module's SSR data.
pub fn extract_source(el: NRef<'_>) -> (Option<String>, Option<String>) {
    let module = match dom::find_module(el, "source") {
        Some(m) => m,
        None => return (None, None),
    };
    let data = dom::sdata_json(module);
    let empty = Value::Object(Default::default());
    let source = data
        .get("sourceData")
        .filter(|v| v.is_object())
        .unwrap_or(&empty);
    let site = source
        .get("text")
        .and_then(|v| v.as_str())
        .and_then(|s| dom::normalize_text(Some(s)));
    let date = source
        .get("time")
        .and_then(|v| v.as_str())
        .and_then(|s| dom::normalize_text(Some(s)));
    (site, date)
}

// ---------------------------------------------------------------- links

/// The URL the result's main link points to, exactly as in the HTML.
pub fn extract_click_url(el: NRef<'_>, title: Option<&str>) -> Option<String> {
    let articles: Vec<NRef> = descendants(el)
        .into_iter()
        .filter(|n| tag_is(*n, "article") && dom::attr(*n, "rl-link-href").is_some())
        .collect();
    if !articles.is_empty() {
        return dom::attr(articles[0], "rl-link-href");
    }
    // Anchor wrapping the title heading.
    for a in descendants(el).into_iter().filter(|n| tag_is(*n, "a")) {
        if dom::attr(a, "href").is_none() {
            continue;
        }
        let has_h3 = dom::descendant_elements(a)
            .into_iter()
            .any(|n| tag_is(n, "h3"));
        if has_h3 {
            return dom::attr(a, "href");
        }
    }
    // Anchor whose visible text is the title.
    if let Some(title) = title {
        for a in descendants(el).into_iter().filter(|n| tag_is(*n, "a")) {
            if dom::attr(a, "href").is_none() {
                continue;
            }
            if dom::element_text(a).as_deref() == Some(title) {
                return dom::attr(a, "href");
            }
        }
    }
    None
}

/// (url, url_source, fusion_site) — only values the HTML itself states.
///
/// Primary: `data-log` JSON member `mu`. Secondary: the `url` member
/// of the article's `data-click-info` JSON `extra` object,
/// percent-decoded twice.
pub fn extract_external_url(el: NRef<'_>) -> (Option<String>, Option<String>, Option<String>) {
    let log = dom::parse_json_attr(el, "data-log");
    let mu = log
        .get("mu")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let mut fusion_site: Option<String> = None;
    let mut extra = Value::Object(Default::default());
    let articles: Vec<NRef> = descendants(el)
        .into_iter()
        .filter(|n| tag_is(*n, "article") && dom::attr(*n, "data-click-info").is_some())
        .collect();
    if !articles.is_empty() {
        let info = dom::parse_json_attr(articles[0], "data-click-info");
        if let Some(raw_extra) = info.get("extra") {
            if let Some(s) = raw_extra.as_str() {
                if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                    if parsed.is_object() {
                        extra = parsed;
                    }
                }
            } else if raw_extra.is_object() {
                extra = raw_extra.clone();
            }
        }
        fusion_site = extra
            .get("fusion_site")
            .and_then(|v| v.as_str())
            .and_then(|s| dom::normalize_text(Some(s)));
    }
    if let Some(mu) = &mu {
        if mu.starts_with("http://") || mu.starts_with("https://") {
            return (
                Some(mu.clone()),
                Some("data_log.mu".to_string()),
                fusion_site,
            );
        }
    }
    let encoded = extra.get("url").and_then(|v| v.as_str());
    if let Some(encoded) = encoded {
        if !encoded.is_empty() {
            let decoded = urltools::percent_decode(&urltools::percent_decode(encoded));
            if decoded.starts_with("http://") || decoded.starts_with("https://") {
                return (
                    Some(decoded),
                    Some("data_click_info.extra.url".to_string()),
                    fusion_site,
                );
            }
        }
    }
    (None, None, fusion_site)
}

// ---------------------------------------------------------------- media

pub fn extract_thumbnail(el: NRef<'_>) -> Option<String> {
    let source_module = dom::find_module(el, "source");
    for img in descendants(el).into_iter().filter(|n| tag_is(*n, "img")) {
        if let Some(sm) = source_module {
            if dom::is_within(img, sm) {
                continue; // source avatars are not result thumbnails
            }
        }
        let cls = dom::class_attr(img);
        if cls.contains("avatar") {
            continue;
        }
        let src = dom::attr(img, "src").filter(|s| !s.is_empty());
        let src = src.or_else(|| dom::attr(img, "data-src"));
        if let Some(s) = src {
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

// ---------------------------------------------------------------- sitelinks

pub fn extract_sitelinks(el: NRef<'_>, base_url: Option<&str>) -> Vec<(String, Option<String>)> {
    let mut links: Vec<(String, Option<String>)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for a in descendants(el).into_iter().filter(|n| tag_is(*n, "a")) {
        let dm = dom::attr(a, "data-module").unwrap_or_default();
        let is_subpos = dm.starts_with("subpos");
        let is_sublink = dom::class_contains(a, "sc-sublink");
        if !is_subpos && !is_sublink {
            continue;
        }
        let text = match dom::element_text(a) {
            Some(t) => t,
            None => continue,
        };
        if seen.contains(&text) {
            continue;
        }
        seen.push(text.clone());
        let url = urltools::resolve(dom::attr(a, "href").as_deref(), base_url);
        links.push((text, url));
    }
    links
}

// ---------------------------------------------------------------- cards

pub const AI_MARKERS: &[&str] = &["内容由AI生成", "由AI生成", "以上内容均由AI生成"];

pub fn has_ai_marker(el: NRef<'_>) -> bool {
    let text = dom::element_text(el).unwrap_or_default();
    AI_MARKERS.iter().any(|m| text.contains(m))
}

/// Main body text of a card (longest anchor text of >= 50 chars).
pub fn extract_card_text(el: NRef<'_>) -> Option<String> {
    let mut best: Option<String> = None;
    for a in descendants(el)
        .into_iter()
        .filter(|n| tag_is(*n, "a") && dom::attr(*n, "href").is_some())
    {
        if let Some(text) = dom::element_text(a) {
            if text.chars().count() >= 50
                && (best.is_none() || text.chars().count() > best.as_ref().unwrap().chars().count())
            {
                best = Some(text);
            }
        }
    }
    best
}

/// List entries of a card: anchors with meaningful text, in order.
pub fn extract_card_items(
    el: NRef<'_>,
    base_url: Option<&str>,
) -> Vec<(String, Option<String>, Option<String>)> {
    let mut items: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    let mut seen: Vec<(String, Option<String>)> = Vec::new();
    let source_module = dom::find_module(el, "source");
    for a in descendants(el)
        .into_iter()
        .filter(|n| tag_is(*n, "a") && dom::attr(*n, "href").is_some())
    {
        if let Some(sm) = source_module {
            if dom::is_within(a, sm) {
                continue;
            }
        }
        let text = match dom::element_text(a) {
            Some(t) => t,
            None => continue,
        };
        if text.chars().count() < 4 {
            continue;
        }
        let href = urltools::resolve(dom::attr(a, "href").as_deref(), base_url);
        let key = (text.clone(), href.clone());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        items.push((text, href, None));
        if items.len() >= 20 {
            break;
        }
    }
    items
}

// ---------------------------------------------------------------- related

/// Related searches from one recommend_list element or page-relative.
pub fn extract_related_from_block(
    el: NRef<'_>,
    base_url: Option<&str>,
) -> Vec<(String, Option<String>)> {
    let mut out: Vec<(String, Option<String>)> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for a in descendants(el)
        .into_iter()
        .filter(|n| tag_is(*n, "a") && dom::attr(*n, "href").is_some())
    {
        let text = match dom::element_text(a) {
            Some(t) => t,
            None => continue,
        };
        if seen.contains(&text) {
            continue;
        }
        seen.push(text.clone());
        let url = urltools::resolve(dom::attr(a, "href").as_deref(), base_url);
        out.push((text, url));
    }
    out
}
