//! Offline parser tests against the real pages captured during the APK
//! analysis (query `테스트` / `고양이`, mobile host, 2026-11-06). These
//! pin the parsers to Naver's actual markup: if a parser change loosens
//! or breaks extraction, the counts and representative fields below move.

use naver_search::{
    extract_more_url, map_image_items, parse_blog_results, parse_cafe_results, parse_image_results,
    parse_integrated_results, parse_kin_results, parse_more_response, parse_news_results,
    parse_suggest_json, parse_video_results, parse_web_results, MorePayload,
};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn web_tab_fixture_batch() {
    // This capture was fetched with a page=2 URL during the analysis.
    // Audit W-22 (2026-11-08) resolved why it matches page 1: the
    // bar's page=2 link carries start=1, i.e. it points back at
    // batch 1 — the batches do change (see web_paging_fixtures).
    // The ranks are Naver's own (sparse: 1, 1, 5, 6, 6, 18, 18, 20, 20).
    let items = parse_web_results(&fixture("fixture-web-mobile.html"), 20).unwrap();
    assert_eq!(items.len(), 9, "the tab batch lists 9 results");
    assert_eq!(items[0].url, "https://test-it.co.kr/");
    assert!(items[0].title.contains("test it"));
    assert!(items.iter().all(|i| i.url.starts_with("http")));
    assert!(items.iter().all(|i| !i.title.is_empty()));
    let ranks: Vec<i64> = items.iter().map(|i| i.rank).collect();
    assert_eq!(ranks, [1, 1, 5, 6, 6, 18, 18, 20, 20]);
}

#[test]
fn image_fixture_items_carry_original_urls() {
    let items = parse_image_results(&fixture("fixture-image-mobile.html"), 50).unwrap();
    assert!(
        items.len() >= 40,
        "expected the embedded batch of ~46 images, got {}",
        items.len()
    );
    for item in &items {
        assert!(item.url.starts_with("http"), "original URL: {}", item.url);
        assert!(
            !item.url.contains("search.pstatic.net"),
            "original must not be the proxy URL: {}",
            item.url
        );
        assert!(
            item.thumbnail.is_some(),
            "every image has a proxy thumbnail"
        );
    }
    assert!(items.iter().any(|i| i.title.is_some()));
    assert!(items.iter().any(|i| i.source_page.is_some()));
}

#[test]
fn news_fixture_groups_articles() {
    let items = parse_news_results(&fixture("fixture-news-mobile.html"), 30).unwrap();
    assert!(
        items.len() >= 10,
        "expected at least the 13 Naver-hosted articles, got {}",
        items.len()
    );
    assert!(items
        .iter()
        .any(|i| i.url.contains("n.news.naver.com/article/")));
    assert!(items.iter().all(|i| !i.title.is_empty()));
    assert!(
        items.iter().any(|i| i.publisher.is_some()),
        "press anchors should name at least one publisher"
    );
    assert!(
        items.iter().any(|i| i.snippet.is_some()),
        "snippet anchors should fill at least one snippet"
    );
}

#[test]
fn video_fixture_cards() {
    let items = parse_video_results(&fixture("fixture-video-mobile.html"), 48).unwrap();
    assert!(
        items.len() >= 20,
        "expected the video batch (45 triggers on the page), got {}",
        items.len()
    );
    assert!(items.iter().all(|i| i.url.starts_with("http")));
    assert!(items.iter().all(|i| !i.title.is_empty()));
    assert!(
        items.iter().any(|i| i.source.as_deref() == Some("youtube")),
        "data-source should surface the hosting platform"
    );
    assert!(items.iter().any(|i| i.duration.is_some()));
    assert!(items.iter().any(|i| i.thumbnail.is_some()));
}

#[test]
fn blog_fixture_posts() {
    let items = parse_blog_results(&fixture("fixture-blog-mobile.html"), 30).unwrap();
    assert_eq!(
        items.len(),
        30,
        "the blog tab's first batch is exactly 30 posts"
    );
    for item in &items {
        assert!(item.url.starts_with("https://m.blog.naver.com/"));
        assert!(item.blog_id.is_some());
        assert!(!item.title.is_empty());
    }
    assert!(items.iter().any(|i| i.blog_name.is_some()));
    assert!(items.iter().any(|i| i.snippet.is_some()));
}

#[test]
fn integrated_fixture_sections() {
    let sections =
        parse_integrated_results(&fixture("fixture-integrated-mobile.html"), 10).unwrap();
    assert!(
        sections.len() >= 5,
        "expected several subject sections, got {}",
        sections.len()
    );
    let names: Vec<&str> = sections.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"뉴스"), "sections: {names:?}");
    // Note: this capture's "이미지" section is a related-searches tag
    // cloud (relative links only), so it correctly yields no items and
    // is dropped — section composition varies by query.
    assert!(sections.iter().all(|s| !s.items.is_empty()));
    assert!(sections
        .iter()
        .flat_map(|s| &s.items)
        .all(|i| i.url.starts_with("http") && !i.title.is_empty()));
    // Audit W-27: generic (non-web) sections now surface snippets
    // where the page carries body text for an item. (The 뉴스
    // section's cards carry no body text in the SSR markup, so the
    // assertion is over all sections, not per-section.)
    assert!(sections
        .iter()
        .flat_map(|s| &s.items)
        .any(|i| i.snippet.is_some()));
}

#[test]
fn web_paging_fixtures_are_disjoint_batches() {
    // Captured live 2026-11-08 (audit W-22): batch 1 (the tab URL)
    // and batch 2 (the bar's page=3&start=21 href) share no result
    // URL — the web view really pages.
    let batch1 = parse_web_results(&fixture("fixture-web-paging.html"), 30).unwrap();
    let batch2 = parse_web_results(&fixture("fixture-web-page2-mobile.html"), 30).unwrap();
    assert!(!batch1.is_empty() && !batch2.is_empty());
    let urls1: Vec<&str> = batch1.iter().map(|i| i.url.as_str()).collect();
    assert!(batch2.iter().all(|i| !urls1.contains(&i.url.as_str())));
}

#[test]
fn cafe_fixture_parses_posts() {
    // Captured live 2026-11-08 (audit W-21), query 커피.
    let items = parse_cafe_results(&fixture("fixture-cafe-mobile.html"), 30).unwrap();
    assert!(!items.is_empty(), "cafe items empty");
    assert!(items
        .iter()
        .all(|i| i.url.contains("cafe.naver.com/") && !i.title.is_empty()));
    assert!(items.iter().any(|i| i.cafe_name.is_some()));
    assert!(items.iter().any(|i| i.snippet.is_some()));
}

#[test]
fn kin_fixture_parses_questions() {
    // Captured live 2026-11-08 (audit W-21), query 보험.
    let items = parse_kin_results(&fixture("fixture-kin-mobile.html"), 30).unwrap();
    assert!(!items.is_empty(), "kin items empty");
    assert!(items
        .iter()
        .all(|i| i.url.contains("kin.naver.com/qna/") && !i.title.is_empty()));
    assert!(items.iter().any(|i| i.snippet.is_some()));
}

#[test]
fn suggest_fixture_json() {
    let value: serde_json::Value = serde_json::from_str(&fixture("fixture-suggest.json")).unwrap();
    let out = parse_suggest_json(&value);
    assert_eq!(out.len(), 10);
    assert_eq!(out[0].keyword, "네이버웹툰");
    // Audit W-25: the candidate kind (second pair element) is kept.
    assert_eq!(out[0].kind.as_deref(), Some("0"));
    assert_eq!(out[1].kind.as_deref(), Some("1"));
    assert!(out
        .iter()
        .all(|s| s.search_url.contains("m.search.naver.com")));
}

// ── More-API chain: URL extraction from tab pages and parsing of the
// captured API responses (live captures, 2026-11-06). ──

#[test]
fn more_url_is_embedded_in_each_chained_tab_page() {
    let image = extract_more_url(&fixture("fixture-image-mobile.html"), "image").unwrap();
    assert!(image.contains("/p/c/image/"), "{image}");
    assert!(image.contains("api_type=mobile_tab_more"), "{image}");

    let news = extract_more_url(&fixture("fixture-news-mobile.html"), "news").unwrap();
    assert!(news.contains("/p/newssearch/"), "{news}");
    assert!(news.contains("start=16"), "{news}");

    let video = extract_more_url(&fixture("fixture-video-mobile.html"), "video").unwrap();
    assert!(video.contains("/p/video/"), "{video}");
    assert!(video.contains("video_more=1"), "{video}");

    let blog = extract_more_url(&fixture("fixture-blog-mobile.html"), "blog").unwrap();
    assert!(blog.contains("/p/review/"), "{blog}");

    // The web tab issues no more-API URL.
    assert!(extract_more_url(&fixture("fixture-web-mobile.html"), "web").is_none());
}

fn api_fixture(name: &str) -> serde_json::Value {
    serde_json::from_str(&fixture(name)).unwrap()
}

#[test]
fn api_image_page_two_parses() {
    let (payload, next) =
        parse_more_response("image", &api_fixture("api-image-page2.json")).unwrap();
    let MorePayload::ImageItems(raw) = payload else {
        panic!("image API payload must be items");
    };
    let items = map_image_items(&raw, usize::MAX);
    assert!(
        items.len() >= 90,
        "expected the ~98 image entries of batch 2, got {}",
        items.len()
    );
    assert!(items
        .iter()
        .all(|i| i.url.starts_with("http") && !i.url.contains("search.pstatic.net")));
    let next = next.expect("the chain continues");
    assert!(next.contains("logStart=151"), "{next}");
}

#[test]
fn api_video_page_two_parses() {
    let (payload, next) =
        parse_more_response("video", &api_fixture("api-video-page2.json")).unwrap();
    let MorePayload::Html(html) = payload else {
        panic!("video API payload must be HTML");
    };
    let items = parse_video_results(&html, usize::MAX).unwrap();
    assert!(
        items.len() >= 20,
        "expected a full video batch, got {}",
        items.len()
    );
    assert!(next.is_some());
}

#[test]
fn api_news_page_two_parses() {
    let (payload, next) = parse_more_response("news", &api_fixture("api-news-page2.json")).unwrap();
    let MorePayload::Html(html) = payload else {
        panic!("news API payload must be HTML");
    };
    let items = parse_news_results(&html, usize::MAX).unwrap();
    assert!(
        items.len() >= 10,
        "expected a full news batch, got {}",
        items.len()
    );
    assert!(next.is_some());
}

#[test]
fn api_blog_page_two_parses() {
    let (payload, next) = parse_more_response("blog", &api_fixture("api-blog-page2.json")).unwrap();
    let MorePayload::Html(html) = payload else {
        panic!("blog API payload must be HTML");
    };
    let items = parse_blog_results(&html, usize::MAX).unwrap();
    assert!(
        items.len() >= 20,
        "expected a full blog batch, got {}",
        items.len()
    );
    assert!(next.is_some());
}
