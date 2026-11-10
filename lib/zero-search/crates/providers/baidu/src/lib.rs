//! Baidu search provider — Rust port of the Python
//! `baidu_search` client + `baidu_parser` parser packages.
//!
//! This crate is usable in two ways:
//!
//! * as the provider behind the `baidu-mcp` MCP server binary, and
//! * as a plain Rust library (this page documents the library API).
//!
//! The package name is `baidu-mcp-provider`; the library is
//! imported as `baidu_search` (the `[lib]` target name), following
//! the naming pattern shared with the Yahoo! JAPAN provider
//! (`yahoo_search`). Before the 2026-11 unification the package
//! was named `baidu-mcp-baidu` and the library `baidu_mcp_baidu`;
//! update the package name in your `Cargo.toml` and
//! `use baidu_search::…` in your code when upgrading.
//!
//! # Stable public API
//!
//! External users should import from the crate root only. The
//! following items are the supported, stable surface:
//!
//! * [`BaiduClient`] — end-to-end search client (generic over a
//!   [`Fetcher`]); [`BaiduClient::new_live`] builds one backed by
//!   [`ReqwestFetcher`].
//! * [`Fetcher`] / [`ReqwestFetcher`] / [`TransportResponse`] —
//!   transport abstraction and its reqwest implementation. Implement
//!   [`Fetcher`] yourself to substitute a fake or cached transport
//!   in tests.
//! * [`parse_search_results`] / [`parse_search_results_bytes`] —
//!   offline parsing of a saved Baidu (APK / wise) search-result
//!   HTML page. No network access.
//! * [`build_initial_search_url`] — build the first-page search URL
//!   for a query (APK `bq0.m` reproduction). No network access.
//! * Models: [`SearchPage`], [`AggregatedSearch`],
//!   [`SinglePageResult`], [`PageRecord`], [`ResultEntry`],
//!   [`CardEntry`], [`PageInfo`], [`RelatedSearch`],
//!   [`UnknownElement`], [`SiteLink`], [`CardItem`].
//! * [`BaiduError`] — error type; its `kind` field is a stable
//!   string (`access_restricted`, `timeout`, …) matching the Python
//!   client.
//!
//! # Internal modules
//!
//! The public modules [`antct`], [`classifiers`], [`dom`],
//! [`extractors`], [`pagination`], [`preprocess`] and [`urlbuild`]
//! are **internal implementation details**. They remain `pub`
//! only so the MCP server crate and the ported test suite in this
//! workspace can reach them without a breaking change; they are
//! not part of the stable library API, may change without notice,
//! and some of them expose `scraper` DOM types (see
//! [`dom::NRef`]). Do not depend on them from external code.
//!
//! The modules [`client`], [`errors`], [`models`], [`parser`],
//! [`search_url`] and [`urls`] are also public, and the items they
//! contain are re-exported at the crate root where they form the
//! stable surface above. Prefer the crate-root imports.
//!
//! # Examples
//!
//! Parse a saved HTML page offline (no network):
//!
//! ```rust
//! use baidu_search::parse_search_results;
//!
//! let html = r#"<html><body><div id="results"></div></body></html>"#;
//! let page = parse_search_results(html, Some("weather"), None);
//! assert!(page.results.is_empty());
//! assert_eq!(page.query.as_deref(), Some("weather"));
//! // Structured JSON identical to the Python parser's `to_dict()`:
//! let _json = page.to_value();
//! ```
//!
//! Build a first-page search URL offline (no network). Pass
//! `Some(now_ms)` to pin the clock for reproducible output; pass
//! `None` in production to use the current time:
//!
//! ```rust
//! use baidu_search::build_initial_search_url;
//!
//! let url = build_initial_search_url("weather", Some(1_700_000_000_000));
//! assert!(url.starts_with("https://m.baidu.com/s?"));
//! assert!(url.contains("word=weather"));
//! assert!(url.contains("ant_ct="));
//! ```
//!
//! Live search (network; honours a ≥ 1 s request interval, performs
//! no retries, and stops on HTTP 403 / 429):
//!
//! ```no_run
//! use baidu_search::BaiduClient;
//!
//! # async fn run() -> Result<(), baidu_search::BaiduError> {
//! let client = BaiduClient::new_live();
//! let aggregated = client.search("weather", 1).await?;
//! println!("fetched {} page(s), {} result(s)",
//!          aggregated.pages_fetched, aggregated.results.len());
//! # Ok(())
//! # }
//! ```
//!
//! Runnable versions of the offline examples live in this crate's
//! `examples/` directory (`parse_saved_html`, `build_search_url`).
//!
//! Module layout mirrors the Python packages:
//! `antct`, `urlbuild`, `search_url`, `errors`, `models`, `urls`,
//! `classifiers`, `extractors`, `pagination`, `parser`, `client`
//! (`dom` is the internal scraper-DOM helper layer).

pub mod antct;
pub mod classifiers;
pub mod client;
pub mod dom;
pub mod errors;
pub mod extractors;
pub mod models;
pub mod pagination;
pub mod parser;
pub mod preprocess;
pub mod search_url;
pub mod suggest;
pub mod urlbuild;
pub mod urls;

pub use client::{BaiduClient, Fetcher, ReqwestFetcher, TransportResponse};
pub use errors::BaiduError;
pub use models::{
    AggregatedSearch, CardEntry, CardItem, PageInfo, PageRecord, RelatedSearch, ResultEntry,
    SearchPage, SinglePageResult, SiteLink, UnknownElement,
};
pub use parser::{parse_search_results, parse_search_results_bytes};
pub use search_url::build_initial_search_url;
pub use suggest::{
    build_sug_url, build_sugrec_rec_payload, build_sugrec_rs_payload, parse_inputbox_response,
    parse_sug_response, parse_sugrec_response, InputBoxSug, InputBoxSugInfo, SugEntry, SugRecEntry,
    SuggestClient,
};
pub use urlbuild::{build_search_url_for_page, build_vertical_search_url};
