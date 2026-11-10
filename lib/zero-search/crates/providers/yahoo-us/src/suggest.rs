//! Yahoo (US) search suggestions — the classic "gossip" autocomplete API.
//!
//! The Yahoo Scout app's built-in SearchAssist suggestions sit behind a
//! crumb (CSRF token) flow that returns `{"crumb":null}` for anonymous
//! callers, so they cannot be used without a login session. The SERP's own
//! gossip endpoint needs no token at all:
//! `GET https://search.yahoo.com/sugg/gossip/gossip-us-ura/?command=<q>&output=json`
//! answers `{"gossip":{"results":[{"key": "..."}]}}` anonymously. Only the
//! `gossip-us-ura` entry point works — `gossip-all` is rejected by the
//! service.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::YahooUsSearchClient;

const REFERER: &str = "https://search.yahoo.com/";

/// One autocomplete suggestion, with the web-search URL for that keyword.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub keyword: String,
    pub search_url: String,
    /// The gossip response's `mrk` value for this candidate, kept
    /// verbatim (audit W-33). Its semantics are the service's own —
    /// the analysis never pinned down what it measures (every
    /// captured response carried `mrk: 6`), so it is surfaced
    /// uninterpreted rather than dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mrk: Option<i64>,
}

/// Response envelope for [`YahooUsSearchClient::suggest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResult {
    // Owned strings, like `SearchResult`: `&'static str` here would make the
    // result impossible for downstream users to deserialise back.
    pub provider: String,
    pub vertical: String,
    pub ok: bool,
    pub query: String,
    pub count: usize,
    pub suggestions: Vec<Suggestion>,
}

impl YahooUsSearchClient {
    /// Get search suggestions for a (partial) query via the gossip API.
    /// `limit` is clamped to 20. An empty query is an error.
    pub async fn suggest(&self, query: &str, limit: i64) -> Result<SuggestResult> {
        let query = query.trim();
        if query.is_empty() {
            return Err(anyhow!("query must not be empty"));
        }
        let limit = clamp_limit(limit, 20);

        let url = crate::build_search_url(query, "suggest", 1)?;
        let body = self.get_json(&url, REFERER).await?;
        Ok(parse_suggest_response(&body, query, limit))
    }
}

/// Turn a decoded gossip response body into a [`SuggestResult`]. Offline.
pub fn parse_suggest_response(body: &Value, query: &str, limit: usize) -> SuggestResult {
    let suggestions = body
        .get("gossip")
        .and_then(|g| g.get("results"))
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    let keyword = r
                        .get("key")
                        .and_then(|k| k.as_str())
                        .map(clean_text)
                        .filter(|s| !s.is_empty())?;
                    let search_url = crate::build_search_url(&keyword, "web", 1).ok()?;
                    Some(Suggestion {
                        keyword,
                        search_url,
                        mrk: r.get("mrk").and_then(|m| m.as_i64()),
                    })
                })
                .take(limit)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    SuggestResult {
        provider: crate::PROVIDER.to_string(),
        vertical: "suggest".to_string(),
        ok: true,
        query: query.to_string(),
        count: suggestions.len(),
        suggestions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/fixture-suggest.json");

    #[test]
    fn parses_gossip_fixture() {
        let body: Value = serde_json::from_str(FIXTURE).unwrap();
        let result = parse_suggest_response(&body, "app", 20);
        assert!(result.ok);
        assert_eq!(result.provider, "yahoo_us");
        assert_eq!(result.count, 10);
        assert_eq!(result.suggestions[0].keyword, "app store");
        assert!(result.suggestions[0]
            .search_url
            .starts_with("https://search.yahoo.com/search?p=app%20store"));
        // The fixture contains an escaped apostrophe (applebee\u0027s).
        assert!(result
            .suggestions
            .iter()
            .any(|s| s.keyword == "applebee's grill + bar"));
        // Audit W-33: every fixture result carries `mrk: 6`.
        assert!(result.suggestions.iter().all(|s| s.mrk == Some(6)));
    }

    #[test]
    fn limit_is_respected() {
        let body: Value = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(parse_suggest_response(&body, "app", 3).count, 3);
    }

    #[test]
    fn malformed_body_yields_empty_suggestions() {
        let result = parse_suggest_response(&serde_json::json!({"nope": 1}), "x", 10);
        assert_eq!(result.count, 0);
    }
}
