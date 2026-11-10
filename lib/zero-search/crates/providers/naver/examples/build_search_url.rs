//! Print the search URLs the client would fetch — no network.
//!
//! Only first pages have a URL: tab pages ignore page parameters, and
//! deeper batches of the image / news / video / blog tabs come from
//! the more-API chain each page itself issues (see the `more`
//! module), so `build_page_url` refuses page 2+ with an explicit
//! error instead of printing a URL that would repeat page 1.
//!
//! Run with:
//! `cargo run -p naver-mcp-provider --example build_search_url`

use naver_search::{build_page_url, build_search_url, suggest_url};

fn main() {
    let query = "제주도 여행";
    for vertical in ["integrated", "web", "image", "news", "video", "blog"] {
        println!(
            "{vertical:>10} p1: {}",
            build_search_url(query, vertical).unwrap()
        );
    }
    for vertical in ["web", "image", "news"] {
        match build_page_url(query, vertical, 2) {
            Ok(url) => println!("{vertical:>10} p2: {url}"),
            Err(e) => println!("{vertical:>10} p2: error: {e}"),
        }
    }
    println!("{:>10}   : {}", "suggest", suggest_url("제주"));
}
