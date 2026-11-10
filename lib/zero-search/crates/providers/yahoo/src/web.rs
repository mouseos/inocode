//! Yahoo! JAPAN web search (`https://search.yahoo.co.jp/search`).
//!
//! Results come from `__NEXT_DATA__ -> props.pageProps.initialProps.pageData`,
//! the same payload the page hydrates React from.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::{SearchResult, YahooSearchClient};

const BASE: &str = "https://search.yahoo.co.jp/search";
const REFERER: &str = "https://www.yahoo.co.jp/";
/// Yahoo returns 10 organic results per page and paginates by 1-based offset.
const PER_PAGE: i64 = 10;

/// One organic web result: rank, title, destination URL and snippet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
    /// Payload entry type for entries that are not ordinary
    /// `"Algo"` results (audit W-09). Ordinary results leave this
    /// `None`; other typed entries the page returns are surfaced
    /// with their type verbatim instead of being silently dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_type: Option<String>,
}

/// Value accepted by the `updated` argument, mapped to Yahoo's `vd` parameter.
/// Yahoo offers exactly three windows — day, week and year. There is no month
/// option: `vd=m` is accepted by the URL but silently ignored, so it must not
/// be advertised.
fn updated_param(updated: &str) -> Option<&'static str> {
    match updated.trim().to_lowercase().as_str() {
        "day" | "d" | "24h" => Some("d"),
        "week" | "w" => Some("w"),
        "year" | "y" => Some("y"),
        _ => None,
    }
}

impl YahooSearchClient {
    /// Search the web index.
    ///
    /// `site` restricts results to one host by appending a `site:` operator —
    /// Yahoo honours it as part of the query string rather than a parameter.
    pub async fn web_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
        updated: &str,
        site: &str,
    ) -> Result<SearchResult<WebItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);

        let mut effective_query = query.trim().to_string();
        let site = site.trim();
        if !site.is_empty() {
            // Audit W-12: previously the `site` argument was
            // silently ignored when the query already contained a
            // `site:` operator. Refuse the ambiguity explicitly.
            if effective_query.contains("site:") {
                return Err(anyhow!(
                    "site argument '{site}' conflicts with a site: operator already present in the query; pass only one of them"
                ));
            }
            effective_query = format!("{effective_query} site:{site}");
        }

        let offset = (page - 1) * PER_PAGE + 1;
        let mut url = format!(
            "{BASE}?p={}&ei=UTF-8&b={offset}",
            urlencode(&effective_query)
        );
        if let Some(vd) = updated_param(updated) {
            url.push_str(&format!("&vd={vd}"));
        }

        let html = self.get_html(&url, REFERER).await?;
        parse_web_page(&html, query, page, limit as i64)
    }
}

/// Parse a saved web-search page (offline; audit W-10).
pub fn parse_web_page(
    html: &str,
    query: &str,
    page: i64,
    limit: i64,
) -> Result<SearchResult<WebItem>> {
    let page = page.max(1);
    let limit = clamp_limit(limit, PER_PAGE);
    let data = extract_next_data(html)?;
    let page_data = get_path(&data, &["props", "pageProps", "initialProps", "pageData"])
        .cloned()
        .unwrap_or(Value::Null);

    let items = parse_algos(&page_data, limit);
    let total = get_path(&page_data, &["pager", "hits"]).and_then(|v| v.as_i64());
    let has_next = get_path(&page_data, &["pager", "nextPage"])
        .and_then(|v| v.as_str())
        .is_some();

    Ok(SearchResult::new("web", query, page, items)
        .with_total(total)
        .with_next_page(has_next.then_some(page + 1))
        .with_related(parse_related(&page_data)))
}

fn parse_algos(page_data: &Value, limit: usize) -> Vec<WebItem> {
    page_data
        .get("algos")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    // Audit W-09: entries whose payload `type` is
                    // not "Algo" used to be discarded without a
                    // trace. Surface them, marked with their type.
                    let entry_type = a.get("type").and_then(|t| t.as_str())?;
                    let mut item = parse_algo(a)?;
                    if entry_type != "Algo" {
                        item.entry_type = Some(entry_type.to_string());
                    }
                    Some(item)
                })
                .take(limit)
                .collect()
        })
        .unwrap_or_default()
}

fn parse_algo(algo: &Value) -> Option<WebItem> {
    let url = get_raw(algo, "url")?;
    Some(WebItem {
        rank: get_i64(algo, "index").unwrap_or(0),
        title: get_str(algo, "title").unwrap_or_else(|| url.clone()),
        url,
        description: get_str(algo, "description"),
        site: get_str(algo, "citeString"),
        entry_type: None,
    })
}

/// Yahoo scatters "people also searched" queries across every organic result;
/// collect them in page order and de-duplicate.
fn parse_related(page_data: &Value) -> Vec<String> {
    let mut seen = Vec::new();
    let Some(algos) = page_data.get("algos").and_then(|v| v.as_array()) else {
        return seen;
    };
    for algo in algos {
        let Some(queries) =
            get_path(algo, &["anotherSuggest", "exploreQueries"]).and_then(|v| v.as_array())
        else {
            continue;
        };
        for q in queries {
            if let Some(text) = get_str(q, "query") {
                if !seen.contains(&text) {
                    seen.push(text);
                }
            }
        }
    }
    seen.truncate(20);
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn updated_param_maps_aliases() {
        assert_eq!(updated_param("day"), Some("d"));
        assert_eq!(updated_param("Week"), Some("w"));
        assert_eq!(updated_param("year"), Some("y"));
        assert_eq!(updated_param(""), None);
        assert_eq!(updated_param("all"), None);
        assert_eq!(updated_param("nonsense"), None);
        // Yahoo has no month window — it must not silently pass through.
        assert_eq!(updated_param("month"), None);
    }

    #[test]
    fn parse_algo_skips_entries_without_url() {
        assert!(parse_algo(&serde_json::json!({"title": "x"})).is_none());
    }

    #[test]
    fn parse_algos_surfaces_non_algo_entries_with_type() {
        // Audit W-09: non-"Algo" entries must not vanish silently.
        let page_data = serde_json::json!({"algos": [
            {"type": "Algo", "index": 1, "title": "A", "url": "https://a.jp/"},
            {"type": "KnowledgePanel", "index": 2, "title": "B", "url": "https://b.jp/"}
        ]});
        let items = parse_algos(&page_data, 10);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].entry_type, None);
        assert_eq!(items[1].entry_type.as_deref(), Some("KnowledgePanel"));
        // Ordinary results serialize exactly as before (no new key).
        let v = serde_json::to_value(&items[0]).unwrap();
        assert!(v.get("entry_type").is_none());
    }
}
