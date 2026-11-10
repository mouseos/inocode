//! Offline example: build a first-page Baidu search URL.
//!
//! No network access is performed; this only generates the URL
//! string (APK `bq0.m` reproduction).
//!
//! Usage:
//!
//! ```sh
//! cargo run --example build_search_url
//! cargo run --example build_search_url -- "札幌 天気"
//! cargo run --example build_search_url -- weather 1700000000000
//! ```
//!
//! The optional second argument pins `now_ms` (epoch milliseconds)
//! so the output — including the `ant_ct` value — is reproducible.
//! Omit it to use the current time, as production callers do via
//! `build_initial_search_url(query, None)`.

use baidu_search::build_initial_search_url;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let query = args.get(1).map(|s| s.as_str()).unwrap_or("天气");
    let now_ms: Option<i64> = args.get(2).and_then(|s| s.parse::<i64>().ok());

    // Crate-root import is the stable form. The same function is
    // also reachable as `baidu_search::search_url::build_initial_search_url`.
    let url = build_initial_search_url(query, now_ms);

    println!("query: {query}");
    println!("now_ms: {now_ms:?}");
    println!("url: {url}");
    println!("user_agent: {}", baidu_search::search_url::user_agent());

    assert!(url.starts_with("https://m.baidu.com/s?"));
    assert!(url.contains("ant_ct="));
    assert!(url.contains("t_samp="));
}
