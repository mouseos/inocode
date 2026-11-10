//! Library surface test: everything below is imported from the crate root
//! only, the way an external consumer would use `bing_search`.

use bing_search::{
    build_search_url, clean_text, parse_image_results, parse_news_results, parse_suggest_json,
    parse_video_results, parse_web_results, suggest_url, verticals, web_api_url, BingSearchClient,
    NewsItem, SearchResult, Suggestion, WebItem, PROVIDER,
};

#[test]
fn root_imports_cover_the_documented_surface() {
    assert_eq!(PROVIDER, "bing");

    let v = verticals();
    let ids: Vec<&str> = v["verticals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["web", "image", "video", "news", "suggest"]);

    assert!(clean_text("<b>a</b>  b").eq("a b"));

    let empty: SearchResult<WebItem> = SearchResult::new("web", "q", 1, vec![]);
    assert_eq!(empty.count, 0);

    let _client_type_check: Option<BingSearchClient> = None;
    let _item_type_check: Option<NewsItem> = None;
    let _suggestion_type_check: Option<Suggestion> = None;
}

#[test]
fn url_builders_work_offline() {
    let web = build_search_url("テスト", "web", "ja-JP", "ja").unwrap();
    assert!(web.starts_with("https://www.bing.com/search?q="));
    assert!(web.contains("&mkt=ja-JP&setlang=ja&first=1"));

    let image = build_search_url("猫", "image", "ja-JP", "ja").unwrap();
    assert!(image.contains("/images/search?q="));
    assert!(image.contains("&form=L2GV01&sdkhh=1"));

    let suggest = suggest_url("テスト", "videos", "ja-JP", "ja").unwrap();
    assert!(suggest.contains("/api/v7/suggestions/videos?q="));

    let api = web_api_url("テスト", 2, "ja-JP", "ja");
    assert!(api.starts_with("https://www.bingapis.com/api/v7/search?appid="));
    assert!(api.contains("&cc=JP&setlang=ja&count=10&offset=10"));
}

#[test]
fn parsers_work_from_the_root() {
    let web_json: serde_json::Value = serde_json::json!({
        "webPages": {"value": [{"name": "T", "url": "https://example.com/"}]}
    });
    let web = parse_web_results(&web_json, 10).unwrap();
    assert_eq!(web.items.len(), 1);
    assert_eq!(web.items[0].title, "T");

    let video_html = r#"<div class="mc_vtvc"><a href="/videos/riverview/relatedvideo?q=x&amp;churl=https%3A%2F%2Fyoutu.be%2Fabc">v</a><div class="mc_vtvc_title" title="Clip"></div></div>"#;
    let videos = parse_video_results(video_html, 10).unwrap();
    assert_eq!(videos.items[0].url, "https://youtu.be/abc");

    let news_html = r#"<div class="news-card" url="https://example.com/n" data-title="Head" data-author="Wire"></div>"#;
    let news = parse_news_results(news_html, 10).unwrap();
    assert_eq!(news[0].publisher.as_deref(), Some("Wire"));

    let image_html = r##"<a class="iusc" m="{&quot;murl&quot;:&quot;https://example.com/i.jpg&quot;,&quot;t&quot;:&quot;Pic&quot;}">x</a>"##;
    let images = parse_image_results(image_html, 1, 10).unwrap();
    assert_eq!(images.items[0].title, "Pic");

    let json: serde_json::Value = serde_json::json!({
        "suggestionGroups": [{"searchSuggestions": [{"displayText": "abc", "url": "https://www.bing.com/search?q=abc"}]}]
    });
    let suggestions = parse_suggest_json(&json);
    assert_eq!(suggestions[0].keyword, "abc");
}
