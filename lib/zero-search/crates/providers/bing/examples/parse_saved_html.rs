//! Parse saved Bing result payloads without any network access.
//! Web results are the v7 search API's JSON; image results are saved
//! page markup.
//!
//! Run with: `cargo run -p bing-mcp-provider --example parse_saved_html`

use bing_search::{parse_image_results, parse_suggest_json, parse_web_results};

fn main() -> anyhow::Result<()> {
    // A minimal v7 search API response, shaped like the real payload.
    // URLs are direct destinations — no click-tracking redirect.
    let web_json: serde_json::Value = serde_json::json!({
        "webPages": {
            "totalEstimatedMatches": 42,
            "value": [{
                "name": "Example result",
                "url": "https://example.com/page",
                "displayUrl": "https://example.com › page",
                "snippet": "An example snippet."
            }]
        },
        "relatedSearches": {"value": [{"text": "example query"}]}
    });
    let page = parse_web_results(&web_json, 10)?;
    println!("web: {} item(s)", page.items.len());
    for item in &page.items {
        println!("  #{} {} -> {}", item.rank, item.title, item.url);
    }
    println!("  related: {}", page.related_queries.join(", "));

    // One image unit: the `m` attribute carries the metadata JSON.
    let image_html = r##"<a class="iusc" href="/images/search?view=detailV2" m="{&quot;murl&quot;:&quot;https://example.com/cat.jpg&quot;,&quot;turl&quot;:&quot;https://ts1.mm.bing.net/th?id=OIP.x&amp;pid=15.1&quot;,&quot;purl&quot;:&quot;https://example.com/cats&quot;,&quot;t&quot;:&quot;A cat&quot;}">x</a>"##;
    let images = parse_image_results(image_html, 1, 12)?;
    println!("image: {} item(s)", images.items.len());
    for item in &images.items {
        println!("  {} -> {}", item.title, item.url);
    }

    // Suggestions API JSON.
    let json: serde_json::Value = serde_json::json!({
        "suggestionGroups": [{
            "name": "Web",
            "searchSuggestions": [
                { "displayText": "テスト", "query": "テスト", "url": "https://www.bing.com/search?q=%E3%83%86%E3%82%B9%E3%83%88" },
                { "displayText": "テストセンター", "query": "テストセンター", "url": "https://www.bing.com/search?q=x" }
            ]
        }]
    });
    let suggestions = parse_suggest_json(&json);
    println!("suggest: {} candidate(s)", suggestions.len());
    for s in &suggestions {
        println!("  {}", s.keyword);
    }
    Ok(())
}
