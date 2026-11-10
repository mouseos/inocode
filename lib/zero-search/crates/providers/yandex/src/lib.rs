//! Yandex search client — usable both as the search engine of the
//! Yandex_MCP server and as a standalone Rust library.
//!
//! Everything is read from the touch (mobile web) interface the official
//! Yandex app itself uses. Requests reproduce the app's request shape — its
//! user agent, `Accept-Language: ru-RU`, the `X-Requested-With:
//! ru.yandex.searchplugin` and `X-Yandex-Flags` headers, and the
//! `ui=webmobileapp.yandex` page parameter (audit W-39) — and, critically,
//! its *session*: Yandex
//! only serves page 2 and deeper, and the other verticals, to clients that
//! first loaded a search page and kept the cookies the server issued. The
//! client therefore keeps a cookie store and establishes the session from
//! the web touch page before anything else, using only server-issued
//! cookies. Nothing here needs an API key, a login or any user
//! configuration. (The app's `clid` is a device-side identifier and is
//! deliberately not sent: fabricating one would misstate the client.)
//!
//! If Yandex answers with a captcha challenge (`showcaptcha`) or HTTP
//! 403/429, the call fails with an `access_restricted` error. The client
//! never retries around it and never tries to solve or evade the challenge.
//!
//! Yandex News is not exposed: the news vertical moved to dzen.ru and now
//! requires Yandex SSO sign-in, so it cannot be read anonymously.
//!
//! # Library usage
//!
//! Add the provider crate to your `Cargo.toml` (the library is imported as
//! `yandex_search`, after the `[lib]` target name):
//!
//! ```toml
//! [dependencies]
//! yandex_search = { git = "https://github.com/mouseos/Yandex_MCP", package = "yandex-mcp-provider" }
//! tokio = { version = "1", features = ["full"] }
//! anyhow = "1"
//! ```
//!
//! All searches go through [`YandexSearchClient`]; every method returns
//! `anyhow::Result`, wrapping either a [`SearchResult`] of the vertical's
//! item type or, for autocomplete, a [`SuggestResult`]:
//!
//! ```no_run
//! # fn main() {
//! # let rt = tokio::runtime::Builder::new_current_thread()
//! #     .enable_all().build().unwrap();
//! # rt.block_on(async {
//! use yandex_search::YandexSearchClient;
//!
//! let client = YandexSearchClient::new();
//! // Web search: query, page (1-based), limit.
//! let result = client.web_search("Москва погода", 1, 5).await.unwrap();
//! assert!(result.ok);
//! for item in &result.items {
//!     println!("{} — {}", item.title, item.url);
//! }
//! // Autocomplete suggestions.
//! let suggest = client.suggest("моск", 10).await.unwrap();
//! for s in &suggest.suggestions {
//!     println!("{}", s.keyword);
//! }
//! # });
//! # }
//! ```
//!
//! The result types and the offline helpers are also available without any
//! network access — handy for building values, serialising results, parsing
//! a saved search page and constructing search URLs:
//!
//! ```
//! use yandex_search::{SearchResult, WebItem, PROVIDER};
//!
//! let item = WebItem {
//!     rank: 1,
//!     title: "Example".to_string(),
//!     url: "https://example.com/".to_string(),
//!     description: None,
//!     site: None,
//! };
//! let result = SearchResult::new("web", "запрос", 1, vec![item]);
//! assert_eq!(result.provider, PROVIDER);
//! assert_eq!(result.count, 1);
//! assert_eq!(
//!     yandex_search::build_search_url("тест", "web").unwrap(),
//!     "https://yandex.ru/search/touch/?text=%D1%82%D0%B5%D1%81%D1%82&ui=webmobileapp.yandex"
//! );
//! ```

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod common;
pub mod image;
pub mod suggest;
pub mod urlbuild;
pub mod video;
pub mod web;

use common::{ACCEPT_LANGUAGE, REQUESTED_WITH, USER_AGENT, YANDEX_FLAGS};

// ── Root re-exports: the stable library surface ──
//
// Library users should only need `use yandex_search::{...}`. The per-vertical
// modules stay public, but every item type, the suggest types, the parsers
// and the offline helpers are re-exported here so callers never have to
// reach into a module path.

/// Offline helpers: text cleanup, embedded-state extraction, URL building.
pub use common::{
    absolutize, clean_text, decode_entities, extract_data_state, extract_json_after, urlencode,
};
/// Image page parser for saved pages (offline).
pub use image::parse_image_results;
/// Image item returned by [`YandexSearchClient::image_search`].
pub use image::ImageItem;
/// Suggest response parser for saved responses (offline).
pub use suggest::parse_suggest_body;
/// Suggest-endings response parser for saved responses (offline).
pub use suggest::parse_suggest_endings_body;
/// Autocomplete types returned by [`YandexSearchClient::suggest`].
pub use suggest::{SuggestResult, Suggestion};
/// Offline search-URL construction (the `yandex_build_search_url` tool).
pub use urlbuild::build_search_url;
/// Offline suggest-endings URL construction.
pub use urlbuild::build_suggest_endings_url;
/// Video page parser for saved pages (offline).
pub use video::parse_video_results;
/// Video item returned by [`YandexSearchClient::video_search`].
pub use video::VideoItem;
/// Web item returned by [`YandexSearchClient::web_search`].
pub use web::WebItem;
/// Web page parser for saved pages (offline), and the approximate-total
/// display string Yandex prints on the page.
pub use web::{
    approx_total_text, parse_special_cards, parse_web_results, SpecialCard, WebSearchPage,
};

/// Provider identifier used in every result envelope (`"yandex"`).
pub const PROVIDER: &str = "yandex";

/// Deepest page verified live within one cookie session (audit
/// W-38). Measured 2026-11-08: pages 1 and 2 returned results in a
/// single session; page 3 was answered with a captcha challenge
/// (`access_restricted`). The analysis rounds had likewise only
/// ever verified pages 1–2. Deeper pages are still attempted — the
/// outcome may differ by query and time — but results beyond this
/// page carry a note marking them as outside the verified range.
pub const VERIFIED_MAX_PAGE: i64 = 2;

/// Note attached to results beyond [`VERIFIED_MAX_PAGE`].
pub(crate) const BEYOND_VERIFIED_NOTE: &str =
    "Beyond the live-verified session range (pages 1-2; page 3 drew a captcha challenge when measured 2026-11-08).";

/// Prefix of the error message returned when Yandex restricts access
/// (captcha challenge or HTTP 403/429). Callers branch on this prefix; the
/// MCP server maps it to the `access_restricted` error kind.
pub const ACCESS_RESTRICTED_PREFIX: &str = "access_restricted";

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
    /// attach totals, paging and notes.
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

    /// Attach the total hit count, if the provider reports an exact one.
    /// (Yandex prints only an approximate figure, so web results leave this
    /// unset and carry the printed estimate in `note` instead.)
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

/// Shared HTTP client: one connection pool, one cookie jar, one session.
///
/// Cheap to clone. The session flag records whether the web touch page has
/// been loaded (and Yandex's cookies stored) in this process; every method
/// that needs a session establishes it first via [`YandexSearchClient`]'s
/// internal `ensure_session`.
#[derive(Clone)]
pub struct YandexSearchClient {
    http: Client,
    session_started: std::sync::Arc<tokio::sync::Mutex<bool>>,
}

impl Default for YandexSearchClient {
    fn default() -> Self {
        Self::new()
    }
}

impl YandexSearchClient {
    /// Create a client with the app-shaped request identity and a cookie
    /// store. No configuration, API key or login is required. Cheap to clone
    /// and safe to share across tasks; one client per process is the norm.
    pub fn new() -> Self {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .cookie_store(true)
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build HTTP client");

        Self {
            http,
            session_started: std::sync::Arc::new(tokio::sync::Mutex::new(false)),
        }
    }

    /// GET a Yandex page as text, with the app's headers.
    ///
    /// A captcha challenge (the `showcaptcha` page, reached directly or by
    /// redirect) and HTTP 403/429 are reported as `access_restricted`
    /// errors; nothing is retried or evaded.
    pub(crate) async fn get_page(&self, url: &str) -> Result<String> {
        let resp = self
            .http
            .get(url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", ACCEPT_LANGUAGE)
            .header("X-Requested-With", REQUESTED_WITH)
            .header("X-Yandex-Flags", YANDEX_FLAGS)
            .send()
            .await?;
        let status = resp.status();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            return Err(access_restricted(&format!("HTTP {status}")));
        }
        let resp = resp.error_for_status()?;
        let final_url = resp.url().to_string();
        let body = resp.text().await?;
        if final_url.contains("showcaptcha") || body.contains("showcaptcha") {
            return Err(access_restricted("captcha challenge (showcaptcha)"));
        }
        Ok(body)
    }

    /// Establish the cookie session by loading web touch page 1 for `query`
    /// — the same first request the app makes. No-op once established.
    pub(crate) async fn ensure_session(&self, query: &str) -> Result<()> {
        if *self.session_started.lock().await {
            return Ok(());
        }
        let url = web::web_search_url(query, 1);
        let _ = self.get_page(&url).await?;
        self.mark_session_started().await;
        Ok(())
    }

    /// Record that a page-1 load already established the session.
    pub(crate) async fn mark_session_started(&self) {
        *self.session_started.lock().await = true;
    }
}

/// The error returned for captcha / HTTP 403 / 429 responses.
pub(crate) fn access_restricted(detail: &str) -> anyhow::Error {
    anyhow!(
        "{ACCESS_RESTRICTED_PREFIX}: Yandex restricted access ({detail}); \
         the request was not retried and no challenge was bypassed"
    )
}

/// Static description of the searchable verticals (web, image, video,
/// suggest — plus news, marked unsupported) as a JSON value — powers the
/// `yandex_list_verticals` MCP tool so an agent can pick the right one, and
/// doubles as machine-readable documentation for library users.
pub fn verticals() -> serde_json::Value {
    serde_json::json!({
        "provider": PROVIDER,
        "auth": "none",
        "config_required": false,
        "verticals": [
            {
                "id": "web",
                "name": "Yandex Web search",
                "tool": "yandex_web_search",
                "description": "General web search over the touch SERP: title, destination URL, displayed path and snippet. ~10 organic results per page.",
                "paginated": true,
                "supported": true
            },
            {
                "id": "image",
                "name": "Yandex Images",
                "tool": "yandex_image_search",
                "description": "Image search with original URL, thumbnail, pixel dimensions and source page. 20 results per page.",
                "paginated": true,
                "supported": true
            },
            {
                "id": "video",
                "name": "Yandex Video",
                "tool": "yandex_video_search",
                "description": "Video search across VK, YouTube, RuTube, OK and other hosts, with watch URL, duration and provider.",
                "paginated": true,
                "supported": true
            },
            {
                "id": "suggest",
                "name": "Yandex Suggest",
                "tool": "yandex_suggest",
                "description": "Autocomplete suggestions for a partial query.",
                "paginated": false,
                "supported": true
            },
            {
                "id": "news",
                "name": "Yandex News",
                "tool": null,
                "description": "Not available anonymously.",
                "paginated": false,
                "supported": false,
                "reason": "Yandex News moved to dzen.ru (news search redirects to dzen.ru/news/search and then to Yandex SSO sign-in), so it cannot be read without an account."
            }
        ]
    })
}
