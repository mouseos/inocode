//! Offline basics for using `yahoo_us_search` as a library — no network.
//!
//! Shows the parts of the public API that work without contacting Yahoo:
//! building result values, serialising them, building search URLs and
//! listing the verticals.
//!
//! Run with:
//! `cargo run -p yahoo-us-mcp-provider --example offline_basics`
//!
//! The network searches themselves are one-liners on the same client, e.g.
//! `client.web_search("apple pie recipe", 1, 5).await?` inside an async
//! (tokio) context — see the crate-level documentation.

use yahoo_us_search::{
    build_search_url, urlencode, verticals, SearchResult, WebItem, YahooUsSearchClient, PROVIDER,
};

fn main() {
    // The client is cheap to construct and needs no configuration; creating
    // it performs no network I/O.
    let _client = YahooUsSearchClient::new();

    // Build a result value exactly as a search would return it, and
    // serialise it (result types derive serde::Serialize/Deserialize).
    let item = WebItem {
        rank: 1,
        title: "Apple Pie Recipe".to_string(),
        url: "https://example.com/apple-pie".to_string(),
        description: Some("A classic double-crust apple pie".to_string()),
        site: Some("example.com".to_string()),
    };
    let result =
        SearchResult::new("web", "apple pie recipe", 1, vec![item]).with_next_page(Some(2));
    assert_eq!(result.provider, PROVIDER);
    println!("provider={} count={}", result.provider, result.count);
    println!("{}", serde_json::to_string_pretty(&result).unwrap());

    // Search URLs for every vertical, built offline.
    for vertical in ["web", "video", "news", "suggest"] {
        println!(
            "{vertical}: {}",
            build_search_url("apple pie", vertical, 1).unwrap()
        );
    }

    // Query-string encoding, as used for every search URL.
    println!("urlencode: {}", urlencode("apple pie & more"));

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
