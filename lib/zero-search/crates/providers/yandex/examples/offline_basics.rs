//! Offline basics for using `yandex_search` as a library — no network.
//!
//! Shows the parts of the public API that work without contacting Yandex:
//! building result values, serialising them, listing the verticals and
//! constructing search URLs.
//!
//! Run with:
//! `cargo run -p yandex-mcp-provider --example offline_basics`
//!
//! The network searches themselves are one-liners on the same client, e.g.
//! `client.web_search("Москва погода", 1, 5).await?` inside an async
//! (tokio) context — see the crate-level documentation.

use yandex_search::{
    build_search_url, clean_text, urlencode, verticals, SearchResult, WebItem, YandexSearchClient,
    PROVIDER,
};

fn main() {
    // The client is cheap to construct and needs no configuration; creating
    // it performs no network I/O.
    let _client = YandexSearchClient::new();

    // Build a result value exactly as a search would return it, and
    // serialise it (result types derive serde::Serialize/Deserialize).
    let item = WebItem {
        rank: 1,
        title: clean_text("<b>Тест</b> на IQ"),
        url: "https://example.com/tests/1".to_string(),
        description: Some("Проверь свой интеллект".to_string()),
        site: Some("example.com › tests".to_string()),
    };
    let result = SearchResult::new("web", "тест", 1, vec![item]).with_next_page(Some(2));
    assert_eq!(result.provider, PROVIDER);
    println!("provider={} count={}", result.provider, result.count);
    println!("{}", serde_json::to_string_pretty(&result).unwrap());

    // Search URLs for every vertical, built offline.
    for vertical in ["web", "image", "video", "suggest"] {
        println!(
            "{vertical}: {}",
            build_search_url("тест на iq", vertical).unwrap()
        );
    }
    // News has no URL builder: it moved to dzen.ru behind Yandex SSO.
    println!("news: {}", build_search_url("тест", "news").unwrap_err());

    // Query-string encoding, as used for every search URL.
    println!("urlencode: {}", urlencode("тест на iq"));

    // Machine-readable list of the searchable verticals.
    let v = verticals();
    let ids: Vec<String> = v["verticals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            let mark = if entry["supported"] == true {
                ""
            } else {
                " (unsupported)"
            };
            format!("{}{mark}", entry["id"].as_str().unwrap())
        })
        .collect();
    println!("verticals: {}", ids.join(", "));
}
