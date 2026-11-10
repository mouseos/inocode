//! Parse saved Yandex touch pages — no network.
//!
//! The parsers the live searches use are public, so a page saved from the
//! browser (or a test fixture) can be turned into the same item values the
//! client would return.
//!
//! Run with:
//! `cargo run -p yandex-mcp-provider --example parse_saved_pages -- <web|image|video|suggest> <file>`
//!
//! * `web`, `image`, `video` expect a saved HTML page;
//! * `suggest` expects a saved `suggest-ya.cgi` JSON response body.

use std::process::exit;

use yandex_search::{
    parse_image_results, parse_suggest_body, parse_video_results, parse_web_results,
};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!(
            "usage: parse_saved_pages <web|image|video|suggest> <file>\n\
             (file: saved HTML page, or a suggest JSON body for `suggest`)"
        );
        exit(2);
    }
    let (vertical, path) = (args[1].as_str(), args[2].as_str());
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        exit(1);
    });

    match vertical {
        "web" => {
            let items = parse_web_results(&text, usize::MAX);
            println!("{} web results", items.len());
            for item in items {
                println!("{}. {} — {}", item.rank, item.title, item.url);
            }
        }
        "image" => {
            let items = parse_image_results(&text, usize::MAX).unwrap_or_else(|e| {
                eprintln!("parse failed: {e}");
                exit(1);
            });
            println!("{} image results", items.len());
            for item in items {
                println!("{}. {} — {}", item.rank, item.title, item.original_url);
            }
        }
        "video" => {
            let items = parse_video_results(&text, usize::MAX);
            println!("{} video results", items.len());
            for item in items {
                println!(
                    "{}. {} — {}",
                    item.rank,
                    item.title,
                    item.url.as_deref().unwrap_or(&item.preview_url)
                );
            }
        }
        "suggest" => {
            let body: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|e| {
                eprintln!("invalid JSON: {e}");
                exit(1);
            });
            let result = parse_suggest_body(&body, "", usize::MAX);
            println!("{} suggestions", result.count);
            for s in result.suggestions {
                println!("{}", s.keyword);
            }
        }
        other => {
            eprintln!("unknown vertical: {other} (expected web, image, video or suggest)");
            exit(2);
        }
    }
}
