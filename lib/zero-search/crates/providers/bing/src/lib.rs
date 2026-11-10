//! Microsoft Bing search client library — web, image, video and news
//! search plus search suggestions, implemented in pure Rust.
//!
//! This crate is the provider library behind the **Bing_MCP** server
//! (stdio JSON-RPC). It is modelled on what the official Bing Android app
//! does, which an APK analysis established in detail. The app *displays*
//! results in a WebView shell around
//! `https://www.bing.com/search?q=%s&form=%s&sdkhh=1`, so image, video
//! and news still fetch those same server-rendered pages here. Web
//! search instead calls Bing's native v7 search API
//! (`https://www.bingapis.com/api/v7/search`), which the app embeds
//! with a public `appid` for related queries: its response carries the
//! full web result set, so the API is consumed directly. Suggestions
//! likewise use the app's own suggestions API. No user API key,
//! account, or signature is involved anywhere.
//!
//! Two details decide whether Bing answers at all:
//!
//! * The market must be pinned (`mkt`/`setlang` on the pages, `cc` /
//!   `setlang` on the APIs). Without them Bing mis-detects the market
//!   and returns an empty "no results" response. Empty arguments fall
//!   back to `ja-JP` / `ja`.
//! * Image search additionally needs the app-style treatment — a mobile
//!   user agent plus `form=L2GV01&sdkhh=1` — because the desktop image
//!   page loads its results with JavaScript only. [`BingSearchClient::
//!   image_search`] handles that internally.
//!
//! # Example (offline: build URLs and parse a saved API response)
//!
//! ```
//! use bing_search::{build_search_url, parse_web_results};
//!
//! let url = build_search_url("テスト", "web", "ja-JP", "ja").unwrap();
//! assert!(url.starts_with("https://www.bing.com/search?q="));
//!
//! let json: serde_json::Value = serde_json::json!({
//!     "webPages": {
//!         "totalEstimatedMatches": 1234,
//!         "value": [{"name": "Example", "url": "https://example.com/",
//!                    "displayUrl": "https://example.com", "snippet": "Snippet."}]
//!     },
//!     "relatedSearches": {"value": [{"text": "example query"}]}
//! });
//! let page = parse_web_results(&json, 10).unwrap();
//! assert_eq!(page.items.len(), 1);
//! assert_eq!(page.items[0].url, "https://example.com/");
//! assert_eq!(page.related_queries, vec!["example query".to_string()]);
//! ```
//!
//! # Example (live search; not run in tests)
//!
//! ```no_run
//! use bing_search::BingSearchClient;
//!
//! # async fn run() -> anyhow::Result<()> {
//! let client = BingSearchClient::new()?;
//! let web = client.web_search("Rust", 1, 5, "ja-JP", "ja").await?;
//! println!("{} results", web.count);
//! let images = client.image_search("猫", 1, 12, "ja-JP", "ja").await?;
//! println!("{} images", images.count);
//! let suggestions = client.suggest("テスト", "web", "ja-JP", "ja").await?;
//! println!("{} suggestions", suggestions.count);
//! # Ok(())
//! # }
//! ```

pub mod common;
pub mod image;
pub mod news;
pub mod shop;
pub mod suggest;
pub mod trending;
pub mod urlbuild;
pub mod video;
pub mod visual;
pub mod web;

use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, REFERER};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

pub use common::{clean_text, decode_entities, urlencode, MOBILE_USER_AGENT, USER_AGENT};
pub use image::{parse_image_results, ImageItem, ImageResults};
pub use news::{parse_news_results, NewsItem};
pub use shop::{parse_shop_results, ShopItem, ShopResults};
pub use suggest::{parse_suggest_json, SuggestResult, Suggestion};
pub use trending::{parse_trending_topics, trending_url, TrendingTopic};
pub use urlbuild::{build_page_url, build_search_url, suggest_url, web_api_url};
pub use video::{parse_video_results, VideoItem, VideoResults};
pub use visual::{knowledge_request_json, parse_visual_response, visual_search_url, VisualTag};
pub use web::{parse_web_results, DeepLink, WebItem, WebResults};

/// Provider identifier used in every result envelope.
pub const PROVIDER: &str = "bing";

/// The verticals this provider exposes. Image search is supported through
/// the app-style (mobile) form of the page — see the crate docs.
pub fn verticals() -> Value {
    json!({
        "provider": PROVIDER,
        "verticals": [
            { "id": "web", "label": "Web search (native v7 search API)", "tool": "bing_web_search", "supported": true },
            { "id": "image", "label": "Image search (app-style page, mobile UA)", "tool": "bing_image_search", "supported": true },
            { "id": "video", "label": "Video search", "tool": "bing_video_search", "supported": true },
            { "id": "news", "label": "News search (first page only)", "tool": "bing_news_search", "supported": true },
            { "id": "suggest", "label": "Search suggestions (web / images / videos)", "tool": "bing_suggest", "supported": true }
        ],
        "builder_tool": "bing_build_search_url",
        "list_tool": "bing_list_verticals",
        "unsupported": []
    })
}

/// Shared result envelope, structurally identical to the Baidu / Yahoo
/// providers so MCP clients see one shape across all three.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult<T> {
    pub provider: String,
    pub vertical: String,
    pub query: String,
    pub page: i64,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_hits: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_page: Option<i64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_queries: Vec<String>,
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl<T> SearchResult<T> {
    pub fn new(vertical: &str, query: &str, page: i64, items: Vec<T>) -> Self {
        Self {
            provider: PROVIDER.to_string(),
            vertical: vertical.to_string(),
            query: query.trim().to_string(),
            page,
            count: items.len(),
            total_hits: None,
            next_page: None,
            related_queries: Vec::new(),
            items,
            note: None,
        }
    }

    pub fn with_total(mut self, total_hits: Option<i64>) -> Self {
        self.total_hits = total_hits;
        self
    }

    pub fn with_next_page(mut self, next_page: Option<i64>) -> Self {
        self.next_page = next_page;
        self
    }

    pub fn with_related_queries(mut self, related_queries: Vec<String>) -> Self {
        self.related_queries = related_queries;
        self
    }

    pub fn with_note_opt(mut self, note: Option<String>) -> Self {
        self.note = note;
        self
    }
}

fn build_http(user_agent: &'static str) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::USER_AGENT,
        HeaderValue::from_static(user_agent),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,application/json;q=0.8,*/*;q=0.7",
        ),
    );
    headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static(common::ACCEPT_LANGUAGE),
    );
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .cookie_store(true)
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .build()?)
}

/// Bing search client. Holds two HTTP stacks — desktop UA for the
/// JSON APIs and the video / news pages, mobile UA for image search —
/// sharing one cookie jar each. Construct once and reuse.
#[derive(Debug, Clone)]
pub struct BingSearchClient {
    http: reqwest::Client,
    http_mobile: reqwest::Client,
}

impl BingSearchClient {
    pub fn new() -> Result<Self> {
        Ok(Self {
            http: build_http(common::USER_AGENT)?,
            http_mobile: build_http(common::MOBILE_USER_AGENT)?,
        })
    }

    async fn get_with(&self, client: &reqwest::Client, url: &str, referer: &str) -> Result<String> {
        // One retry on HTTP 429 after a pause: Bing's quota is per
        // appid + egress IP and shared, so a transient 429 often
        // clears within seconds (Z12: a facade search hit 429 while
        // spaced identical searches succeeded). The retry waits —
        // it does not change identity or shape — and a second 429
        // surfaces as the usual access_restricted error.
        for attempt in 0..=1 {
            let resp = client
                .get(url)
                .header(REFERER, referer)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("connection_error: {e}"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
            if !status.is_success() {
                return Err(common::status_error(status));
            }
            return resp
                .text()
                .await
                .map_err(|e| anyhow::anyhow!("connection_error: {e}"));
        }
        unreachable!("the retry loop always returns")
    }

    /// Fetch a page with the desktop user agent.
    pub async fn get_html(&self, url: &str, referer: &str) -> Result<String> {
        self.get_with(&self.http, url, referer).await
    }

    /// Fetch a page with the mobile user agent (image search).
    pub async fn get_html_mobile(&self, url: &str, referer: &str) -> Result<String> {
        self.get_with(&self.http_mobile, url, referer).await
    }

    /// POST a multipart form and return the body text (visual
    /// search). Mirrors the app's pre-auth state: the
    /// `Authorization` header carries an empty bearer, exactly as
    /// the app sends before its auth flow completes.
    pub async fn http_post_multipart(
        &self,
        url: &str,
        form: reqwest::multipart::Form,
    ) -> Result<String> {
        let resp = self
            .http
            .post(url)
            .header(REFERER, "https://www.bing.com")
            .header("Authorization", "Bearer ")
            .multipart(form)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("connection_error: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(common::status_error(status));
        }
        resp.text()
            .await
            .map_err(|e| anyhow::anyhow!("connection_error: {e}"))
    }

    /// Fetch a JSON document (the app's APIs: v7 search, suggestions).
    /// Retries once on HTTP 429 after a pause, like
    /// [`Self::get_with`].
    pub async fn get_json(&self, url: &str) -> Result<Value> {
        for attempt in 0..=1 {
            let resp = self
                .http
                .get(url)
                .header(REFERER, "https://www.bing.com/")
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("connection_error: {e}"))?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS && attempt == 0 {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
            if !status.is_success() {
                return Err(common::status_error(status));
            }
            return resp
                .json::<Value>()
                .await
                .map_err(|e| anyhow::anyhow!("parse_error: {e}"));
        }
        unreachable!("the retry loop always returns")
    }
}

impl Default for BingSearchClient {
    fn default() -> Self {
        Self::new().expect("build Bing HTTP client")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verticals_lists_all_five_supported() {
        let v = verticals();
        let list = v["verticals"].as_array().unwrap();
        let ids: Vec<&str> = list.iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["web", "image", "video", "news", "suggest"]);
        assert!(list.iter().all(|x| x["supported"].as_bool() == Some(true)));
    }

    #[test]
    fn envelope_omits_empty_optionals() {
        let result: SearchResult<WebItem> = SearchResult::new("web", "q", 1, vec![]);
        let text = serde_json::to_string(&result).unwrap();
        assert!(!text.contains("total_hits"));
        assert!(!text.contains("next_page"));
        assert!(text.contains("\"count\":0"));
    }
}
