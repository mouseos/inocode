//! Bing trending topics, via the app's native endpoint
//! `GET https://www.bing.com/api/v7/news/trendingtopics`
//! (APK `rfa.java`, native-protocol audit §5; audit W-16).
//!
//! The app calls this for its trending display with an app-embedded
//! public `appid`, `cc` (country) and `setlang`.
//!
//! Live status (2026-11-08): from the verification egress the
//! endpoint answered **HTTP 401** to the anonymous appid request,
//! and a warm-cookie retry hit a connection timeout — so anonymous
//! availability is *not* confirmed; the app presumably relies on
//! its authenticated session state. The client and parser are
//! implemented and offline-tested; callers see the server's real
//! answer. The response's
//! `value[]` entries carry `name`, `webSearchUrl`, `newsSearchUrl`,
//! `image.url` and `description`; some entries nest a `query`
//! object (`displayText` / `query` / `webSearchUrl` /
//! `image.thumbnailUrl`) instead — both shapes are flattened here.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::BingSearchClient;

/// App-embedded public identifier for the trending-topics API
/// (APK `rfa.java`). It ships inside the app; not a user credential.
pub const TRENDING_APPID: &str = "91B36E34F9D1B900E54E85A77CF11FB3BE5279E6";
/// Trending-topics API base (the app's own endpoint).
pub const TRENDING_BASE: &str = "https://www.bing.com/api/v7/news/trendingtopics";

/// One trending topic.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrendingTopic {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub web_search_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub news_search_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_breaking_news: Option<bool>,
}

/// Build the trending-topics URL. Empty market/language fall back
/// to Japan / Japanese, like the rest of the provider.
pub fn trending_url(mkt: &str, setlang: &str) -> String {
    let mkt = if mkt.trim().is_empty() {
        "ja-JP"
    } else {
        mkt.trim()
    };
    let cc = match mkt.rsplit('-').next() {
        Some(cc) if cc.len() == 2 => cc.to_uppercase(),
        _ => "JP".to_string(),
    };
    let lang = if setlang.trim().is_empty() {
        "ja"
    } else {
        setlang.trim()
    };
    format!(
        "{TRENDING_BASE}?appid={TRENDING_APPID}&cc={cc}&setlang={}",
        urlencode(lang)
    )
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(clean_text)
        .filter(|s| !s.is_empty())
}

fn raw_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Parse a trending-topics API response (offline-testable).
pub fn parse_trending_topics(value: &Value, limit: usize) -> Vec<TrendingTopic> {
    let Some(entries) = value.get("value").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries {
        if out.len() >= limit {
            break;
        }
        if let Some(query) = entry.get("query").filter(|q| q.is_object()) {
            // Nested form: the topic text lives in the query object.
            let name = str_field(query, "displayText")
                .or_else(|| str_field(entry, "name"))
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            out.push(TrendingTopic {
                name,
                query: raw_field(query, "query"),
                web_search_url: raw_field(query, "webSearchUrl"),
                news_search_url: raw_field(entry, "newsSearchUrl"),
                image_url: query
                    .get("image")
                    .and_then(|i| raw_field(i, "thumbnailUrl").or_else(|| raw_field(i, "url"))),
                description: str_field(entry, "description"),
                is_breaking_news: entry.get("isBreakingNews").and_then(|b| b.as_bool()),
            });
            continue;
        }
        let Some(name) = str_field(entry, "name") else {
            continue;
        };
        out.push(TrendingTopic {
            name,
            query: None,
            web_search_url: raw_field(entry, "webSearchUrl"),
            news_search_url: raw_field(entry, "newsSearchUrl"),
            image_url: entry.get("image").and_then(|i| raw_field(i, "url")),
            description: str_field(entry, "description"),
            is_breaking_news: entry.get("isBreakingNews").and_then(|b| b.as_bool()),
        });
    }
    out
}

impl BingSearchClient {
    /// Fetch the current trending topics. `limit` is clamped to 50.
    pub async fn trending_topics(
        &self,
        limit: i64,
        mkt: &str,
        setlang: &str,
    ) -> Result<Vec<TrendingTopic>> {
        let limit = clamp_limit(limit, 50);
        let json = self.get_json(&trending_url(mkt, setlang)).await?;
        Ok(parse_trending_topics(&json, limit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trending_url_shape() {
        let url = trending_url("", "");
        assert!(url.starts_with(
            "https://www.bing.com/api/v7/news/trendingtopics?appid=91B36E34F9D1B900E54E85A77CF11FB3BE5279E6&cc=JP&setlang=ja"
        ));
        assert!(trending_url("en-US", "en").contains("&cc=US&setlang=en"));
    }

    #[test]
    fn parse_flat_and_nested_entries() {
        let value = json!({
            "_type": "NewsTrendingTopicsResponse",
            "value": [
                {"name": "Topic A", "webSearchUrl": "https://www.bing.com/search?q=A",
                 "newsSearchUrl": "https://www.bing.com/news/search?q=A",
                 "image": {"url": "https://img.example/a.jpg"},
                 "description": "About A", "isBreakingNews": true},
                {"query": {"displayText": "Topic B", "query": "B",
                           "webSearchUrl": "https://www.bing.com/search?q=B",
                           "image": {"thumbnailUrl": "https://img.example/b.jpg"}}}
            ]
        });
        let topics = parse_trending_topics(&value, 10);
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].name, "Topic A");
        assert_eq!(topics[0].is_breaking_news, Some(true));
        assert_eq!(
            topics[0].image_url.as_deref(),
            Some("https://img.example/a.jpg")
        );
        assert_eq!(topics[1].name, "Topic B");
        assert_eq!(topics[1].query.as_deref(), Some("B"));
        assert_eq!(
            topics[1].image_url.as_deref(),
            Some("https://img.example/b.jpg")
        );
        assert!(parse_trending_topics(&json!({}), 10).is_empty());
    }
}
