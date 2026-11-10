//! Offline parser tests against pages captured from the live service
//! (2026-11-07, query "test", one app-shaped session). The fixtures are the
//! measured ground truth for the selectors and JSON shapes the parsers rely
//! on — if Yandex changes the layout, these tests say so before the live
//! tests do.

use yandex_search::{
    approx_total_text, parse_image_results, parse_special_cards, parse_suggest_body,
    parse_suggest_endings_body, parse_video_results, parse_web_results,
};

const WEB_P1: &str = include_str!("fixtures/fixture-web-p1.html");
const WEB_P2: &str = include_str!("fixtures/fixture-web-p2.html");
const IMAGES: &str = include_str!("fixtures/fixture-images.html");
const VIDEO: &str = include_str!("fixtures/fixture-video.html");
const SUGGEST: &str = include_str!("fixtures/fixture-suggest.json");
const SUGGEST_ENDINGS: &str = include_str!("fixtures/fixture-suggest-endings.json");

#[test]
fn web_page1_special_cards_are_surfaced() {
    // Audit W-37: page 1's non-organic cards — the AI answer, the
    // adv_unisearch block, the related-searches card and the
    // labelled ad — come back as their own kinds.
    let cards = parse_special_cards(WEB_P1);
    let kinds: Vec<&str> = cards.iter().map(|c| c.kind.as_str()).collect();
    assert!(kinds.contains(&"neuro_answer"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"adv_unisearch"), "kinds: {kinds:?}");
    assert!(kinds.contains(&"ad"), "kinds: {kinds:?}");
    let ai = cards.iter().find(|c| c.kind == "neuro_answer").unwrap();
    assert!(ai.text.as_deref().unwrap().contains("Алисы AI"));
}

#[test]
fn suggest_endings_fixture_parses_four_candidates() {
    // The analysis' live capture (round 3, trial 6b).
    let body: serde_json::Value = serde_json::from_str(SUGGEST_ENDINGS).unwrap();
    let result = parse_suggest_endings_body(&body, "test", 20);
    assert_eq!(result.count, 4);
    assert_eq!(result.suggestions[0].keyword, "testcenter kz");
}

#[test]
fn web_page1_yields_ten_organic_results() {
    let items = parse_web_results(WEB_P1, usize::MAX);
    // 13 serp-item cards: 1 AI-answer card, 1 adv_unisearch block and 1
    // labelled ad are excluded, leaving exactly the 10 organic results.
    assert_eq!(
        items.len(),
        10,
        "items: {:?}",
        items.iter().map(|i| &i.url).collect::<Vec<_>>()
    );
    let first = &items[0];
    assert_eq!(first.rank, 1);
    assert_eq!(first.url, "https://onlinetestpad.com/ru");
    assert!(
        first.title.contains("Online Test Pad"),
        "title: {}",
        first.title
    );
    assert!(
        !first.title.contains("<b>"),
        "markup leaked: {}",
        first.title
    );
    assert_eq!(first.site.as_deref(), Some("onlinetestpad.com › ru"));
    let snippet = first.description.as_deref().unwrap_or("");
    assert!(snippet.contains("Online Test Pad"), "snippet: {snippet}");
    // No click-counter or Yandex-internal URL may survive as a result.
    assert!(items
        .iter()
        .all(|i| !i.url.contains("/an/count/") && !i.url.contains("yabs.yandex.ru")));
    assert_eq!(
        items[1].url,
        "https://ru.m.wikipedia.org/wiki/%D0%A2%D0%B5%D1%81%D1%82"
    );
}

#[test]
fn web_page2_differs_from_page1() {
    let p1 = parse_web_results(WEB_P1, usize::MAX);
    let p2 = parse_web_results(WEB_P2, usize::MAX);
    assert_eq!(p2.len(), 10);
    assert_eq!(p2[0].url, "https://pikuco.ru/");
    let urls1: Vec<&str> = p1.iter().map(|i| i.url.as_str()).collect();
    assert!(
        p2.iter().any(|i| !urls1.contains(&i.url.as_str())),
        "page 2 must contain results page 1 does not"
    );
    // Measured new-on-page-2 destinations.
    assert!(p2.iter().any(|i| i.url.contains("context.reverso.net")));
    assert!(p2.iter().any(|i| i.url.contains("videouroki.net")));
}

#[test]
fn web_approx_total_is_read_from_page_text() {
    let text = approx_total_text(WEB_P1).unwrap();
    assert!(text.contains("195"), "approx total: {text}");
    assert!(text.contains("результатов"), "approx total: {text}");
}

#[test]
fn web_limit_is_applied() {
    let items = parse_web_results(WEB_P1, 3);
    assert_eq!(items.len(), 3);
    assert_eq!(items[2].rank, 3);
}

#[test]
fn image_page_yields_twenty_results_with_measured_fields() {
    let items = parse_image_results(IMAGES, usize::MAX).unwrap();
    assert_eq!(items.len(), 20);
    let first = &items[0];
    assert_eq!(first.rank, 1);
    assert_eq!(first.title, "Тесты за пять минут");
    assert_eq!(
        first.original_url,
        "https://s3.objstor.cloud4u.com/unti-prod-people/file/presentation/project/evdghrqp5a.jpg"
    );
    let thumb = first.thumbnail_url.as_deref().unwrap_or("");
    assert!(
        thumb.starts_with("https://avatars.mds.yandex.net/i?id="),
        "thumbnail: {thumb}"
    );
    assert!(thumb.contains("images-thumbs"), "thumbnail: {thumb}");
    assert_eq!(first.width, Some(1600));
    assert_eq!(first.height, Some(862));
    assert_eq!(first.thumbnail_width, Some(480));
    assert_eq!(first.thumbnail_height, Some(259));
    assert_eq!(
        first.source_url.as_deref(),
        Some("https://pt.2035.university/project/testy-za-pat-minut")
    );
    assert_eq!(first.source_site.as_deref(), Some("pt.2035.university"));
    // Every item has an original image and a distinct source.
    assert!(items.iter().all(|i| i.original_url.starts_with("http")));
    assert_eq!(
        items[1].title,
        "B1 DIAGNOSTIC TEST: Pусский РКИ pабочие листы пдф и док"
    );
}

#[test]
fn video_page_yields_four_cards_enriched_from_json() {
    let items = parse_video_results(VIDEO, usize::MAX);
    assert_eq!(items.len(), 4, "items: {items:?}");
    let first = &items[0];
    assert_eq!(first.rank, 1);
    assert!(
        first.title.starts_with("A1 Listening Test"),
        "title: {}",
        first.title
    );
    assert_eq!(first.video_id, "14654444776412803979");
    assert_eq!(
        first.url.as_deref(),
        Some("http://vk.com/video771543382_456239161")
    );
    assert_eq!(first.duration.as_deref(), Some("18:00"));
    assert_eq!(first.duration_seconds, Some(1080));
    assert_eq!(first.provider.as_deref(), Some("vk.com"));
    assert_eq!(first.provider_name.as_deref(), Some("VK Видео"));
    assert_eq!(first.published_date.as_deref(), Some("13 авг 2024"));
    let thumb = first.thumbnail_url.as_deref().unwrap_or("");
    assert!(
        thumb.starts_with("https://avatars.mds.yandex.net/get-vthumb/"),
        "thumbnail: {thumb}"
    );
    assert!(first
        .preview_url
        .contains("/video/touch/preview/14654444776412803979"));
    // Second card: IQ test on VK, 13:34 = 814 s.
    assert_eq!(items[1].duration_seconds, Some(814));
    assert_eq!(
        items[1].url.as_deref(),
        Some("http://vk.com/video-23936657_456241242")
    );
    // Third/fourth cards resolve to RuTube watch URLs.
    assert!(items[2].url.as_deref().unwrap_or("").contains("rutube.ru"));
    assert!(items[3].url.as_deref().unwrap_or("").contains("rutube.ru"));
}

#[test]
fn suggest_fixture_parses_four_candidates() {
    let body: serde_json::Value = serde_json::from_str(SUGGEST).unwrap();
    let result = parse_suggest_body(&body, "test", 20);
    let keywords: Vec<&str> = result
        .suggestions
        .iter()
        .map(|s| s.keyword.as_str())
        .collect();
    assert_eq!(
        keywords,
        [
            "testcenter kz",
            "test speed",
            "test english",
            "test your english"
        ]
    );
    assert_eq!(result.count, 4);
    assert!(result.suggestions[0]
        .search_url
        .starts_with("https://yandex.ru/search/touch/?text="));
}
