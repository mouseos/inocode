//! Offline basics for using `naver_search` as a library — no network.
//!
//! Shows the parts of the public API that work without contacting
//! Naver: building result values, serialising them, building search
//! URLs, listing the verticals, and parsing a saved image tab page.
//!
//! Run with:
//! `cargo run -p naver-mcp-provider --example offline_basics`
//!
//! The network searches themselves are one-liners on the same client,
//! e.g. `client.web_search("제주도 여행", 1, 5).await?` inside an async
//! (tokio) context — see the crate-level documentation.

use naver_search::{
    build_page_url, build_search_url, parse_image_results, verticals, NaverSearchClient,
    SearchResult, WebItem, PROVIDER,
};

fn main() {
    // The client is cheap to construct and needs no configuration;
    // creating it performs no network I/O.
    let _client = NaverSearchClient::new();

    // Build a result value exactly as a search would return it, and
    // serialise it (result types derive serde::Serialize/Deserialize).
    let item = WebItem {
        rank: 1,
        title: "제주도 여행 코스 정리".to_string(),
        url: "https://example.com/jeju".to_string(),
        snippet: Some("2박 3일 일정과 맛집 목록".to_string()),
    };
    let result = SearchResult::new("web", "제주도 여행", 1, vec![item]);
    assert_eq!(result.provider, PROVIDER);
    println!("provider={} count={}", result.provider, result.count);
    println!("{}", serde_json::to_string_pretty(&result).unwrap());

    // Search URL generation, exactly as the client fetches it.
    println!(
        "integrated: {}",
        build_search_url("테스트", "integrated").unwrap()
    );
    // Only first pages have URLs — deeper pages are not expressible
    // as a URL (tab pages ignore page parameters; the chained tabs
    // page through the API each page itself issues).
    println!(
        "web page 2: {}",
        build_page_url("테스트", "web", 2).unwrap_err()
    );
    println!(
        "blog page 2: {}",
        build_page_url("테스트", "blog", 2).unwrap_err()
    );

    // Parse the embedded items of a saved image tab page.
    let saved_html = r#"<script>var imageSearchTabData = { content: { items: [
        {"type":"image","title":"고양이","originalUrl":"http://example.com/cat.jpg",
         "thumb":"https://search.pstatic.net/common/?src=http%3A%2F%2Fexample.com%2Fcat.jpg&type=f54_54"}
    ] } };</script>"#;
    let items = parse_image_results(saved_html, 10).expect("items should parse");
    println!(
        "image item: {} -> {}",
        items[0].title.as_deref().unwrap(),
        items[0].url
    );

    // Machine-readable list of the searchable verticals.
    let v = verticals();
    let ids: Vec<&str> = v["verticals"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["id"].as_str())
        .collect();
    println!("verticals: {}", ids.join(", "));
}
