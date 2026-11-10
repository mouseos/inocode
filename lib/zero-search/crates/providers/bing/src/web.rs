//! Bing web search via the app's native v7 search API
//! (`https://www.bingapis.com/api/v7/search`).
//!
//! The Bing app embeds this endpoint (APK `n38.java`, native-protocol
//! audit 2026-11-07) and uses it for related queries with an
//! app-embedded public `appid`. The response also carries the full web
//! result set — `webPages.value[]` with direct destination URLs (no
//! click-tracking redirect to unwrap), `totalEstimatedMatches`, and
//! `relatedSearches` — so `bing_web_search` consumes the API directly
//! instead of scraping the HTML results page. Note the distinction:
//! the app *displays* web results in a WebView page; this API's in-app
//! role is related queries only. Image / video / news have no
//! equivalent in the API response and still read the result pages,
//! exactly as the app displays them.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::urlbuild::{web_api_url, WEB_PER_PAGE};
use crate::{BingSearchClient, SearchResult};

/// One deep link (site link) nested under a web result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeepLink {
    pub name: String,
    pub url: String,
}

/// One organic web result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    /// The breadcrumb-style URL Bing displays (`example.com › page`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    /// Site links the API nests under the result (audit W-14).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deep_links: Vec<DeepLink>,
    /// ISO-8601 crawl timestamp (`dateLastCrawled`, audit W-14).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date_last_crawled: Option<String>,
    /// Content language code (`language`, audit W-14).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_family_friendly: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_navigational: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
}

/// Parsed web search API response: items plus the paging facts and the
/// related queries the envelope needs.
#[derive(Debug, Clone)]
pub struct WebResults {
    pub items: Vec<WebItem>,
    pub total_hits: Option<i64>,
    pub has_next_page: bool,
    pub related_queries: Vec<String>,
    /// `webPages.someResultsRemoved`: the API's own flag that some
    /// results were withheld (audit W-14).
    pub some_results_removed: Option<bool>,
}

/// Parse a v7 search API response (offline-testable). `limit` caps the
/// returned items; ranks are 1-based *within the page* (the client
/// re-bases them).
pub fn parse_web_results(value: &Value, limit: usize) -> Result<WebResults> {
    let Some(web_pages) = value.get("webPages") else {
        // No result block at all: the API contract changed, or an error
        // payload was served instead of a search response.
        return Err(anyhow!(
            "web results not found in Bing API response (response format may have changed)"
        ));
    };
    let total_hits = web_pages
        .get("totalEstimatedMatches")
        .and_then(|t| t.as_i64());

    let mut items = Vec::new();
    if let Some(results) = web_pages.get("value").and_then(|v| v.as_array()) {
        for entry in results {
            if items.len() >= limit {
                break;
            }
            let title = entry
                .get("name")
                .and_then(|t| t.as_str())
                .map(clean_text)
                .unwrap_or_default();
            let url = entry
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or("")
                .to_string();
            if title.is_empty() || url.is_empty() {
                continue;
            }
            let display_url = entry
                .get("displayUrl")
                .and_then(|d| d.as_str())
                .map(clean_text)
                .filter(|s| !s.is_empty());
            let snippet = entry
                .get("snippet")
                .and_then(|s| s.as_str())
                .map(clean_text)
                .filter(|s| !s.is_empty());
            let deep_links = entry
                .get("deepLinks")
                .and_then(|d| d.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|d| {
                            let name = d
                                .get("name")
                                .and_then(|n| n.as_str())
                                .map(clean_text)
                                .filter(|s| !s.is_empty())?;
                            let url = d
                                .get("url")
                                .and_then(|u| u.as_str())
                                .filter(|s| !s.is_empty())?
                                .to_string();
                            Some(DeepLink { name, url })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let raw_str = |key: &str| {
                entry
                    .get(key)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
            };
            items.push(WebItem {
                rank: items.len() as i64 + 1,
                title,
                url,
                display_url,
                snippet,
                deep_links,
                date_last_crawled: raw_str("dateLastCrawled"),
                language: raw_str("language"),
                is_family_friendly: entry.get("isFamilyFriendly").and_then(|v| v.as_bool()),
                is_navigational: entry.get("isNavigational").and_then(|v| v.as_bool()),
                thumbnail_url: raw_str("thumbnailUrl"),
            });
        }
    }

    let has_next_page = value
        .get("pagination")
        .and_then(|p| p.get("nextPage"))
        .is_some();

    let mut related_queries = Vec::new();
    if let Some(related) = value
        .get("relatedSearches")
        .and_then(|r| r.get("value"))
        .and_then(|v| v.as_array())
    {
        for entry in related {
            let text = entry
                .get("text")
                .and_then(|t| t.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| entry.get("displayText").and_then(|t| t.as_str()))
                .map(clean_text)
                .unwrap_or_default();
            if !text.is_empty() {
                related_queries.push(text);
            }
        }
    }

    Ok(WebResults {
        items,
        total_hits,
        has_next_page,
        related_queries,
        some_results_removed: web_pages
            .get("someResultsRemoved")
            .and_then(|v| v.as_bool()),
    })
}

impl BingSearchClient {
    /// Search the web index through the v7 search API. `page` is
    /// 1-based (converted to the API's `offset`); `limit` is clamped
    /// to 10, the page size.
    pub async fn web_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
        mkt: &str,
        setlang: &str,
    ) -> Result<SearchResult<WebItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, WEB_PER_PAGE);
        let url = web_api_url(query, page, mkt, setlang);
        let json = self.get_json(&url).await?;
        let parsed = parse_web_results(&json, limit)?;
        let mut items = parsed.items;
        for item in &mut items {
            item.rank += (page - 1) * WEB_PER_PAGE;
        }
        let count = items.len();
        let note = if parsed.some_results_removed == Some(true) {
            Some(
                "Bing reports that some results were removed for this query (someResultsRemoved)"
                    .to_string(),
            )
        } else if count == 0 {
            Some("Bing returned no results".to_string())
        } else {
            None
        };
        Ok(SearchResult::new("web", query, page, items)
            .with_total(parsed.total_hits)
            .with_next_page(parsed.has_next_page.then_some(page + 1))
            .with_related_queries(parsed.related_queries)
            .with_note_opt(note))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_full_response_reads_items_total_and_related() {
        let value = json!({
            "_type": "SearchResponse",
            "webPages": {
                "totalEstimatedMatches": 258000,
                "value": [
                    {"name": "Example", "url": "https://example.com/", "displayUrl": "https://example.com", "snippet": "An example."},
                    {"name": "Second", "url": "https://example.org/page"}
                ]
            },
            "relatedSearches": {"value": [{"text": "example query"}, {"text": "example two"}]},
            "pagination": {"currentPageNumber": 1, "nextPage": {"pageNumber": 2, "offset": 10}}
        });
        let parsed = parse_web_results(&value, 10).unwrap();
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[0].rank, 1);
        assert_eq!(parsed.items[0].title, "Example");
        assert_eq!(parsed.items[0].url, "https://example.com/");
        assert_eq!(
            parsed.items[0].display_url.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(parsed.items[0].snippet.as_deref(), Some("An example."));
        assert_eq!(parsed.items[1].display_url, None);
        assert_eq!(parsed.total_hits, Some(258000));
        assert!(parsed.has_next_page);
        assert_eq!(parsed.related_queries, vec!["example query", "example two"]);
    }

    #[test]
    fn parse_respects_limit() {
        let value = json!({
            "webPages": {"value": [
                {"name": "A", "url": "https://a.example/"},
                {"name": "B", "url": "https://b.example/"},
                {"name": "C", "url": "https://c.example/"}
            ]}
        });
        let parsed = parse_web_results(&value, 2).unwrap();
        assert_eq!(parsed.items.len(), 2);
        assert_eq!(parsed.items[1].rank, 2);
        assert!(!parsed.has_next_page);
        assert_eq!(parsed.total_hits, None);
    }

    #[test]
    fn parse_reads_extended_fields_and_removal_flag() {
        // Keys mirror the real fixture fixture-web-ja.json (audit W-14).
        let value = json!({
            "webPages": {
                "someResultsRemoved": true,
                "totalEstimatedMatches": 10,
                "value": [{
                    "name": "Fast",
                    "url": "https://fast.com/",
                    "displayUrl": "https://fast.com",
                    "snippet": "speed",
                    "deepLinks": [{"name": "Share", "url": "https://fast.com/share"}],
                    "dateLastCrawled": "2026-11-06T14:59:00.0000000Z",
                    "language": "ja",
                    "isFamilyFriendly": true,
                    "isNavigational": true
                }, {
                    "name": "Thumb",
                    "url": "https://thumb.example/",
                    "thumbnailUrl": "https://ts.example/th?id=1"
                }]
            }
        });
        let parsed = parse_web_results(&value, 10).unwrap();
        assert_eq!(parsed.some_results_removed, Some(true));
        let first = &parsed.items[0];
        assert_eq!(first.deep_links.len(), 1);
        assert_eq!(first.deep_links[0].url, "https://fast.com/share");
        assert_eq!(
            first.date_last_crawled.as_deref(),
            Some("2026-11-06T14:59:00.0000000Z")
        );
        assert_eq!(first.language.as_deref(), Some("ja"));
        assert_eq!(first.is_family_friendly, Some(true));
        assert_eq!(first.is_navigational, Some(true));
        assert_eq!(
            parsed.items[1].thumbnail_url.as_deref(),
            Some("https://ts.example/th?id=1")
        );
        // Absent fields stay absent (no new keys serialized).
        let v = serde_json::to_value(&parsed.items[1]).unwrap();
        assert!(v.get("deep_links").is_none());
        assert!(v.get("language").is_none());
    }

    /// Audit W-19: only offsets up to page 2 were measured when
    /// this shipped. Verify a deep page (5, offset=40) live, once,
    /// on demand.
    #[tokio::test]
    #[ignore = "live network test"]
    async fn live_web_deep_page() {
        let client = BingSearchClient::new().unwrap();
        let result = client
            .web_search("Rust", 5, 10, "ja-JP", "ja")
            .await
            .unwrap();
        assert!(!result.items.is_empty(), "page 5 returned no items");
        assert!(result.items.iter().all(|i| i.rank > 40));
    }

    #[test]
    fn parse_missing_web_pages_is_a_format_error() {
        let value = json!({"_type": "ErrorResponse", "errors": []});
        let err = parse_web_results(&value, 10).unwrap_err();
        assert!(err.to_string().contains("may have changed"));
    }

    #[test]
    fn parse_empty_value_is_ok_with_no_items() {
        let value = json!({"webPages": {"totalEstimatedMatches": 0, "value": []}});
        let parsed = parse_web_results(&value, 10).unwrap();
        assert!(parsed.items.is_empty());
        assert_eq!(parsed.total_hits, Some(0));
        assert!(parsed.related_queries.is_empty());
    }
}
