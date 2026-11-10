//! Fixture tests: the parsers are run against real SERP pages recorded
//! during the Yahoo Scout APK analysis (query "apple", suggest query
//! "app") and must reproduce the results the analysis verified by hand.
//! Everything here is offline.

use yahoo_us_search::{
    news_has_next_page, parse_image_next_offset, parse_image_results, parse_image_total,
    parse_news_results, parse_related_queries, parse_suggest_response, parse_video_next_offset,
    parse_video_results, parse_video_total, parse_web_results, web_has_next_page,
};

const WEB_P1: &str = include_str!("fixtures/fixture-web.html");
const WEB_P2: &str = include_str!("fixtures/fixture-web-p2.html");
const VIDEO: &str = include_str!("fixtures/fixture-video.html");
const IMAGE: &str = include_str!("fixtures/fixture-image.html");
const NEWS: &str = include_str!("fixtures/fixture-news.html");
const SUGGEST: &str = include_str!("fixtures/fixture-suggest.json");

#[test]
fn web_fixture_page_one() {
    let items = parse_web_results(WEB_P1, 7);
    assert_eq!(items.len(), 7);
    assert!(items.iter().all(|i| i.url.starts_with("http")));
    // The RU= redirect segment must be decoded for every result.
    assert!(items.iter().all(|i| !i.url.contains("r.search.yahoo.com")));
    assert!(items.iter().any(|i| i.url == "https://asw.apple.com/home"));
    assert!(web_has_next_page(WEB_P1));
}

#[test]
fn web_fixture_related_queries() {
    // Audit W-31: the page's AlgoRsItem config carries the related
    // terms (first relItems group).
    let related = parse_related_queries(WEB_P1);
    assert_eq!(related.len(), 8);
    assert!(related.contains(&"apple music".to_string()));
    assert!(related.contains(&"apple store".to_string()));
    // The other captured pages do not embed the config.
    assert!(parse_related_queries(VIDEO).is_empty());
    assert!(parse_related_queries(NEWS).is_empty());
}

#[test]
fn web_fixture_page_two_differs() {
    let p1 = parse_web_results(WEB_P1, 7);
    let p2 = parse_web_results(WEB_P2, 7);
    assert_eq!(p2.len(), 7);
    assert!(p2.iter().any(|i| !p1.iter().any(|a| a.url == i.url)));
    assert!(web_has_next_page(WEB_P2));
}

#[test]
fn video_fixture() {
    let items = parse_video_results(VIDEO, 60);
    assert_eq!(items.len(), 60);
    assert_eq!(items[0].url, "https://www.youtube.com/watch?v=hnZI5uUSdEo");
    assert_eq!(items[0].duration.as_deref(), Some("01:57"));
    assert_eq!(parse_video_total(VIDEO), Some(70));
    assert_eq!(parse_video_next_offset(VIDEO), Some(60));
}

#[test]
fn image_fixture() {
    // Recorded through the browser-grade client (the image host
    // refuses plain HTTP): query "apple", first page.
    let items = parse_image_results(IMAGE, 60);
    assert_eq!(items.len(), 60);
    assert_eq!(
        items[0].url,
        "https://www.archyde.com/ios-27-introduces-wi-fi-setup-renaming-and-enhanced-connectivity/"
    );
    assert!(items[0].image_url.contains("media.cnn.com"));
    assert_eq!(items[0].width, Some(2994));
    assert_eq!(items[0].height, Some(1684));
    assert!(items.iter().all(|i| i.image_url.starts_with("http")));
    assert!(items
        .iter()
        .all(|i| !i.url.contains("images.search.yahoo.com")));
    assert_eq!(parse_image_total(IMAGE), Some(536));
    assert_eq!(parse_image_next_offset(IMAGE), Some(60));
}

#[test]
fn news_fixture() {
    let items = parse_news_results(NEWS, 10);
    assert_eq!(items.len(), 10);
    assert_eq!(
        items[0].url,
        "https://macdailynews.com/2026/11/06/apple-tv-announces-season-three-for-knife-edge-chasing-michelin-stars/"
    );
    assert_eq!(items[0].source.as_deref(), Some("MacDailyNews"));
    assert!(items.iter().all(|i| !i.url.contains("r.search.yahoo.com")));
    assert!(news_has_next_page(NEWS));
}

#[test]
fn suggest_fixture() {
    let body: serde_json::Value = serde_json::from_str(SUGGEST).unwrap();
    let result = parse_suggest_response(&body, "app", 20);
    assert_eq!(result.count, 10);
    assert_eq!(result.suggestions[0].keyword, "app store");
    assert_eq!(result.suggestions[2].keyword, "apple");
}
