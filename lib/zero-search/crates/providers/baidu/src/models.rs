//! Data models — faithful port of Python `baidu_parser.models` and
//! `baidu_search.models`.
//!
//! `to_value()` output matches the Python `to_dict()` field names,
//! field order, types and null representation exactly. Unknown
//! values are `None` (JSON null); absent collections are empty lists.

use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};

pub const PARSER_NAME: &str = "baidu_parser";
pub const PARSER_VERSION: &str = "1.0.0";
pub const CLIENT_NAME: &str = "baidu_search";
pub const CLIENT_VERSION: &str = "1.0.0";

// ---------------------------------------------------------------- parser

/// A sitelink attached to a [`ResultEntry`].
#[derive(Debug, Clone, Default, Serialize)]
pub struct SiteLink {
    pub text: String,
    pub url: Option<String>,
}

/// One item inside a [`CardEntry`] (for example a video or note entry).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CardItem {
    pub title: Option<String>,
    pub url: Option<String>,
    pub thumbnail: Option<String>,
}

/// One organic or advertising result parsed from a `.c-result` element.
#[derive(Debug, Clone, Serialize)]
pub struct ResultEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // "organic" | "ad"
    pub display_position: Option<i64>,
    pub position_in_page: Option<i64>,
    pub organic_rank: Option<i64>,
    pub tpl: Option<String>,
    pub srcid: Option<String>,
    pub new_srcid: Option<String>,
    pub title: Option<String>,
    pub url: Option<String>,
    pub url_source: Option<String>,
    pub click_url: Option<String>,
    pub click_url_kind: Option<String>,
    pub is_baidu_redirect: bool,
    pub display_url: Option<String>,
    pub domain: Option<String>,
    pub snippet: Option<String>,
    pub site_name: Option<String>,
    pub date: Option<String>,
    pub date_source: Option<String>,
    pub thumbnail: Option<String>,
    pub sitelinks: Vec<SiteLink>,
}

impl ResultEntry {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// One card result (weather, knowledge graph, video, …) parsed from a `.c-result` element.
#[derive(Debug, Clone, Serialize)]
pub struct CardEntry {
    #[serde(rename = "type")]
    pub entry_type: String, // always "card"
    pub card_kind: Option<String>,
    pub display_position: Option<i64>,
    pub position_in_page: Option<i64>,
    pub tpl: Option<String>,
    pub srcid: Option<String>,
    pub new_srcid: Option<String>,
    pub title: Option<String>,
    pub text: Option<String>,
    pub url: Option<String>,
    pub click_url: Option<String>,
    pub thumbnail: Option<String>,
    pub site_name: Option<String>,
    pub date: Option<String>,
    pub ai_generated_marker: bool,
    pub items: Vec<CardItem>,
}

impl CardEntry {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// A related-search suggestion found on the page.
#[derive(Debug, Clone, Serialize)]
pub struct RelatedSearch {
    pub query: String,
    pub url: Option<String>,
    pub source: Option<String>,
}

/// A `.c-result` element whose template could not be classified.
/// The generic title / URL / snippet extraction applied to known
/// results also runs for these elements (audit W-05), so a result
/// served under a template the classifier does not know yet stays
/// visible instead of being reduced to metadata only.
#[derive(Debug, Clone, Serialize)]
pub struct UnknownElement {
    pub display_position: Option<i64>,
    pub position_in_page: Option<i64>,
    pub tpl: Option<String>,
    pub srcid: Option<String>,
    pub reason: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub snippet: Option<String>,
}

/// Pagination information for a parsed page (`pn`, page number, next/previous URLs).
#[derive(Debug, Clone, Serialize)]
pub struct PageInfo {
    pub pn: i64,
    pub page_number: i64,
    pub is_first_page: bool,
    pub next_url: Option<String>,
    pub previous_url: Option<String>,
    pub first_url: Option<String>,
    pub next_pn: Option<i64>,
    pub previous_pn: Option<i64>,
}

impl Default for PageInfo {
    fn default() -> Self {
        PageInfo {
            pn: 0,
            page_number: 1,
            is_first_page: true,
            next_url: None,
            previous_url: None,
            first_url: None,
            next_pn: None,
            previous_pn: None,
        }
    }
}

/// One parsed Baidu search-result page: results, ads, cards, related searches and diagnostics.
///
/// Produced by [`crate::parse_search_results`] / [`crate::parse_search_results_bytes`].
/// Use [`SearchPage::to_value`] for the JSON shape identical to the Python parser.
#[derive(Debug, Clone)]
pub struct SearchPage {
    pub parser_name: String,
    pub parser_version: String,
    pub query: Option<String>,
    pub source_url: Option<String>,
    pub page: PageInfo,
    pub results: Vec<ResultEntry>,
    pub ads: Vec<ResultEntry>,
    pub cards: Vec<CardEntry>,
    pub related_searches: Vec<RelatedSearch>,
    pub unknown_elements: Vec<UnknownElement>,
    pub diagnostics: Value,
}

impl Default for SearchPage {
    fn default() -> Self {
        SearchPage {
            parser_name: PARSER_NAME.to_string(),
            parser_version: String::new(),
            query: None,
            source_url: None,
            page: PageInfo::default(),
            results: Vec::new(),
            ads: Vec::new(),
            cards: Vec::new(),
            related_searches: Vec::new(),
            unknown_elements: Vec::new(),
            diagnostics: json!({}),
        }
    }
}

impl SearchPage {
    /// Python `SearchPage.to_dict()`.
    pub fn to_value(&self) -> Value {
        json!({
            "parser": {"name": self.parser_name, "version": self.parser_version},
            "query": self.query,
            "source_url": self.source_url,
            "page": serde_json::to_value(&self.page).unwrap_or(Value::Null),
            "results": self.results.iter().map(|r| r.to_value()).collect::<Vec<_>>(),
            "ads": self.ads.iter().map(|r| r.to_value()).collect::<Vec<_>>(),
            "cards": self.cards.iter().map(|c| c.to_value()).collect::<Vec<_>>(),
            "related_searches": serde_json::to_value(&self.related_searches).unwrap_or(Value::Null),
            "unknown_elements": serde_json::to_value(&self.unknown_elements).unwrap_or(Value::Null),
            "diagnostics": self.diagnostics,
        })
    }
    pub fn diagnostics_warnings(&self) -> Vec<String> {
        self.diagnostics
            .get("warnings")
            .and_then(|w| w.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn c_result_count(&self) -> i64 {
        self.diagnostics
            .get("c_result_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------- client

/// Mirror of Python `baidu_search.models.PageRecord`.
#[derive(Debug, Clone, Serialize)]
pub struct PageRecord {
    pub page_number: Option<i64>,
    pub pn: Option<i64>,
    pub source_url: Option<String>,
    pub final_url: Option<String>,
    pub status_code: Option<i64>,
    pub next_url: Option<String>,
    pub counts: Map<String, Value>,
    pub parser_warnings: Vec<String>,
    pub error: Option<Value>,
}

impl Default for PageRecord {
    fn default() -> Self {
        PageRecord {
            page_number: None,
            pn: None,
            source_url: None,
            final_url: None,
            status_code: None,
            next_url: None,
            counts: Map::new(),
            parser_warnings: Vec::new(),
            error: None,
        }
    }
}

impl PageRecord {
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Mirror of Python `baidu_search.models.SinglePageResult`.
#[derive(Debug, Clone)]
pub struct SinglePageResult {
    pub query: Option<String>,
    pub source_url: String,
    pub final_url: Option<String>,
    pub status_code: Option<i64>,
    pub page: SearchPage,
    pub warnings: Vec<String>,
}

impl SinglePageResult {
    /// Python `SinglePageResult.to_dict()`: parser dict + `fetch`.
    pub fn to_value(&self) -> Value {
        let mut data = self.page.to_value();
        if let Some(obj) = data.as_object_mut() {
            obj.insert(
                "fetch".to_string(),
                json!({
                    "source_url": self.source_url,
                    "final_url": self.final_url,
                    "status_code": self.status_code,
                    "client_warnings": self.warnings,
                }),
            );
        }
        data
    }
}

/// Mirror of Python `baidu_search.models.AggregatedSearch`.
#[derive(Debug, Clone)]
pub struct AggregatedSearch {
    pub query: String,
    pub pages_requested: i64,
    pub pages_fetched: i64,
    pub results: Vec<Value>,
    pub ads: Vec<Value>,
    pub cards: Vec<Value>,
    pub related_searches: Vec<Value>,
    pub unknown_elements: Vec<Value>,
    pub pages: Vec<PageRecord>,
    pub errors: Vec<Value>,
    pub warnings: Vec<String>,
    pub duplicate_items: Vec<Value>,
}

impl AggregatedSearch {
    /// Python `AggregatedSearch.to_dict()`.
    pub fn to_value(&self) -> Value {
        json!({
            "client": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
            "query": self.query,
            "pages_requested": self.pages_requested,
            "pages_fetched": self.pages_fetched,
            "results": self.results,
            "ads": self.ads,
            "cards": self.cards,
            "related_searches": self.related_searches,
            "unknown_elements": self.unknown_elements,
            "pages": self.pages.iter().map(|p| p.to_value()).collect::<Vec<_>>(),
            "diagnostics": {
                "duplicates": self.duplicate_items.len(),
                "duplicate_items": self.duplicate_items,
                "errors": self.errors,
                "warnings": self.warnings,
            },
        })
    }
}

fn annotate(entry: &Value, single: &SinglePageResult) -> Value {
    let mut out = entry.clone();
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "page_number".to_string(),
            json!(single.page.page.page_number),
        );
        obj.insert("page_pn".to_string(), json!(single.page.page.pn));
        obj.insert("source_page_url".to_string(), json!(single.source_url));
    }
    out
}

fn dedup_key(entry: &Value) -> Option<String> {
    // Python: entry.get("url") or entry.get("click_url")
    let url = entry.get("url").and_then(|v| v.as_str());
    if let Some(u) = url {
        if !u.is_empty() {
            return Some(u.to_string());
        }
    }
    // Python `or`: None and "" are both falsy; a non-string truthy
    // value would be returned as-is, but url/click_url are str|None.
    entry
        .get("click_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

#[allow(clippy::too_many_arguments)]
fn absorb_ranked(
    target: &mut Vec<Value>,
    entries: &[Value],
    single: &SinglePageResult,
    seen: &mut HashMap<String, Value>,
    duplicates: &mut Vec<Value>,
    global_rank_counter: &mut i64,
    ranked: bool,
) {
    for entry in entries {
        let mut item = annotate(entry, single);
        if ranked {
            *global_rank_counter += 1;
            if let Some(obj) = item.as_object_mut() {
                obj.insert(
                    "global_organic_rank".to_string(),
                    json!(*global_rank_counter),
                );
            }
        }
        let key = dedup_key(&item);
        match &key {
            Some(k) if seen.contains_key(k) => {
                let first_page_number = seen
                    .get(k)
                    .and_then(|v| v.get("page_number").cloned())
                    .unwrap_or(Value::Null);
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("duplicate".to_string(), json!(true));
                }
                duplicates.push(json!({
                    "key": k,
                    "first_page_number": first_page_number,
                    "duplicate_page_number": item.get("page_number").cloned().unwrap_or(Value::Null),
                    "title": item.get("title").cloned().unwrap_or(Value::Null),
                }));
            }
            _ => {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert("duplicate".to_string(), json!(false));
                }
                if let Some(k) = key {
                    seen.insert(k, item.clone());
                }
            }
        }
        target.push(item);
    }
}

/// Mirror of Python `baidu_search.models.aggregate`.
pub fn aggregate(
    query: &str,
    pages_requested: i64,
    singles: &[SinglePageResult],
    errors: &[Value],
    warnings: &[String],
    failed_pages: &[PageRecord],
) -> AggregatedSearch {
    let mut agg = AggregatedSearch {
        query: query.to_string(),
        pages_requested,
        pages_fetched: 0,
        results: Vec::new(),
        ads: Vec::new(),
        cards: Vec::new(),
        related_searches: Vec::new(),
        unknown_elements: Vec::new(),
        pages: Vec::new(),
        errors: errors.to_vec(),
        warnings: warnings.to_vec(),
        duplicate_items: Vec::new(),
    };

    let mut seen: HashMap<String, Value> = HashMap::new();
    let mut rank_counter: i64 = 0;
    let mut seen_related: HashSet<String> = HashSet::new();

    for single in singles {
        let page = &single.page;
        let mut counts = Map::new();
        counts.insert("results".to_string(), json!(page.results.len()));
        counts.insert("ads".to_string(), json!(page.ads.len()));
        counts.insert("cards".to_string(), json!(page.cards.len()));
        counts.insert(
            "related_searches".to_string(),
            json!(page.related_searches.len()),
        );
        counts.insert(
            "unknown_elements".to_string(),
            json!(page.unknown_elements.len()),
        );
        agg.pages.push(PageRecord {
            page_number: Some(page.page.page_number),
            pn: Some(page.page.pn),
            source_url: Some(single.source_url.clone()),
            final_url: single.final_url.clone(),
            status_code: single.status_code,
            next_url: page.page.next_url.clone(),
            counts,
            parser_warnings: page.diagnostics_warnings(),
            error: None,
        });
        let result_values: Vec<Value> = page.results.iter().map(|r| r.to_value()).collect();
        absorb_ranked(
            &mut agg.results,
            &result_values,
            single,
            &mut seen,
            &mut agg.duplicate_items,
            &mut rank_counter,
            true,
        );
        let ad_values: Vec<Value> = page.ads.iter().map(|r| r.to_value()).collect();
        absorb_ranked(
            &mut agg.ads,
            &ad_values,
            single,
            &mut seen,
            &mut agg.duplicate_items,
            &mut rank_counter,
            false,
        );
        for card in &page.cards {
            agg.cards.push(annotate(&card.to_value(), single));
        }
        for rel in &page.related_searches {
            if seen_related.contains(&rel.query) {
                continue;
            }
            seen_related.insert(rel.query.clone());
            let mut item = json!({
                "query": rel.query,
                "url": rel.url,
                "source": rel.source,
            });
            if let Some(obj) = item.as_object_mut() {
                obj.insert("page_number".to_string(), json!(page.page.page_number));
                obj.insert("page_pn".to_string(), json!(page.page.pn));
                obj.insert(
                    "source_page_url".to_string(),
                    json!(single.source_url.clone()),
                );
            }
            agg.related_searches.push(item);
        }
        for unk in &page.unknown_elements {
            let v = serde_json::to_value(unk).unwrap_or(Value::Null);
            agg.unknown_elements.push(annotate(&v, single));
        }
        for w in &single.warnings {
            if !agg.warnings.contains(w) {
                agg.warnings.push(w.clone());
            }
        }
    }

    if !failed_pages.is_empty() {
        agg.pages.extend(failed_pages.iter().cloned());
        // Python: sort by (page_number is None, page_number or 0) —
        // stable sort keeps fetch order among equal keys.
        agg.pages.sort_by(|a, b| {
            let ka = (a.page_number.is_none(), a.page_number.unwrap_or(0));
            let kb = (b.page_number.is_none(), b.page_number.unwrap_or(0));
            ka.cmp(&kb)
        });
    }

    agg.pages_fetched = singles.len() as i64;
    agg
}
