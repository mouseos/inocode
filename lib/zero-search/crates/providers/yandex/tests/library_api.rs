//! External-use tests: this file deliberately imports *only* from the crate
//! root (`use yandex_search::{...}`), exactly as a downstream library user
//! would. No per-vertical module paths, no internal helpers.
//!
//! Everything here is offline: constructing values, serialising results,
//! building URLs and checking that the async search methods exist with the
//! documented signatures (by taking references to them — calling them would
//! hit the network).

use yandex_search::{
    build_search_url, clean_text, decode_entities, urlencode, verticals, ImageItem, SearchResult,
    SuggestResult, Suggestion, VideoItem, WebItem, YandexSearchClient, PROVIDER,
};

#[test]
fn provider_constant_and_client_construction() {
    assert_eq!(PROVIDER, "yandex");
    // Construction performs no network I/O and needs no configuration.
    let _client = YandexSearchClient::new();
    let _default = YandexSearchClient::default();
}

#[test]
fn search_methods_are_reachable_from_the_root_client_type() {
    // Referencing each method as a function item type-checks its public
    // signature without performing any request.
    let _web = YandexSearchClient::web_search;
    let _image = YandexSearchClient::image_search;
    let _video = YandexSearchClient::video_search;
    let _suggest = YandexSearchClient::suggest;
}

#[test]
fn web_result_builds_and_serialises() {
    let item = WebItem {
        rank: 1,
        title: "Заголовок".to_string(),
        url: "https://example.com/".to_string(),
        description: Some("Описание".to_string()),
        site: None,
    };
    let result = SearchResult::new("web", "запрос", 1, vec![item])
        .with_next_page(Some(2))
        .with_note("note");
    assert!(result.ok);
    assert_eq!(result.vertical, "web");
    assert_eq!(result.count, 1);
    assert_eq!(result.items[0].title, "Заголовок");

    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["provider"], "yandex");
    assert_eq!(json["items"][0]["url"], "https://example.com/");
    // Yandex reports no exact total, so the field stays absent.
    assert!(json.get("total_hits").is_none());

    let back: SearchResult<WebItem> = serde_json::from_value(json).unwrap();
    assert_eq!(back.items[0].url, "https://example.com/");
}

#[test]
fn roundtrip_tolerates_omitted_empty_vecs() {
    // Empty Vec fields are skipped when serialising; deserialising must
    // still succeed (serde `default`). Regression test carried over from the
    // sibling libraries: this round-trip used to fail with
    // "missing field `related_queries`".
    let result: SearchResult<ImageItem> = SearchResult::new(
        "image",
        "запрос",
        1,
        vec![ImageItem {
            rank: 1,
            title: "img".to_string(),
            original_url: "https://example.com/a.jpg".to_string(),
            thumbnail_url: None,
            width: Some(640),
            height: Some(480),
            thumbnail_width: None,
            thumbnail_height: None,
            source_url: None,
            source_site: None,
        }],
    );
    let json = serde_json::to_string(&result).unwrap();
    assert!(!json.contains("related_queries"));
    let back: SearchResult<ImageItem> = serde_json::from_str(&json).unwrap();
    assert!(back.related_queries.is_empty());
    assert_eq!(back.items[0].width, Some(640));
}

#[test]
fn every_vertical_item_type_is_constructible_from_the_root() {
    let _image = ImageItem {
        rank: 1,
        title: "img".to_string(),
        original_url: "https://example.com/a.jpg".to_string(),
        thumbnail_url: Some("https://avatars.mds.yandex.net/i?id=x".to_string()),
        width: Some(640),
        height: Some(480),
        thumbnail_width: Some(226),
        thumbnail_height: Some(320),
        source_url: Some("https://example.com/page".to_string()),
        source_site: Some("example.com".to_string()),
    };
    let _video = VideoItem {
        rank: 1,
        title: "movie".to_string(),
        video_id: "123".to_string(),
        preview_url: "https://yandex.ru/video/touch/preview/123".to_string(),
        url: Some("https://vk.com/video1_2".to_string()),
        thumbnail_url: None,
        duration: Some("18:00".to_string()),
        duration_seconds: Some(1080),
        provider: Some("vk.com".to_string()),
        provider_name: Some("VK Видео".to_string()),
        published_date: Some("13 авг 2024".to_string()),
        views: None,
    };
    let suggest = SuggestResult {
        provider: PROVIDER.to_string(),
        vertical: "suggest".to_string(),
        ok: true,
        query: "тес".to_string(),
        count: 1,
        suggestions: vec![Suggestion {
            keyword: "тест".to_string(),
            search_url: "https://yandex.ru/search/touch/?text=x".to_string(),
        }],
    };
    assert_eq!(suggest.suggestions[0].keyword, "тест");
}

#[test]
fn offline_url_building_from_the_root() {
    assert_eq!(
        build_search_url("test", "web").unwrap(),
        "https://yandex.ru/search/touch/?text=test&ui=webmobileapp.yandex"
    );
    assert_eq!(
        build_search_url("test", "image").unwrap(),
        "https://yandex.ru/images/touch/search?text=test&source=tabbar&ui=webmobileapp.yandex"
    );
    // News is unsupported, with the reason in the error.
    let err = build_search_url("test", "news").unwrap_err().to_string();
    assert!(err.contains("dzen.ru"), "error: {err}");
}

#[test]
fn text_helpers_are_root_level() {
    assert_eq!(clean_text("<b>тест</b> &amp; ещё"), "тест & ещё");
    assert_eq!(decode_entities("a &lt; b"), "a < b");
    assert_eq!(urlencode("a b&c"), "a%20b%26c");
}

#[test]
fn verticals_listing_marks_news_unsupported() {
    let v = verticals();
    assert_eq!(v["provider"], PROVIDER);
    assert_eq!(v["auth"], "none");
    assert_eq!(v["config_required"], false);
    let entries = v["verticals"].as_array().unwrap();
    let supported: Vec<&str> = entries
        .iter()
        .filter(|e| e["supported"] == true)
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(supported, ["web", "image", "video", "suggest"]);
    let news = entries.iter().find(|e| e["id"] == "news").unwrap();
    assert_eq!(news["supported"], false);
    assert!(news["reason"].as_str().unwrap().contains("dzen.ru"));
}
