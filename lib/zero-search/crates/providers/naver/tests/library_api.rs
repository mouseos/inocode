//! External-use tests: this file deliberately imports *only* from the
//! crate root (`use naver_search::{...}`), exactly as a downstream
//! library user would. No per-vertical module paths, no internal helpers.
//!
//! Everything here is offline: constructing values, serialising results,
//! building URLs, parsing saved pages and checking that the async search
//! methods exist with the documented signatures (by taking references to
//! them — calling them would hit the network).

use naver_search::{
    build_page_url, build_search_url, clean_text, decode_entities, parse_image_results,
    parse_suggest_json, suggest_url, urlencode, verticals, BlogItem, ImageItem, IntegratedResult,
    NaverSearchClient, NewsItem, SearchResult, SuggestResult, Suggestion, VideoItem, WebItem,
    PROVIDER,
};

#[test]
fn provider_constant_and_client_construction() {
    assert_eq!(PROVIDER, "naver");
    // Construction performs no network I/O and needs no configuration.
    let _client = NaverSearchClient::new();
    let _default = NaverSearchClient::default();
}

#[test]
fn search_methods_are_reachable_from_the_root_client_type() {
    // Referencing each method as a function item type-checks its public
    // signature without performing any request.
    let _integrated = NaverSearchClient::search;
    let _web = NaverSearchClient::web_search;
    let _image = NaverSearchClient::image_search;
    let _news = NaverSearchClient::news_search;
    let _video = NaverSearchClient::video_search;
    let _blog = NaverSearchClient::blog_search;
    let _suggest = NaverSearchClient::suggest;
}

#[test]
fn web_result_builds_and_serialises() {
    let item = WebItem {
        rank: 1,
        title: "제목".to_string(),
        url: "https://example.com/".to_string(),
        snippet: Some("설명".to_string()),
    };
    let result = SearchResult::new("web", "쿼리", 1, vec![item]).with_note("note");
    assert!(result.ok);
    assert_eq!(result.vertical, "web");
    assert_eq!(result.count, 1);
    assert_eq!(result.items[0].title, "제목");

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["provider"], "naver");
    assert_eq!(json["items"][0]["url"], "https://example.com/");
    // Naver reports no total hit count; the field must not appear.
    assert!(json.get("total_hits").is_none());

    let back: SearchResult<WebItem> = serde_json::from_value(json).unwrap();
    assert_eq!(back.items[0].url, "https://example.com/");
}

#[test]
fn every_vertical_item_type_is_constructible_from_the_root() {
    let _image = ImageItem {
        rank: 1,
        title: Some("고양이".to_string()),
        url: "https://example.com/a.jpg".to_string(),
        thumbnail: None,
        source_page: None,
        source: Some("네이버 블로그".to_string()),
        width: Some(800),
        height: Some(600),
    };
    let _video = VideoItem {
        rank: 1,
        title: "영상".to_string(),
        url: "https://m.youtube.com/watch?v=x".to_string(),
        source: Some("youtube".to_string()),
        channel: None,
        duration: Some("08:03".to_string()),
        published: None,
        views: None,
        thumbnail: None,
    };
    let _news = NewsItem {
        rank: 1,
        title: "見出し".to_string(),
        url: "https://n.news.naver.com/article/001/0000000001".to_string(),
        publisher: Some("연합뉴스".to_string()),
        published: Some("8시간 전".to_string()),
        snippet: None,
        thumbnail: None,
    };
    let _blog = BlogItem {
        rank: 1,
        title: "포스트".to_string(),
        url: "https://m.blog.naver.com/example/1".to_string(),
        blog_name: Some("예제 블로그".to_string()),
        blog_id: Some("example".to_string()),
        published: None,
        snippet: None,
        thumbnail: None,
    };
    let suggest = SuggestResult {
        provider: PROVIDER.to_string(),
        vertical: "suggest".to_string(),
        ok: true,
        query: "네이".to_string(),
        count: 1,
        suggestions: vec![Suggestion {
            keyword: "네이버".to_string(),
            search_url: build_search_url("네이버", "integrated").unwrap(),
            kind: Some("0".to_string()),
        }],
        answer: vec![],
        intend: vec![],
    };
    assert_eq!(suggest.suggestions[0].keyword, "네이버");

    let integrated = IntegratedResult {
        provider: PROVIDER.to_string(),
        vertical: "integrated".to_string(),
        ok: true,
        query: "테스트".to_string(),
        page: 1,
        count: 0,
        sections: vec![],
    };
    assert_eq!(integrated.vertical, "integrated");
}

#[test]
fn url_builders_are_root_level() {
    let integrated = build_search_url("테스트", "integrated").unwrap();
    assert!(integrated.starts_with("https://m.search.naver.com/search.naver?where=m"));

    // Only first pages have URLs for the tab verticals: tab pages
    // ignore page parameters, and deeper batches come from the
    // page-issued more-API chain. The web view is the exception —
    // it pages through its own bar hrefs (audit W-22).
    for v in ["image", "news", "video", "blog", "cafe", "kin"] {
        let err = build_page_url("테스트", v, 2).unwrap_err();
        assert!(
            err.to_string().starts_with("unsupported_page:"),
            "{v}: {err}"
        );
    }
    assert!(build_page_url("테스트", "web", 2)
        .unwrap()
        .contains("start=21"));

    assert!(suggest_url("네이").contains("ac.search.naver.com"));
}

#[test]
fn offline_parsers_are_root_level() {
    let html = r#"<script>var imageSearchTabData = { content: { items: [
        {"type":"image","title":"t","originalUrl":"http://example.com/a.jpg"}
    ] } };</script>"#;
    let items = parse_image_results(html, 10).unwrap();
    assert_eq!(items[0].url, "http://example.com/a.jpg");

    let value: serde_json::Value = serde_json::from_str(r#"{"items":[[["네이버","0"]]]}"#).unwrap();
    assert_eq!(parse_suggest_json(&value)[0].keyword, "네이버");
}

#[test]
fn text_helpers_are_root_level() {
    assert_eq!(
        clean_text("<mark>테스트</mark> &amp; 실험"),
        "테스트 & 실험"
    );
    assert_eq!(decode_entities("a &lt; b"), "a < b");
    assert_eq!(urlencode("a b&c"), "a%20b%26c");
}

#[test]
fn verticals_listing_covers_all_searches() {
    let v = verticals();
    assert_eq!(v["provider"], PROVIDER);
    assert_eq!(v["auth"], "none");
    assert_eq!(v["config_required"], false);
    let ids: Vec<&str> = v["verticals"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
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
    let unsupported: Vec<&str> = v["unsupported"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    assert_eq!(unsupported, ["ai", "clip"]);
}
