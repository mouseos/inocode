//! Entry point: parse a Baidu (APK / wise) search-result HTML page.
//! Faithful port of Python `baidu_parser.parser`.
//! The parser performs no network access.

use crate::classifiers;
use crate::dom;
use crate::extractors;
use crate::models::{
    CardEntry, CardItem, PageInfo, RelatedSearch, ResultEntry, SearchPage, SiteLink,
    UnknownElement, PARSER_VERSION,
};
use crate::pagination::extract_pagination;
use crate::urls as urltools;
use scraper::Html;
use serde_json::{json, Map};

fn int_or_none(value: Option<String>) -> Option<i64> {
    value.and_then(|v| v.trim().parse::<i64>().ok())
}

fn build_result(
    el: dom::NRef<'_>,
    category: &str,
    base_url: Option<&str>,
    position: i64,
) -> ResultEntry {
    let title = extractors::extract_title(el);
    let (url, url_source, fusion_site) = extractors::extract_external_url(el);
    let (snippet, prefix_date) = extractors::extract_snippet(el);
    let (site_name, sdata_date) = extractors::extract_source(el);
    let click_raw = extractors::extract_click_url(el, title.as_deref());
    let click_url = urltools::resolve(click_raw.as_deref(), base_url);
    let domain = urltools::host_of(url.as_deref());
    let (date, date_source) = if sdata_date.is_some() {
        (sdata_date, Some("source_sdata".to_string()))
    } else if prefix_date.is_some() {
        (prefix_date, Some("abstract_prefix_time".to_string()))
    } else {
        (None, None)
    };
    ResultEntry {
        entry_type: if category == "ad" { "ad" } else { "organic" }.to_string(),
        display_position: int_or_none(dom::attr(el, "order")),
        position_in_page: Some(position),
        organic_rank: None,
        tpl: dom::attr(el, "tpl"),
        srcid: dom::attr(el, "srcid"),
        new_srcid: dom::attr(el, "new_srcid"),
        title,
        url: url.clone(),
        url_source,
        click_url_kind: urltools::classify_click_url(click_url.as_deref()),
        is_baidu_redirect: urltools::is_baidu_redirect(click_url.as_deref()),
        click_url,
        display_url: fusion_site.or_else(|| domain.clone()),
        domain,
        snippet,
        site_name,
        date,
        date_source,
        thumbnail: urltools::resolve(extractors::extract_thumbnail(el).as_deref(), base_url),
        sitelinks: extractors::extract_sitelinks(el, base_url)
            .into_iter()
            .map(|(text, url)| SiteLink { text, url })
            .collect(),
    }
}

fn build_card(el: dom::NRef<'_>, kind: &str, base_url: Option<&str>, position: i64) -> CardEntry {
    let title = extractors::extract_title(el);
    let (mut url, _src, _fusion) = extractors::extract_external_url(el);
    let (site_name, sdata_date) = extractors::extract_source(el);
    let (_snippet, prefix_date) = extractors::extract_snippet(el);
    // Pseudo targets such as nourl.ubs.baidu.com are placeholders.
    if let Some(u) = &url {
        let host = urltools::host_of(Some(u)).unwrap_or_default();
        if host.ends_with(".ubs.baidu.com") {
            url = None;
        }
    }
    // Audit W-04: item and body-text extraction used to run only
    // for a few card kinds, dropping the contents of the others
    // (weather, dictionary, ai_answer, …). Both extractors are
    // content-driven (they return nothing when the card carries no
    // matching anchors), so run them for every kind.
    let items: Vec<CardItem> = extractors::extract_card_items(el, base_url)
        .into_iter()
        .map(|(title, url, thumbnail)| CardItem {
            title: Some(title),
            url,
            thumbnail,
        })
        .collect();
    let click_url = urltools::resolve(
        extractors::extract_click_url(el, title.as_deref()).as_deref(),
        base_url,
    );
    let thumbnail = urltools::resolve(extractors::extract_thumbnail(el).as_deref(), base_url);
    let text = extractors::extract_card_text(el);
    CardEntry {
        entry_type: "card".to_string(),
        card_kind: Some(kind.to_string()),
        display_position: int_or_none(dom::attr(el, "order")),
        position_in_page: Some(position),
        tpl: dom::attr(el, "tpl"),
        srcid: dom::attr(el, "srcid"),
        new_srcid: dom::attr(el, "new_srcid"),
        title,
        text,
        url,
        click_url,
        thumbnail,
        site_name,
        date: sdata_date.or(prefix_date),
        ai_generated_marker: kind == "ai_answer" && extractors::has_ai_marker(el),
        items,
    }
}

/// Parse one search-result HTML page into a `SearchPage`.
/// Accepts raw bytes (UTF-8, as the measured pages are) exactly like
/// the Python parser accepts both `str` and `bytes`.
pub fn parse_search_results_bytes(
    raw: &[u8],
    query: Option<&str>,
    page_url: Option<&str>,
) -> SearchPage {
    let html = String::from_utf8_lossy(raw).into_owned();
    parse_search_results(&html, query, page_url)
}

/// Parse one search-result HTML page (UTF-8 text form of
/// [`parse_search_results_bytes`]). Performs no network access.
pub fn parse_search_results(html: &str, query: Option<&str>, page_url: Option<&str>) -> SearchPage {
    // lxml (libxml2) honours XML-style self-closing tags on ordinary
    // elements; html5ever ignores them. Normalise first so both
    // parsers build the same tree (see crate::preprocess).
    let html = crate::preprocess::preprocess_html(html);
    let doc = Html::parse_document(&html);
    let root = doc.tree.root();

    let mut page = SearchPage {
        parser_version: PARSER_VERSION.to_string(),
        query: query.map(|s| s.to_string()),
        source_url: page_url.map(|s| s.to_string()),
        ..Default::default()
    };
    let mut warnings: Vec<String> = Vec::new();

    let all = dom::all_elements(root);
    let has_results = all
        .iter()
        .any(|n| dom::attr(*n, "id").as_deref() == Some("results"));
    if !has_results {
        warnings.push("no #results container found".to_string());
    }
    let elements: Vec<dom::NRef> = all
        .into_iter()
        .filter(|n| dom::has_class_token(*n, "c-result"))
        .collect();
    if elements.is_empty() {
        warnings.push("no .c-result elements found".to_string());
    }

    let mut related: Vec<RelatedSearch> = Vec::new();
    let mut seen_related: Vec<String> = Vec::new();
    let add_related = |entries: Vec<(String, Option<String>)>,
                       source: &str,
                       related: &mut Vec<RelatedSearch>,
                       seen_related: &mut Vec<String>| {
        for (q, url) in entries {
            if !seen_related.contains(&q) {
                seen_related.push(q.clone());
                related.push(RelatedSearch {
                    query: q,
                    url,
                    source: Some(source.to_string()),
                });
            }
        }
    };

    let mut organic_rank: i64 = 0;
    let mut counts: Map<String, serde_json::Value> = Map::new();
    for k in ["organic", "ad", "card", "related_block", "unknown"] {
        counts.insert(k.to_string(), json!(0));
    }
    let mut card_kinds: Map<String, serde_json::Value> = Map::new();
    let bump = |counts: &mut Map<String, serde_json::Value>, key: &str| {
        if let Some(v) = counts.get_mut(key) {
            *v = json!(v.as_i64().unwrap_or(0) + 1);
        }
    };

    for (idx, el) in elements.iter().enumerate() {
        let position = idx as i64 + 1;
        if dom::attr(*el, "order").is_none() {
            warnings.push(format!(
                "c-result at position {} has no order attribute",
                position
            ));
        }
        let (category, detail) = classifiers::classify(*el);
        match category.as_str() {
            "organic" => {
                let mut entry = build_result(*el, &category, page_url, position);
                organic_rank += 1;
                entry.organic_rank = Some(organic_rank);
                page.results.push(entry);
                bump(&mut counts, "organic");
            }
            "ad" => {
                let entry = build_result(*el, &category, page_url, position);
                page.ads.push(entry);
                bump(&mut counts, "ad");
            }
            "card" => {
                let card = build_card(*el, &detail, page_url, position);
                page.cards.push(card);
                bump(&mut counts, "card");
                let n = card_kinds
                    .get(&detail)
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                card_kinds.insert(detail.clone(), json!(n + 1));
            }
            "related" => {
                let entries = extractors::extract_related_from_block(*el, page_url);
                add_related(entries, "recommend_list", &mut related, &mut seen_related);
                bump(&mut counts, "related_block");
            }
            _ => {
                // Audit W-05: run the generic extraction used for
                // known results so unknown-template results keep
                // their title / URL / snippet.
                let (u_url, _src, _fusion) = extractors::extract_external_url(*el);
                let (u_snippet, _prefix) = extractors::extract_snippet(*el);
                page.unknown_elements.push(UnknownElement {
                    display_position: int_or_none(dom::attr(*el, "order")),
                    position_in_page: Some(position),
                    tpl: dom::attr(*el, "tpl"),
                    srcid: dom::attr(*el, "srcid"),
                    reason: detail,
                    title: extractors::extract_title(*el),
                    url: u_url,
                    snippet: u_snippet,
                });
                bump(&mut counts, "unknown");
            }
        }
    }

    // Related searches also appear in the page footer block.
    for block in dom::all_elements(root)
        .into_iter()
        .filter(|n| dom::class_contains(*n, "page-relative"))
    {
        let entries = extractors::extract_related_from_block(block, page_url);
        add_related(entries, "page_relative", &mut related, &mut seen_related);
    }

    page.related_searches = related;
    page.page = extract_pagination(root, page_url);

    if let Some(q) = query {
        if !q.is_empty() {
            if let Some(next_url) = &page.page.next_url {
                if let Some(next_word) = urltools::word_of(Some(next_url)) {
                    if next_word != q {
                        warnings.push(format!(
                            "next_url word '{}' differs from query '{}'",
                            next_word, q
                        ));
                    }
                }
            }
        }
    }

    let mut diagnostics = Map::new();
    diagnostics.insert("c_result_count".to_string(), json!(elements.len()));
    diagnostics.insert("classified".to_string(), serde_json::Value::Object(counts));
    diagnostics.insert(
        "card_kinds".to_string(),
        serde_json::Value::Object(card_kinds),
    );
    diagnostics.insert("warnings".to_string(), json!(warnings));
    page.diagnostics = serde_json::Value::Object(diagnostics);
    page
}

/// Keep the Python name for the page-info type reachable from here.
pub type _PageInfoAlias = PageInfo;
