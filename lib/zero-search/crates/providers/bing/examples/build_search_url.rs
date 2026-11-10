//! Build Bing search URLs for every vertical without any network access.
//! This mirrors the `bing_build_search_url` MCP tool.
//!
//! Run with: `cargo run -p bing-mcp-provider --example build_search_url`

use bing_search::{build_page_url, build_search_url, suggest_url};

fn main() -> anyhow::Result<()> {
    for vertical in ["web", "image", "video", "news"] {
        let url = build_search_url("テスト", vertical, "ja-JP", "ja")?;
        println!("{vertical:6} {url}");
    }
    // Paging: web advances first by 10, image by 12.
    println!(
        "web p2 {}",
        build_page_url("テスト", "web", 2, "ja-JP", "ja")?
    );
    println!(
        "img p2 {}",
        build_page_url("テスト", "image", 2, "ja-JP", "ja")?
    );
    println!("suggest {}", suggest_url("テスト", "web", "ja-JP", "ja")?);
    Ok(())
}
