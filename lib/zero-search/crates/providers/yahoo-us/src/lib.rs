//! Yahoo (US) search client — usable both as the search engine of the
//! Yahoo_US_MCP server and as a standalone Rust library.
//!
//! The verticals are read from the same public search pages the Yahoo
//! Scout app opens in its WebView (`search.yahoo.com` and friends). The US
//! SERP ships no JSON payload, so results are parsed from the
//! server-rendered HTML, and result links are unwrapped from Yahoo's
//! `r.search.yahoo.com` redirect URLs into real destination URLs. Nothing
//! here needs an API key, a login, cookies or any user configuration.
//!
//! Image search is the one exception to the shared HTTP client: the
//! image host refuses plain HTTP clients (HTTP 500, empty body) while
//! answering real browsers on the same connection, so that vertical's
//! pages are fetched by a companion helper process running a
//! browser-grade client (wreq, Firefox profile) — the same
//! server-rendered page, no browser driven. See [`image`].
//!
//! # Library usage
//!
//! Add the provider crate to your `Cargo.toml` (the library is imported as
//! `yahoo_us_search`, after the `[lib]` target name):
//!
//! ```toml
//! [dependencies]
//! yahoo_us_search = { git = "https://github.com/mouseos/Yahoo_US_MCP", package = "yahoo-us-mcp-provider" }
//! tokio = { version = "1", features = ["full"] }
//! anyhow = "1"
//! ```
//!
//! All searches go through [`YahooUsSearchClient`]; every method returns
//! `anyhow::Result`, wrapping either a [`SearchResult`] of the vertical's
//! item type or, for autocomplete, a [`SuggestResult`]:
//!
//! ```no_run
//! # fn main() {
//! # let rt = tokio::runtime::Builder::new_current_thread()
//! #     .enable_all().build().unwrap();
//! # rt.block_on(async {
//! use yahoo_us_search::YahooUsSearchClient;
//!
//! let client = YahooUsSearchClient::new();
//! // Web search: query, page (1-based), limit.
//! let result = client.web_search("apple pie recipe", 1, 5).await.unwrap();
//! assert!(result.ok);
//! for item in &result.items {
//!     println!("{} — {}", item.title, item.url);
//! }
//! // Search suggestions.
//! let suggest = client.suggest("app", 10).await.unwrap();
//! for s in &suggest.suggestions {
//!     println!("{}", s.keyword);
//! }
//! # });
//! # }
//! ```
//!
//! The parsers also work offline on saved pages, which is how the test
//! suite verifies them against recorded fixtures:
//!
//! ```
//! use yahoo_us_search::{build_search_url, parse_web_results, PROVIDER};
//!
//! assert_eq!(PROVIDER, "yahoo_us");
//! assert_eq!(
//!     build_search_url("apple", "web", 2).unwrap(),
//!     "https://search.yahoo.com/search?p=apple&b=8&pz=7"
//! );
//! let html = r#"<div id="web"></div>"#;
//! assert!(parse_web_results(html, 7).is_empty());
//! ```

use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod common;
pub mod helper;
pub mod image;
pub mod news;
pub mod suggest;
pub mod urlbuild;
pub mod video;
pub mod web;

// ── Root re-exports: the stable library surface ──
//
// Library users should only need `use yahoo_us_search::{...}`. The
// per-vertical modules stay public, but every item type, the suggest
// types, the offline parsers and the text/URL helpers are re-exported
// here so callers never have to reach into a module path.

/// Text and URL helpers shared by all verticals.
pub use common::{
    clean_text, decode_entities, decode_redirect_url, parse_related_queries, urldecode, urlencode,
};
/// Image item returned by [`YahooUsSearchClient::image_search`].
pub use image::ImageItem;
/// Offline image parsers for saved SERP pages.
pub use image::{parse_image_next_offset, parse_image_results, parse_image_total};
/// News article item returned by [`YahooUsSearchClient::news_search`].
pub use news::NewsItem;
/// Offline news parsers for saved SERP pages.
pub use news::{news_has_next_page, parse_news_results};
/// Offline suggest-response parser for a decoded gossip JSON body.
pub use suggest::parse_suggest_response;
/// Autocomplete types returned by [`YahooUsSearchClient::suggest`].
pub use suggest::{SuggestResult, Suggestion};
/// Search URL builder (no network).
pub use urlbuild::{
    build_image_search_url, build_search_url, build_search_url_with_vm, ImageFilters,
};
/// Video item returned by [`YahooUsSearchClient::video_search`].
pub use video::VideoItem;
/// Offline video parsers for saved SERP pages.
pub use video::{parse_video_next_offset, parse_video_results, parse_video_total};
/// Web item returned by [`YahooUsSearchClient::web_search`].
pub use web::WebItem;
/// Offline web parsers for saved SERP pages.
pub use web::{parse_web_results, web_has_next_page};

/// Provider identifier used in every result envelope (`"yahoo_us"`).
pub const PROVIDER: &str = "yahoo_us";

/// A vertical-agnostic search response envelope.
///
/// `provider` / `vertical` are owned `String`s (not `&'static str`) so that
/// downstream users can deserialise results back — e.g. from
/// `serde_json::Value` or a cache file — which a borrowed field makes
/// impossible outside `'static` lifetimes. The JSON form is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult<T> {
    pub provider: String,
    pub vertical: String,
    pub ok: bool,
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
    /// Build a successful envelope for `vertical` from already-fetched items.
    /// `count` is derived from `items.len()`; chain the `with_*` builders to
    /// attach totals and paging.
    pub fn new(vertical: &str, query: &str, page: i64, items: Vec<T>) -> Self {
        Self {
            provider: PROVIDER.to_string(),
            vertical: vertical.to_string(),
            ok: true,
            query: query.to_string(),
            page,
            count: items.len(),
            total_hits: None,
            next_page: None,
            related_queries: Vec::new(),
            items,
            note: None,
        }
    }

    /// Attach the total hit count reported by Yahoo, if known.
    pub fn with_total(mut self, total: Option<i64>) -> Self {
        self.total_hits = total;
        self
    }

    /// Attach the next 1-based page number, if a further page exists.
    pub fn with_next_page(mut self, next: Option<i64>) -> Self {
        self.next_page = next;
        self
    }

    /// Attach related queries suggested alongside the results.
    pub fn with_related(mut self, related: Vec<String>) -> Self {
        self.related_queries = related;
        self
    }

    /// Attach a human-readable note about the result (e.g. a paging caveat).
    pub fn with_note(mut self, note: &str) -> Self {
        self.note = Some(note.to_string());
        self
    }
}

/// Shared HTTP client. Cheap to clone; holds a connection pool. No cookie
/// jar is needed — the US SERPs answer anonymous requests as-is (verified
/// during the APK analysis).
///
/// Image search is served through a helper process (see
/// [`helper`]): Yahoo's image host refuses plain HTTP clients with
/// HTTP 500 while answering real browsers, so a browser-grade client
/// reproduces the browser's TLS/HTTP2 request fingerprint — in a
/// separate binary, because its TLS stack cannot link into this one.
#[derive(Clone)]
pub struct YahooUsSearchClient {
    http: Client,
}

impl Default for YahooUsSearchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl YahooUsSearchClient {
    /// Create a client with the shared connection pool.
    /// No configuration, API key or login is required. Cheap to clone and
    /// safe to share across tasks; one client per process is the norm.
    pub fn new() -> Self {
        let http = Client::builder()
            .user_agent(common::USER_AGENT)
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self { http }
    }

    /// GET a Yahoo page as text, with the headers their edge expects.
    ///
    /// When the direct fetch fails, the request is retried once
    /// through the helper process (see [`helper`]) if the helper is
    /// installed: Yahoo's edge answers `search.yahoo.com` with a
    /// bare HTTP 500 to plain HTTP clients while serving real
    /// browsers (Z12 diagnosis, measured from two egresses), which
    /// is the same refusal the image host has always shown. Where
    /// the direct fetch works it is used untouched, and where no
    /// helper is installed the original error surfaces unchanged.
    pub(crate) async fn get_html(&self, url: &str, referer: &str) -> Result<String> {
        match self.get_html_direct(url, referer).await {
            Ok(html) => Ok(html),
            Err(direct_err) => {
                if helper::helper_path().is_some() {
                    if let Ok(html) = helper::fetch_page_html(url).await {
                        return Ok(html);
                    }
                }
                Err(direct_err)
            }
        }
    }

    async fn get_html_direct(&self, url: &str, referer: &str) -> Result<String> {
        let resp = self
            .http
            .get(url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", common::ACCEPT_LANGUAGE)
            .header("Referer", referer)
            .header("Upgrade-Insecure-Requests", "1")
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.text().await?)
    }

    /// GET an image-search page as text through the helper process
    /// (see [`helper`]): the helper's request carries a real Firefox
    /// TLS/HTTP2 fingerprint (wreq emulation), which is what Yahoo's
    /// image host demands — it returns HTTP 500 to plain HTTP
    /// clients. Like the reqwest client above, proxying follows the
    /// standard environment variables; no configuration is read.
    pub(crate) async fn get_image_html(&self, url: &str) -> Result<String> {
        helper::fetch_image_html(url).await
    }

    pub(crate) async fn get_json(&self, url: &str, referer: &str) -> Result<serde_json::Value> {
        match self.get_json_direct(url, referer).await {
            Ok(value) => Ok(value),
            Err(direct_err) => {
                // Same helper fallback as get_html: the JSON
                // endpoints sit behind the same edge.
                if helper::helper_path().is_some() {
                    if let Ok(body) = helper::fetch_page_html(url).await {
                        if let Ok(value) = serde_json::from_str(&body) {
                            return Ok(value);
                        }
                    }
                }
                Err(direct_err)
            }
        }
    }

    async fn get_json_direct(&self, url: &str, referer: &str) -> Result<serde_json::Value> {
        let resp = self
            .http
            .get(url)
            .header("Accept", "application/json,text/javascript,*/*;q=0.01")
            .header("Accept-Language", common::ACCEPT_LANGUAGE)
            .header("Referer", referer)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }
}

/// Static description of the searchable verticals (web, image, video,
/// news, suggest) as a JSON value — powers the
/// `yahoo_us_list_verticals` MCP tool so an agent can pick the right one,
/// and doubles as machine-readable documentation for library users.
pub fn verticals() -> serde_json::Value {
    serde_json::json!({
        "provider": PROVIDER,
        "auth": "none",
        "config_required": false,
        "verticals": [
            {
                "id": "web",
                "name": "Yahoo Search (Web)",
                "tool": "yahoo_us_web_search",
                "description": "General web search. 7 results per page; result URLs are unwrapped to their real destinations.",
                "paginated": true,
                "supported": true
            },
            {
                "id": "video",
                "name": "Yahoo Video Search",
                "tool": "yahoo_us_video_search",
                "description": "Video search across YouTube and other hosts, with duration, source and thumbnails. 60 results per page.",
                "paginated": true,
                "supported": true
            },
            {
                "id": "news",
                "name": "Yahoo News Search",
                "tool": "yahoo_us_news_search",
                "description": "News article search with publisher and relative publish time. 10 results per page.",
                "paginated": true,
                "supported": true
            },
            {
                "id": "suggest",
                "name": "Yahoo Search Suggestions",
                "tool": "yahoo_us_suggest",
                "description": "Autocomplete suggestions for a partial query (gossip API).",
                "paginated": false,
                "supported": true
            },
            {
                "id": "image",
                "name": "Yahoo Image Search",
                "tool": "yahoo_us_image_search",
                "description": "Image search with the original image URL, source page, thumbnail and dimensions. 60 results per page. Pages are fetched by the bundled helper process with a browser-grade (Firefox-profile) client because Yahoo's image host returns HTTP 500 to plain HTTP clients.",
                "paginated": true,
                "supported": true
            }
        ],
        "unsupported": [
            { "id": "shopping", "name": "Yahoo Shopping Search", "reason": "Retired by Yahoo (measured live 2026-11-08): shopping.search.yahoo.com/search 301-redirects to shopping.yahoo.com/search, which in turn redirects to scout.yahoo.com/chat — the Yahoo Scout AI chat. No anonymous shopping SERP exists anymore." },
            { "id": "dictionary", "name": "Yahoo Dictionary Search", "reason": "Retired by Yahoo (measured live 2026-11-08): dictionary.search.yahoo.com/search 301-redirects to the plain web search (search.yahoo.com/search); there is no separate dictionary vertical." }
        ]
    })
}
