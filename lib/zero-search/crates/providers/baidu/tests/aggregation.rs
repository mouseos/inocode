//! Port of Python `test_aggregation.py`: duplicates are detected by
//! exact URL match, flagged, and never dropped; parser rank fields
//! are preserved.

use baidu_search::models::{aggregate, SinglePageResult};
use baidu_search::parse_search_results_bytes;

fn fixture(name: &str) -> &'static [u8] {
    match name {
        "tianqi_p1.html" => include_bytes!("fixtures/tianqi_p1.html"),
        "tianqi_pn10.html" => include_bytes!("fixtures/tianqi_pn10.html"),
        "tianqi_pn20.html" => include_bytes!("fixtures/tianqi_pn20.html"),
        "weather_en.html" => include_bytes!("fixtures/weather_en.html"),
        _ => panic!("unknown fixture {}", name),
    }
}

fn single(name: &str, query: &str, page_url: &str) -> SinglePageResult {
    let page = parse_search_results_bytes(fixture(name), Some(query), Some(page_url));
    SinglePageResult {
        query: Some(query.to_string()),
        source_url: page_url.to_string(),
        final_url: Some(page_url.to_string()),
        status_code: Some(200),
        page,
        warnings: vec![],
    }
}

#[test]
fn duplicates_flagged_and_originals_kept() {
    let url = "https://m.baidu.com/s?word=%E5%A4%A9%E6%B0%94";
    let s1 = single("tianqi_p1.html", "天气", url);
    let s2 = single("tianqi_p1.html", "天气", url);
    let agg = aggregate("天气", 2, &[s1, s2], &[], &[], &[]);
    assert_eq!(agg.results.len(), 10);
    let dupes: Vec<_> = agg
        .results
        .iter()
        .filter(|r| r["duplicate"] == serde_json::json!(true))
        .collect();
    assert_eq!(dupes.len(), 5);
    let data = agg.to_value();
    assert_eq!(data["diagnostics"]["duplicates"], 5);
    let items = data["diagnostics"]["duplicate_items"].as_array().unwrap();
    assert_eq!(items[0]["first_page_number"], 1);
    assert_eq!(items[0]["duplicate_page_number"], 1);
    assert_eq!(items[0]["key"], agg.results[0]["url"]);
}

#[test]
fn no_false_duplicates_across_real_pages() {
    let s1 = single("tianqi_p1.html", "天气", "https://m.baidu.com/s?word=x");
    let s2 = single(
        "tianqi_pn10.html",
        "天气",
        "https://m.baidu.com/s?word=x&pn=10",
    );
    let s3 = single(
        "tianqi_pn20.html",
        "天气",
        "https://m.baidu.com/s?word=x&pn=20",
    );
    let agg = aggregate("天气", 3, &[s1, s2, s3], &[], &[], &[]);
    assert_eq!(agg.to_value()["diagnostics"]["duplicates"], 0);
    assert_eq!(agg.results.len(), 19);
}

#[test]
fn related_searches_deduped_by_query() {
    let s1 = single("tianqi_p1.html", "天气", "https://m.baidu.com/s?word=x");
    let s2 = single(
        "tianqi_pn10.html",
        "天气",
        "https://m.baidu.com/s?word=x&pn=10",
    );
    let agg = aggregate("天气", 2, &[s1, s2], &[], &[], &[]);
    let queries: Vec<&str> = agg
        .related_searches
        .iter()
        .map(|r| r["query"].as_str().unwrap())
        .collect();
    let mut dedup = queries.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(queries.len(), dedup.len());
}

#[test]
fn ads_and_cards_kept_separate_with_provenance() {
    let s1 = single(
        "weather_en.html",
        "weather",
        "https://m.baidu.com/s?word=weather",
    );
    let agg = aggregate("weather", 1, &[s1], &[], &[], &[]);
    assert_eq!(agg.cards.len(), 6);
    assert!(agg.cards.iter().all(|c| c["page_pn"] == 0));
    assert!(agg.ads.is_empty());
}
