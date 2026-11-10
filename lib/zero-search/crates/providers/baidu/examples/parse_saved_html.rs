//! Offline example: parse a saved Baidu search-result HTML page.
//!
//! No network access is performed.
//!
//! Usage:
//!
//! ```sh
//! cargo run --example parse_saved_html
//! cargo run --example parse_saved_html -- path/to/saved.html [query]
//! ```
//!
//! With no arguments a small embedded HTML page is parsed, so the
//! example works anywhere. With a file argument, that file is read
//! and parsed instead — for example one of the measured fixtures:
//!
//! ```sh
//! cargo run --example parse_saved_html -- \
//!     crates/providers/baidu/tests/fixtures/tianqi_p1.html 天气
//! ```

use baidu_search::parse_search_results;
use baidu_search::parse_search_results_bytes;

const EMBEDDED_HTML: &str = include_str!("../tests/fixtures/synthetic_ad.html");
const EMBEDDED_QUERY: &str = "test";
const EMBEDDED_URL: &str = "https://m.baidu.com/s?word=test";

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let page = if let Some(path) = args.get(1) {
        let query = args.get(2).map(|s| s.as_str()).unwrap_or(EMBEDDED_QUERY);
        let bytes = std::fs::read(path).unwrap_or_else(|e| {
            eprintln!("failed to read {path}: {e}");
            std::process::exit(1);
        });
        println!(
            "parsing file: {path} ({} bytes), query: {query:?}",
            bytes.len()
        );
        parse_search_results_bytes(&bytes, Some(query), Some(EMBEDDED_URL))
    } else {
        println!("parsing embedded fixture (synthetic_ad.html), query: {EMBEDDED_QUERY:?}");
        parse_search_results(EMBEDDED_HTML, Some(EMBEDDED_QUERY), Some(EMBEDDED_URL))
    };

    println!("query:            {:?}", page.query);
    println!("results (organic): {}", page.results.len());
    println!("ads:               {}", page.ads.len());
    println!("cards:             {}", page.cards.len());
    println!("related searches:  {}", page.related_searches.len());
    println!("unknown elements:  {}", page.unknown_elements.len());
    for result in &page.results {
        println!(
            "  result #{}: {:?} -> {:?}",
            result.organic_rank.unwrap_or_default(),
            result.title,
            result.url
        );
    }
    for ad in &page.ads {
        println!("  ad: {:?} -> {:?}", ad.title, ad.url);
    }
    // The full structured JSON (same shape as the Python parser):
    println!("json: {}", page.to_value());
}
