//! External-user integration test.
//!
//! This file deliberately imports **only** from the crate root
//! (`baidu_search::{…}`), exactly as an external user of the
//! library would. It must not reach into internal modules
//! (`dom`, `classifiers`, `extractors`, `preprocess`, …).
//! If a stable item cannot be reached from the root, this test —
//! not the internal test suite — is the one that should fail.

use baidu_search::{
    build_initial_search_url, parse_search_results, parse_search_results_bytes, AggregatedSearch,
    BaiduClient, BaiduError, Fetcher, ReqwestFetcher, SearchPage, SinglePageResult,
    TransportResponse,
};

// ---------------------------------------------------------------- parse

#[test]
fn root_parse_str_and_bytes_agree() {
    let html = include_bytes!("fixtures/synthetic_ad.html");
    let text = std::str::from_utf8(html).unwrap();
    let url = "https://m.baidu.com/s?word=test";

    let as_str: SearchPage = parse_search_results(text, Some("test"), Some(url));
    let as_bytes: SearchPage = parse_search_results_bytes(html, Some("test"), Some(url));

    assert_eq!(as_str.to_value(), as_bytes.to_value());
    assert_eq!(as_str.results.len(), 2);
    assert_eq!(as_str.ads.len(), 1);
    assert_eq!(as_str.results[0].title.as_deref(), Some("Organic & Title"));
    assert_eq!(as_str.query.as_deref(), Some("test"));
}

#[test]
fn root_parse_empty_page_does_not_panic() {
    let page = parse_search_results("<html><body></body></html>", Some("x"), None);
    assert!(page.results.is_empty());
    assert!(page.ads.is_empty());
    assert!(page.cards.is_empty());
}

// ---------------------------------------------------------------- URL generation

#[test]
fn root_build_initial_search_url_is_deterministic_with_pinned_clock() {
    let a = build_initial_search_url("天气", Some(1_700_000_000_000));
    let b = build_initial_search_url("天气", Some(1_700_000_000_000));
    assert_eq!(a, b, "same query + same now_ms must give the same URL");
    assert!(a.starts_with("https://m.baidu.com/s?tn=zbios&"), "{a}");
    assert!(a.contains("word=%E5%A4%A9%E6%B0%94"), "{a}");
    assert!(a.contains("ant_ct="), "{a}");
    assert!(a.contains("t_samp="), "{a}");
    // Device-derived parameters are never added.
    for forbidden in ["cuid=", "cua=", "pkgname="] {
        assert!(!a.contains(forbidden), "forbidden {forbidden} in {a}");
    }
}

#[test]
fn root_build_initial_search_url_without_clock_still_well_formed() {
    let url = build_initial_search_url("weather", None);
    assert!(url.starts_with("https://m.baidu.com/s?"), "{url}");
    assert!(url.contains("word=weather"), "{url}");
}

// ---------------------------------------------------------------- client construction

#[test]
fn root_client_constructors() {
    // Live client (construction only — this test performs no network I/O).
    let live = BaiduClient::new_live();
    let _: &ReqwestFetcher = live.fetcher();

    let custom = BaiduClient::new(ReqwestFetcher::default()).with_max_pages_limit(3);
    assert_eq!(custom.max_pages_limit, 3);

    let tuned = BaiduClient::new_live_with(10.0, 5, 0.0, 1_000_000);
    assert_eq!(tuned.max_pages_limit, 5);
}

// A user-defined Fetcher, written exactly as external code would.
struct StaticFetcher {
    body: Vec<u8>,
}

impl Fetcher for StaticFetcher {
    async fn fetch(&self, url: &str) -> Result<TransportResponse, BaiduError> {
        Ok(TransportResponse::new(
            200,
            url.to_string(),
            self.body.clone(),
        ))
    }
}

#[tokio::test]
async fn root_client_with_custom_fetcher_parses_offline() {
    let body = include_bytes!("fixtures/synthetic_ad.html").to_vec();
    let client = BaiduClient::new(StaticFetcher { body });
    let single: SinglePageResult = client.search_page("test", None).await.unwrap();
    assert_eq!(single.page.results.len(), 2);
    assert_eq!(single.page.ads.len(), 1);
    assert_eq!(single.status_code, Some(200));

    // Aggregated search over the same static transport (one page,
    // because the fixture has no next-page URL).
    let agg: AggregatedSearch = client.search("test", 1).await.unwrap();
    assert_eq!(agg.pages_fetched, 1);
    assert_eq!(agg.results.len(), 2);
    let json = agg.to_value();
    assert_eq!(json["query"], "test");
    assert_eq!(json["pages_fetched"], 1);
}

#[test]
fn root_baidu_error_shape() {
    let err = BaiduError::new("access_restricted", "access restricted (HTTP 403)")
        .with_url("https://m.baidu.com/s".to_string())
        .with_status(403);
    assert_eq!(err.kind, "access_restricted");
    assert_eq!(err.status_code, Some(403));
    let value = err.to_value();
    assert_eq!(value["kind"], "access_restricted");
    assert_eq!(value["status_code"], 403);
    // BaiduError implements std::error::Error.
    let _: &dyn std::error::Error = &err;
}
