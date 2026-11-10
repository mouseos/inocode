//! Parse SERP pages saved during the Yahoo Scout APK analysis — no network.
//!
//! The recorded fixtures in `tests/fixtures/` (query "apple", suggest
//! query "app") are fed through the same parsers the live searches use,
//! which makes this example a quick end-to-end sanity check of the
//! extraction logic.
//!
//! Run with:
//! `cargo run -p yahoo-us-mcp-provider --example parse_saved_pages`

use yahoo_us_search::{
    parse_news_results, parse_suggest_response, parse_video_results, parse_video_total,
    parse_web_results,
};

fn fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn main() {
    let web = parse_web_results(&fixture("fixture-web.html"), 7);
    println!("web: {} results", web.len());
    for item in &web {
        println!("  {}. {} — {}", item.rank, item.title, item.url);
    }

    let video_html = fixture("fixture-video.html");
    let videos = parse_video_results(&video_html, 60);
    println!(
        "video: {} results (total {:?})",
        videos.len(),
        parse_video_total(&video_html)
    );
    for item in videos.iter().take(3) {
        println!(
            "  {}. {} — {} ({})",
            item.rank,
            item.title,
            item.url,
            item.duration.as_deref().unwrap_or("?")
        );
    }

    let news = parse_news_results(&fixture("fixture-news.html"), 10);
    println!("news: {} articles", news.len());
    for item in news.iter().take(3) {
        println!(
            "  {}. {} — {} ({})",
            item.rank,
            item.title,
            item.url,
            item.source.as_deref().unwrap_or("?")
        );
    }

    let body: serde_json::Value = serde_json::from_str(&fixture("fixture-suggest.json")).unwrap();
    let suggest = parse_suggest_response(&body, "app", 20);
    println!(
        "suggest: {} keywords: {}",
        suggest.count,
        suggest
            .suggestions
            .iter()
            .map(|s| s.keyword.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
}
