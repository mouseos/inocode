//! Naver search client — usable both as the search engine of the
//! Naver_MCP server and as a standalone Rust library.
//!
//! Everything here is modelled on what the official Naver Android app
//! does, established by APK analysis: the app is a WebView shell around
//! `https://m.search.naver.com/search.naver?where=m&ie=utf8&query=...&sm=...`
//! — there is no signed native search API — so this library fetches
//! Naver's server-rendered mobile pages and extracts the results. No API
//! key, account or signature is involved, and requests are fully
//! anonymous. The client deliberately keeps **no cookie store**:
//! measured behaviour showed that requests carrying the cookies a
//! previous response set stall until the timeout, while stateless
//! requests answer immediately.
//!
//! Three measured facts shape the implementation:
//!
//! * Only the **mobile host + a mobile user agent** returns results over
//!   plain HTTP; the desktop host ships a JavaScript shell. Verticals
//!   switch with `ssc=tab.m_*.all` (plus `where=m_*`), not the legacy
//!   `where=image`-style values, which the new UI silently ignores.
//! * The tabs hand out **one batch per page load** and ignore their
//!   URL's paging parameters — page 1 and page 2 URLs return identical
//!   content. Deeper batches of the image / news / video / blog tabs
//!   are served by the "more" API whose URL each page itself issues;
//!   paging follows that chain (see the `more` module), up to page 100.
//! * The web-results view pages through its own pagination-bar
//!   hrefs (`page=N+1&start=(N-1)*20+1` for batch N); the batches
//!   are disjoint (audit W-22, measured 2026-11-08 — an earlier
//!   "same batch on every page" finding came from following the
//!   bar's batch-1 link and was wrong).
//!
//! # Library usage
//!
//! Add the provider crate to your `Cargo.toml` (the library is imported
//! as `naver_search`, after the `[lib]` target name):
//!
//! ```toml
//! [dependencies]
//! naver_search = { git = "https://github.com/mouseos/Naver_MCP", package = "naver-mcp-provider" }
//! tokio = { version = "1", features = ["full"] }
//! anyhow = "1"
//! ```
//!
//! All searches go through [`NaverSearchClient`]; every method returns
//! `anyhow::Result`, wrapping either a [`SearchResult`] of the vertical's
//! item type, an [`IntegratedResult`] (integrated search) or a
//! [`SuggestResult`] (autocomplete):
//!
//! ```no_run
//! # fn main() {
//! # let rt = tokio::runtime::Builder::new_current_thread()
//! #     .enable_all().build().unwrap();
//! # rt.block_on(async {
//! use naver_search::NaverSearchClient;
//!
//! let client = NaverSearchClient::new();
//! // Web-results view: query, page (1-based), limit.
//! let result = client.web_search("제주도 여행", 1, 5).await.unwrap();
//! assert!(result.ok);
//! for item in &result.items {
//!     println!("{} — {}", item.title, item.url);
//! }
//! // Autocomplete suggestions.
//! let suggest = client.suggest("네이").await.unwrap();
//! for s in &suggest.suggestions {
//!     println!("{}", s.keyword);
//! }
//! # });
//! # }
//! ```
//!
//! The result types, the URL builders and the offline page parsers are
//! also available without any network access:
//!
//! ```
//! use naver_search::{build_search_url, SearchResult, WebItem, PROVIDER};
//!
//! let item = WebItem {
//!     rank: 1,
//!     title: "Example".to_string(),
//!     url: "https://example.com/".to_string(),
//!     snippet: None,
//! };
//! let result = SearchResult::new("web", "쿼리", 1, vec![item]);
//! assert_eq!(result.provider, PROVIDER);
//! assert_eq!(result.count, 1);
//! assert!(build_search_url("테스트", "image")
//!     .unwrap()
//!     .contains("ssc=tab.m_image.all"));
//! ```

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

pub mod blog;
pub mod cafe;
pub mod common;
pub mod image;
pub mod kin;
pub mod more;
pub mod news;
pub mod search;
pub mod suggest;
pub mod urlbuild;
pub mod video;
pub mod web;

// ── Root re-exports: the stable library surface ──
//
// Library users should only need `use naver_search::{...}`. The
// per-vertical modules stay public, but every item type, the offline
// parsers and the URL builders are re-exported here so callers never
// have to reach into a module path.
pub use blog::{parse_blog_results, BlogItem};
pub use cafe::{parse_cafe_results, CafeItem};
pub use common::{clean_text, decode_entities, urlencode};
pub use image::{extract_image_items, map_image_items, parse_image_results, ImageItem};
pub use kin::{parse_kin_results, KinItem};
pub use more::{extract_more_url, parse_more_response, MorePayload, MAX_PAGE};
pub use news::{parse_news_results, NewsItem};
pub use search::{parse_integrated_results, IntegratedItem, IntegratedResult, IntegratedSection};
pub use suggest::{
    parse_suggest_json, parse_suggest_response, ParsedSuggest, SuggestResult, Suggestion,
};
pub use urlbuild::{build_page_url, build_search_url, suggest_url};
pub use video::{parse_video_results, VideoItem};
pub use web::{parse_web_results, WebItem};

/// Provider identifier used in every result envelope (`"naver"`).
pub const PROVIDER: &str = "naver";

/// A vertical-agnostic search response envelope.
///
/// `provider` / `vertical` are owned `String`s (not `&'static str`) so
/// that downstream users can deserialise results back — e.g. from
/// `serde_json::Value` or a cache file — which a borrowed field makes
/// impossible outside `'static` lifetimes. The JSON form is unchanged.
///
/// Naver's mobile pages do not report a total hit count, so unlike the
/// Yahoo provider there is deliberately no `total_hits` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult<T> {
    pub provider: String,
    pub vertical: String,
    pub ok: bool,
    pub query: String,
    pub page: i64,
    pub count: usize,
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl<T> SearchResult<T> {
    /// Build a successful envelope for `vertical` from already-fetched
    /// items. `count` is derived from `items.len()`.
    pub fn new(vertical: &str, query: &str, page: i64, items: Vec<T>) -> Self {
        Self {
            provider: PROVIDER.to_string(),
            vertical: vertical.to_string(),
            ok: true,
            query: query.trim().to_string(),
            page,
            count: items.len(),
            items,
            note: None,
        }
    }

    /// Attach a human-readable note about the result.
    pub fn with_note(mut self, note: &str) -> Self {
        self.note = Some(note.to_string());
        self
    }
}

/// Shared HTTP client: a mobile user agent, no cookie store (see
/// `NaverSearchClient::new` for why). Cheap to clone; one client per
/// process is the norm.
#[derive(Clone)]
pub struct NaverSearchClient {
    http: Client,
}

impl Default for NaverSearchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl NaverSearchClient {
    /// Create a client. No configuration, API key or login is required,
    /// and construction performs no network I/O.
    pub fn new() -> Self {
        let http = Client::builder()
            .user_agent(common::MOBILE_USER_AGENT)
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            // Deliberately NO cookie store. Naver search is stateless,
            // and measured behaviour showed that a request carrying the
            // cookies set by a previous response stalls until the
            // timeout (a shared client's second request timed out at
            // exactly 20 s; with cookies disabled it answers in ~2 s).
            .build()
            .expect("failed to build HTTP client");
        Self { http }
    }

    /// GET a Naver page as text, with the headers their edge expects.
    pub(crate) async fn get_html(&self, url: &str, referer: &str) -> Result<String> {
        let resp = self
            .http
            .get(url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", common::ACCEPT_LANGUAGE)
            .header("Referer", referer)
            .send()
            .await
            .map_err(common::map_reqwest_error)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(common::status_error(status));
        }
        resp.text().await.map_err(common::map_reqwest_error)
    }

    /// GET a JSON document (the suggestions endpoint).
    pub(crate) async fn get_json(&self, url: &str, referer: &str) -> Result<Value> {
        let resp = self
            .http
            .get(url)
            .header("Accept", "application/json,text/javascript,*/*;q=0.01")
            .header("Accept-Language", common::ACCEPT_LANGUAGE)
            .header("Referer", referer)
            .send()
            .await
            .map_err(common::map_reqwest_error)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(common::status_error(status));
        }
        resp.json().await.map_err(common::map_reqwest_error)
    }
}

/// Static description of the searchable verticals as a JSON value —
/// powers the `naver_list_verticals` MCP tool so an agent can pick the
/// right one, and doubles as machine-readable documentation for library
/// users. The cafe (카페) and Q&A (지식iN) tabs are implemented for
/// their first page (their continuations are not page-issued URLs),
/// and are listed with `paginated: false`.
pub fn verticals() -> Value {
    serde_json::json!({
        "provider": PROVIDER,
        "auth": "none",
        "config_required": false,
        "verticals": [
            {
                "id": "integrated",
                "name": "Naver 통합검색 (Integrated)",
                "tool": "naver_search",
                "description": "Integrated search: subject sections (news, images, blog, dictionary, ...) in page order.",
                "paginated": false
            },
            {
                "id": "web",
                "name": "Naver 웹문서 (Web results)",
                "tool": "naver_web_search",
                "description": "Web-results-only view. Pages follow the view's own pagination-bar hrefs (page=N+1, start=(N-1)*20+1); batches are disjoint (measured live).",
                "paginated": true
            },
            {
                "id": "image",
                "name": "Naver 이미지 (Images)",
                "tool": "naver_image_search",
                "description": "Image search with original image URL, thumbnail, source page and dimensions. Pages follow the page-issued more-API chain (up to page 100).",
                "paginated": true
            },
            {
                "id": "news",
                "name": "Naver 뉴스 (News)",
                "tool": "naver_news_search",
                "description": "News articles with publisher and relative publish time. Pages follow the page-issued more-API chain (up to page 100).",
                "paginated": true
            },
            {
                "id": "video",
                "name": "Naver 동영상 (Videos)",
                "tool": "naver_video_search",
                "description": "Video search across YouTube and other hosts, with channel, duration and view count. Pages follow the page-issued more-API chain (up to page 100).",
                "paginated": true
            },
            {
                "id": "blog",
                "name": "Naver 블로그 (Blog)",
                "tool": "naver_blog_search",
                "description": "Blog posts with blog name and relative publish time. Pages follow the page-issued more-API chain (up to page 100).",
                "paginated": true
            },
            {
                "id": "cafe",
                "name": "Naver 카페 (Cafe)",
                "tool": "naver_cafe_search",
                "description": "Cafe posts with cafe name, body excerpt and relative date. First page only: the tab's more-API URL is assembled by the page's JavaScript, not page-issued.",
                "paginated": false
            },
            {
                "id": "kin",
                "name": "Naver 지식iN (Q&A)",
                "tool": "naver_kin_search",
                "description": "지식iN questions with the answer excerpt. First page only: the list is an infinite-scroll list whose deeper batches are not page-issued as URLs.",
                "paginated": false
            },
            {
                "id": "suggest",
                "name": "Naver 검색어 자동완성 (Suggestions)",
                "tool": "naver_suggest",
                "description": "Autocomplete keyword suggestions for a partial query.",
                "paginated": false
            }
        ],
        "unsupported": [
            { "id": "ai", "name": "Naver AI탭 (AI search tab)", "reason": "The app builds the AI-tab URL (ssc=tab.m_ait.all) and it is buildable via naver_build_search_url, but the page served over plain HTTP is a client-rendered shell with no server-rendered answer (measured live 2026-11-08); the answer loads through a separate client-side API that the APK analysis did not identify." },
            { "id": "clip", "name": "Naver 클립 (Clip)", "reason": "Clip search uses a native API host (clip-api-ex.io.naver.com) whose API shape the APK analysis did not determine; no page-based route exists." }
        ],
        "builder_tool": "naver_build_search_url",
        "list_tool": "naver_list_verticals"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verticals_lists_nine_supported() {
        let v = verticals();
        let list = v["verticals"].as_array().unwrap();
        let ids: Vec<&str> = list.iter().map(|x| x["id"].as_str().unwrap()).collect();
        assert_eq!(
            ids,
            [
                "integrated",
                "web",
                "image",
                "news",
                "video",
                "blog",
                "cafe",
                "kin",
                "suggest"
            ]
        );
        assert_eq!(v["unsupported"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn envelope_shape_matches_the_unified_form() {
        let result: SearchResult<WebItem> = SearchResult::new("web", "q", 1, vec![]);
        let text = serde_json::to_string(&result).unwrap();
        assert!(text.contains("\"provider\":\"naver\""));
        assert!(text.contains("\"ok\":true"));
        assert!(text.contains("\"count\":0"));
        assert!(!text.contains("total_hits"));
        assert!(!text.contains("note"));
    }
}
