//! External-use tests: this file deliberately imports *only* from the crate
//! root (`use yahoo_us_search::{...}`), exactly as a downstream library user
//! would. No per-vertical module paths, no internal helpers.
//!
//! Everything here is offline: constructing values, serialising results,
//! building URLs, parsing recorded pages and checking that the async search
//! methods exist with the documented signatures (by taking references to
//! them — calling them would hit the network).

use yahoo_us_search::{
    build_search_url, clean_text, decode_entities, decode_redirect_url, parse_news_results,
    parse_suggest_response, parse_video_results, parse_web_results, urlencode, verticals,
    ImageItem, NewsItem, SearchResult, SuggestResult, Suggestion, VideoItem, WebItem,
    YahooUsSearchClient, PROVIDER,
};

#[test]
fn provider_constant_and_client_construction() {
    assert_eq!(PROVIDER, "yahoo_us");
    // Construction performs no network I/O and needs no configuration.
    let _client = YahooUsSearchClient::new();
    let _default = YahooUsSearchClient::default();
}

#[test]
fn search_methods_are_reachable_from_the_root_client_type() {
    // Referencing each method as a function item type-checks its public
    // signature without performing any request.
    let _web = YahooUsSearchClient::web_search;
    let _image = YahooUsSearchClient::image_search;
    let _video = YahooUsSearchClient::video_search;
    let _news = YahooUsSearchClient::news_search;
    let _suggest = YahooUsSearchClient::suggest;
}

#[test]
fn web_result_builds_and_serialises() {
    let item = WebItem {
        rank: 1,
        title: "Apple".to_string(),
        url: "https://www.apple.com/".to_string(),
        description: Some("Think different".to_string()),
        site: Some("apple.com".to_string()),
    };
    let result = SearchResult::new("web", "apple", 1, vec![item]).with_next_page(Some(2));
    assert!(result.ok);
    assert_eq!(result.vertical, "web");
    assert_eq!(result.count, 1);
    // The US web SERP has no total: the field must be absent, not zero.
    assert_eq!(result.total_hits, None);

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["provider"], "yahoo_us");
    assert!(json.get("total_hits").is_none());
    assert_eq!(json["items"][0]["url"], "https://www.apple.com/");

    let back: SearchResult<WebItem> = serde_json::from_value(json).unwrap();
    assert_eq!(back.items[0].url, "https://www.apple.com/");
}

#[test]
fn image_result_builds_and_serialises() {
    let item = ImageItem {
        rank: 1,
        title: "Apple logo".to_string(),
        url: "https://example.com/story".to_string(),
        image_url: "https://example.com/logo.jpg".to_string(),
        thumbnail: Some("https://tse1.mm.bing.net/th/id/OIP.x".to_string()),
        source: Some("example.com".to_string()),
        width: Some(1200),
        height: Some(800),
    };
    let result = SearchResult::new("image", "apple", 1, vec![item]).with_total(Some(536));
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["vertical"], "image");
    assert_eq!(
        json["items"][0]["image_url"],
        "https://example.com/logo.jpg"
    );
    assert_eq!(json["items"][0]["width"], 1200);

    let back: SearchResult<ImageItem> = serde_json::from_value(json).unwrap();
    assert_eq!(back.items[0].image_url, "https://example.com/logo.jpg");
}

#[test]
fn roundtrip_tolerates_omitted_empty_vecs() {
    // Empty Vec fields are skipped when serialising; deserialising must
    // still succeed (serde `default`).
    let result: SearchResult<NewsItem> = SearchResult::new(
        "news",
        "apple",
        1,
        vec![NewsItem {
            rank: 1,
            title: "Headline".to_string(),
            url: "https://example.com/a".to_string(),
            source: None,
            published: None,
            description: None,
            thumbnail: None,
        }],
    );
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("related_queries"));
    let back: SearchResult<NewsItem> = serde_json::from_str(&json).unwrap();
    assert!(back.related_queries.is_empty());
}

#[test]
fn every_vertical_item_type_is_constructible_from_the_root() {
    let _video = VideoItem {
        rank: 1,
        title: "movie".to_string(),
        url: "https://www.youtube.com/watch?v=x".to_string(),
        embed_url: None,
        duration: Some("01:57".to_string()),
        source: Some("YouTube".to_string()),
        description: None,
        thumbnail: None,
    };
    let _news = NewsItem {
        rank: 1,
        title: "Headline".to_string(),
        url: "https://example.com/a".to_string(),
        source: Some("Example News".to_string()),
        published: Some("2 hours ago".to_string()),
        description: None,
        thumbnail: None,
    };
    let suggest = SuggestResult {
        provider: PROVIDER.to_string(),
        vertical: "suggest".to_string(),
        ok: true,
        query: "app".to_string(),
        count: 1,
        suggestions: vec![Suggestion {
            keyword: "app store".to_string(),
            search_url: "https://search.yahoo.com/search?p=app%20store&b=1&pz=7".to_string(),
            mrk: Some(6),
        }],
    };
    assert_eq!(suggest.suggestions[0].keyword, "app store");
}

#[test]
fn build_search_url_covers_every_vertical() {
    assert_eq!(
        build_search_url("apple", "web", 1).unwrap(),
        "https://search.yahoo.com/search?p=apple&b=1&pz=7"
    );
    assert_eq!(
        build_search_url("apple", "video", 1).unwrap(),
        "https://video.search.yahoo.com/search/video?p=apple&b=1"
    );
    assert_eq!(
        build_search_url("apple", "news", 1).unwrap(),
        "https://news.search.yahoo.com/search?p=apple&b=1"
    );
    assert_eq!(
        build_search_url("app", "suggest", 1).unwrap(),
        "https://search.yahoo.com/sugg/gossip/gossip-us-ura/?command=app&output=json"
    );
    assert!(build_search_url("x", "maps", 1).is_err());
}

#[test]
fn offline_parsers_are_root_level() {
    let web_html = include_str!("fixtures/fixture-web.html");
    assert_eq!(parse_web_results(web_html, 7).len(), 7);
    let video_html = include_str!("fixtures/fixture-video.html");
    assert_eq!(parse_video_results(video_html, 60).len(), 60);
    let news_html = include_str!("fixtures/fixture-news.html");
    assert_eq!(parse_news_results(news_html, 10).len(), 10);
    let body: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/fixture-suggest.json")).unwrap();
    assert_eq!(parse_suggest_response(&body, "app", 20).count, 10);
}

#[test]
fn text_and_url_helpers_are_root_level() {
    assert_eq!(clean_text("<b>Apple</b> &amp; more"), "Apple & more");
    assert_eq!(decode_entities("a &lt; b"), "a < b");
    assert_eq!(urlencode("a b&c"), "a%20b%26c");
    assert_eq!(
        decode_redirect_url(
            "https://r.search.yahoo.com/x/RV=2/RU=https%3a%2f%2fexample.com%2f/RK=2/RS=y"
        ),
        "https://example.com/"
    );
}

#[test]
fn verticals_listing_includes_image_search() {
    let v = verticals();
    assert_eq!(v["provider"], PROVIDER);
    assert_eq!(v["auth"], "none");
    let entries = v["verticals"].as_array().unwrap();
    let supported: Vec<&str> = entries
        .iter()
        .filter(|e| e["supported"] == true)
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(supported, ["web", "video", "news", "suggest", "image"]);
    let image = entries.iter().find(|e| e["id"] == "image").unwrap();
    assert_eq!(image["supported"], true);
    assert_eq!(image["tool"], "yahoo_us_image_search");
    assert_eq!(image["paginated"], true);
}
