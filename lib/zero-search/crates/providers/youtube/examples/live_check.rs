//! Live verification driver (not part of the offline test suite).
//!
//! Usage: cargo run --example live_check -- <query> [steps]
//! Runs, in order and with pauses: visitor bootstrap, search,
//! search continuation (page 2), player details for the first hit,
//! watch-next (related + comments entrance), one comments page.
//! Raw responses are saved under ../live/ relative to the crate root
//! when YT_SAVE_DIR is set. Stops at the first error.

use std::time::Duration;

use youtube_search::filters::{SearchFilters, SearchSort};
use youtube_search::YoutubeClient;

async fn pause() {
    tokio::time::sleep(Duration::from_secs(3)).await;
}

#[tokio::main]
async fn main() {
    let query = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "rust programming".to_string());
    let save_dir = std::env::var("YT_SAVE_DIR").ok();
    let client = YoutubeClient::new();

    macro_rules! step {
        ($name:expr, $ fut:expr) => {
            match $fut.await {
                Ok(v) => v,
                Err(e) => {
                    println!("STEP {} FAILED: {}", $name, e);
                    std::process::exit(1);
                }
            }
        };
    }

    let vd = step!("bootstrap", client.bootstrap_visitor());
    println!(
        "bootstrap: visitor_data={}",
        vd.as_deref().map(|_| "issued").unwrap_or("none")
    );
    pause().await;

    let page = step!(
        "search",
        client.search_videos(&query, &SearchFilters::default())
    );
    println!(
        "search: {} items, estimated={:?}, continuation={}",
        page.items.len(),
        page.estimated_results,
        page.next_continuation.is_some()
    );
    for item in page.items.iter().take(5) {
        println!(
            "  - {} | {} | {} | {:?} | {:?}",
            item.video_id, item.title, item.channel_name, item.view_count_text, item.duration_text
        );
    }
    let first_id = page.items.first().map(|i| i.video_id.clone());
    pause().await;

    if let Some(token) = &page.next_continuation {
        let page2 = step!("search_continuation", client.search_continuation(token));
        let overlap = page2
            .items
            .iter()
            .filter(|i| page.items.iter().any(|a| a.video_id == i.video_id))
            .count();
        println!(
            "search page2: {} items, overlap_with_page1={}",
            page2.items.len(),
            overlap
        );
        pause().await;
    }

    if let Some(vid) = &first_id {
        let details = step!("player", client.get_video(vid));
        println!(
            "player: {} | {} | views={:?} likes={:?} len={:?} published={:?} formats={} captions={}",
            details.video_id,
            details.title,
            details.view_count,
            details.like_count,
            details.length_secs,
            details.published_date,
            details.stream_format_count,
            details.has_caption_tracks
        );
        pause().await;

        let next = step!("next", client.get_watch_next(vid));
        println!(
            "next: {} related, comments_token={}",
            next.related.len(),
            next.comments_continuation.is_some()
        );
        for item in next.related.iter().take(3) {
            println!("  ~ {} | {}", item.video_id, item.title);
        }
        pause().await;

        if let Some(ctok) = &next.comments_continuation {
            let comments = step!("comments", client.get_comments(ctok));
            println!(
                "comments: {} items, next={}",
                comments.comments.len(),
                comments.next_continuation.is_some()
            );
            for c in comments.comments.iter().take(3) {
                let text: String = c.text.chars().take(60).collect();
                println!("  * {}: {}", c.author, text);
            }
            pause().await;
        }
    }

    // Sorted search (upload date) as the filter-encoder check.
    let sorted = step!(
        "search_sorted",
        client.search_videos(
            &query,
            &SearchFilters {
                sort: SearchSort::UploadDate,
                ..Default::default()
            },
        )
    );
    println!(
        "search_sorted(upload_date): {} items, first_published={:?}",
        sorted.items.len(),
        sorted.items.first().and_then(|i| i.published_time.clone())
    );

    let _ = save_dir;
    println!("ALL STEPS OK");
}
