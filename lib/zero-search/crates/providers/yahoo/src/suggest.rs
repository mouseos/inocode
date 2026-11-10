//! Yahoo! JAPAN keyword autocomplete.
//!
//! The suggest backend wants an `appid`. That value is *not* a user secret —
//! Yahoo embeds it verbatim in the HTML of its own public search page, which is
//! where the browser search box reads it from too. We do the same: scrape it on
//! first use, cache it for the process lifetime, and fall back to the last
//! known-good value if the page layout shifts. That keeps the server zero-config.

use anyhow::{anyhow, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::YahooSearchClient;

/// The app's suggest endpoint (audit W-08). The Android app calls
/// `assist-search.yahooapis.jp` with `device=smartphone` and
/// `.src=ytop`; the response format is identical to the web host's.
/// `gen` / `yob` are only sent for signed-in users (they carry
/// profile data), so the anonymous reproduction omits them, as the
/// native-protocol audit prescribes.
const ENDPOINT: &str = "https://assist-search.yahooapis.jp/SuggestSearchService/V5/webassistSearch";
const REFERER: &str = "https://search.yahoo.co.jp/";

/// Build the suggest request URL in the app's parameter form.
pub fn build_suggest_url(appid: &str, query: &str) -> String {
    format!(
        "{ENDPOINT}?appid={}&query={}&device=smartphone&.src=ytop",
        urlencode(appid),
        urlencode(query)
    )
}
/// Page scraped for the public `appid`; any query works, so keep it trivial.
const APPID_SOURCE: &str = "https://search.yahoo.co.jp/search?p=a&ei=UTF-8";
/// Last known-good public appid, used only if scraping fails.
const FALLBACK_APPID: &str = "dj0zaiZpPVU5MGlSOUZ4cHVLbCZzPWNvbnN1bWVyc2VjcmV0Jng9ZGQ-";

static APPID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""appid"\s*:\s*"([A-Za-z0-9_\-]+)""#).unwrap());

/// One autocomplete suggestion, with the web-search URL for that keyword.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub keyword: String,
    pub search_url: String,
}

/// Response envelope for [`YahooSearchClient::suggest_keywords`].
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
    /// Where the `appid` used for this call came from: `"page"`
    /// (scraped from Yahoo's public search page) or `"fallback"`
    /// (the built-in last-known-good constant, audit W-11). The
    /// fallback is surfaced explicitly instead of silently
    /// degrading to empty suggestions when it goes stale.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub appid_source: String,
}

impl YahooSearchClient {
    /// Get keyword autocomplete suggestions for a (partial) query.
    /// `limit` is clamped to 20. An empty query is an error.
    pub async fn suggest_keywords(&self, query: &str, limit: i64) -> Result<SuggestResult> {
        let query = query.trim();
        if query.is_empty() {
            return Err(anyhow!("query must not be empty"));
        }
        let limit = clamp_limit(limit, 20);
        let (appid, appid_source) = self.suggest_appid().await;

        let url = build_suggest_url(&appid, query);
        let body = self.get_json(&url, REFERER).await?;

        let suggestions: Vec<Suggestion> = body
            .get("Result")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|r| {
                        let keyword = get_str(r, "Suggest")?;
                        let search_url = get_raw(r, "Url").unwrap_or_else(|| {
                            format!(
                                "https://search.yahoo.co.jp/search?p={}&ei=UTF-8",
                                urlencode(&keyword)
                            )
                        });
                        Some(Suggestion {
                            keyword,
                            search_url,
                        })
                    })
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();

        Ok(SuggestResult {
            provider: crate::PROVIDER.to_string(),
            vertical: "suggest".to_string(),
            ok: true,
            query: query.to_string(),
            count: suggestions.len(),
            suggestions,
            appid_source,
        })
    }

    /// Resolve the public suggest appid, scraping once and caching
    /// thereafter. Returns the value and its source (`"page"` or
    /// `"fallback"`, audit W-11).
    async fn suggest_appid(&self) -> (String, String) {
        let mut cached = self.suggest_appid.lock().await;
        if let Some(pair) = cached.as_ref() {
            return pair.clone();
        }
        let pair = match self
            .get_html(APPID_SOURCE, REFERER)
            .await
            .ok()
            .and_then(|html| extract_appid(&html))
        {
            Some(id) => (id, "page".to_string()),
            None => (FALLBACK_APPID.to_string(), "fallback".to_string()),
        };
        *cached = Some(pair.clone());
        pair
    }
}

fn extract_appid(html: &str) -> Option<String> {
    APPID_RE
        .captures(html)
        .map(|c| c[1].to_string())
        .filter(|s| s.len() > 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_appid_reads_embedded_value() {
        let html = r#"...{"options":{"appid":"dj0zaiZpPVRFU1Q-","logid":"x"}}..."#;
        assert_eq!(extract_appid(html).as_deref(), Some("dj0zaiZpPVRFU1Q-"));
    }

    #[test]
    fn extract_appid_rejects_short_values() {
        assert!(extract_appid(r#"{"appid":"short"}"#).is_none());
        assert!(extract_appid("no appid here").is_none());
    }

    #[test]
    fn suggest_url_uses_app_parameter_form() {
        // Audit W-08: app host, device=smartphone, .src=ytop; no
        // device=pc / output=json, and no gen/yob anonymously.
        let url = build_suggest_url("dj0zaiZpPVRFU1Q-", "ラーメン");
        assert!(url.starts_with(
            "https://assist-search.yahooapis.jp/SuggestSearchService/V5/webassistSearch?"
        ));
        assert!(url.contains("device=smartphone"));
        assert!(url.contains("&.src=ytop"));
        assert!(!url.contains("device=pc"));
        assert!(!url.contains("output=json"));
        assert!(!url.contains("gen="));
        assert!(!url.contains("yob="));
    }
}
