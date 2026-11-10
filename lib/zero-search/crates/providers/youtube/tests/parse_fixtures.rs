//! Offline parser tests against real InnerTube responses captured
//! during live verification (guest session, ANDROID client,
//! 2026-11-07). Fixtures are unmodified server responses.

use serde_json::Value;
use youtube_search::next::{parse_comments_response, parse_next_response};
use youtube_search::parse::SearchItem;
use youtube_search::player::parse_player_response;
use youtube_search::search::parse_search_response;

fn load(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let raw = std::fs::read_to_string(&path).expect("fixture readable");
    serde_json::from_str(&raw).expect("fixture is valid JSON")
}

#[test]
fn search_fixture_parses_compact_renderers() {
    let page = parse_search_response(&load("search.json"));
    assert!(page.items.len() >= 15, "got {} items", page.items.len());
    let first = &page.items[0];
    assert_eq!(first.video_id, "5C_HPTJg5ek");
    assert_eq!(first.title, "Rust in 100 Seconds");
    assert_eq!(first.channel_name, "Fireship");
    assert!(first.view_count.unwrap_or(0) > 2_000_000);
    assert_eq!(first.duration_secs, Some(149));
    assert!(page.estimated_results.unwrap_or(0) > 1_000_000);
    assert!(
        page.next_continuation.is_some(),
        "search continuation present"
    );
    assert!(page
        .items
        .iter()
        .all(|i| !i.video_id.is_empty() && !i.title.is_empty()));
}

#[test]
fn search_fixture_extracts_typed_results() {
    let page = parse_search_response(&load("search.json"));
    // Videos keep flowing into `items`; `results` carries every kind
    // in document order.
    let kinds: Vec<&str> = page
        .results
        .iter()
        .map(|r| match r {
            SearchItem::Video(_) => "video",
            SearchItem::Short(_) => "short",
            SearchItem::Channel(_) => "channel",
            SearchItem::Playlist(_) => "playlist",
        })
        .collect();
    assert_eq!(
        kinds.iter().filter(|k| **k == "video").count(),
        page.items.len()
    );
    // The fixture page contains exactly one playlist.
    let playlists: Vec<_> = page
        .results
        .iter()
        .filter_map(|r| match r {
            SearchItem::Playlist(p) => Some(p),
            _ => None,
        })
        .collect();
    assert_eq!(playlists.len(), 1);
    let pl = playlists[0];
    assert_eq!(pl.playlist_id, "PLzMcBGfZo4-nyLTlSRBvo0zjSnCnqjHYQ");
    assert_eq!(pl.title, "Rust Programming Tutorial");
    assert_eq!(pl.channel_name, "Tech With Tim");
    assert_eq!(pl.channel_id.as_deref(), Some("UC4JX40jDee_tINbkjycV4Sg"));
    assert_eq!(pl.video_count, Some(9));
    // …and one Shorts shelf with four Shorts.
    let shorts: Vec<_> = page
        .results
        .iter()
        .filter_map(|r| match r {
            SearchItem::Short(v) => Some(v),
            _ => None,
        })
        .collect();
    assert_eq!(shorts.len(), 4);
    let first = shorts[0];
    assert_eq!(first.video_id, "w9VvbeOoXsA");
    assert_eq!(
        first.title,
        "Why Beginners Should AVOID Rust #programming #coding #software"
    );
    assert!(first.view_count.unwrap_or(0) > 1_000_000);
    assert_eq!(first.url, "https://www.youtube.com/shorts/w9VvbeOoXsA");
    // Document order: the playlist sits between videos, the Shorts
    // shelf comes after them.
    let pl_pos = kinds.iter().position(|k| *k == "playlist").unwrap();
    let short_pos = kinds.iter().position(|k| *k == "short").unwrap();
    assert!(pl_pos < short_pos);
    // Envelope tagging.
    let env = page.to_value("rust programming");
    let results = env["results"].as_array().unwrap();
    assert!(results.iter().all(|r| r.get("kind").is_some()));
}

#[test]
fn next_fixture_extracts_like_count_and_related_continuation() {
    let wn = parse_next_response(&load("next.json"), "5C_HPTJg5ek");
    // likeCountEntity: the guest-visible ("indifferent") figure.
    assert_eq!(wn.like_count, Some(86_181));
    assert_eq!(wn.like_count_text.as_deref(), Some("86K"));
    assert!(
        wn.related_continuation.is_some(),
        "related 'Show more' continuation extracted"
    );
}

#[test]
fn next_fixture_parses_lockup_related_and_comments_token() {
    let wn = parse_next_response(&load("next.json"), "5C_HPTJg5ek");
    assert_eq!(wn.related.len(), 20, "related lockups parsed");
    assert!(wn
        .related
        .iter()
        .any(|i| i.video_id == "0y6RKiIk6cs" && i.title.contains("Rust for Dummies")));
    let sample = wn
        .related
        .iter()
        .find(|i| i.video_id == "0y6RKiIk6cs")
        .expect("sample present");
    assert_eq!(sample.channel_name, "Let's Get Rusty");
    assert_eq!(sample.duration_secs, Some(750));
    assert!(sample.view_count.unwrap_or(0) > 400_000);
    assert!(
        wn.comments_continuation.is_some(),
        "comments reload token extracted from the comment panel"
    );
}

#[test]
fn channel_search_fixture_parses_compact_channel_models() {
    let page = parse_search_response(&load("search_channel.json"));
    let channels: Vec<_> = page
        .results
        .iter()
        .filter_map(|r| match r {
            SearchItem::Channel(c) => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(channels.len(), 20, "one page of channel results");
    let first = channels[0];
    assert_eq!(first.channel_id, "UCsBjURrPoezykLs9EqgamOA");
    assert_eq!(first.title, "Fireship");
    assert_eq!(first.handle.as_deref(), Some("@Fireship"));
    assert_eq!(first.url, "https://www.youtube.com/@Fireship");
    assert_eq!(
        first.subscriber_count_text.as_deref(),
        Some("4.33M subscribers")
    );
    assert_eq!(first.video_count_text.as_deref(), Some("861 videos"));
    assert!(first.thumbnail.is_some());
    // A channel-type search page contains no video items.
    assert!(page.items.is_empty());
}

#[test]
fn related_page2_fixture_parses_continuation_response() {
    let page =
        youtube_search::next::parse_related_continuation_response(&load("related_page2.json"));
    assert_eq!(page.items.len(), 20, "second page of related videos");
    assert!(page
        .items
        .iter()
        .all(|i| !i.video_id.is_empty() && !i.title.is_empty()));
    assert!(
        page.next_continuation.is_some(),
        "a third related page is offered"
    );
}

#[test]
fn updated_metadata_fixture_parses_entities() {
    let m = youtube_search::metadata::parse_updated_metadata_response(
        &load("updated_metadata.json"),
        "5C_HPTJg5ek",
    );
    assert_eq!(m.view_count, Some(2_651_215));
    assert_eq!(m.like_count, Some(86_194));
    assert_eq!(m.like_count_text.as_deref(), Some("86K"));
    assert_eq!(m.upload_time_text.as_deref(), Some("5y ago"));
    assert_eq!(m.upload_time_a11y_text.as_deref(), Some("5 years ago"));
}

#[test]
fn timedtext_fixture_parses_srv3_cues() {
    let path = format!(
        "{}/tests/fixtures/timedtext_en.xml",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).expect("fixture readable");
    let cues = youtube_search::captions::parse_timedtext_srv3(&raw).expect("srv3 parses");
    assert_eq!(cues.len(), 37);
    assert_eq!(cues[0].start_ms, 160);
    assert_eq!(cues[0].duration_ms, 6119);
    assert!(cues[0]
        .text
        .contains("rust a memory safe compiled programming"));
}

#[test]
fn comments_fixture_parses_entities_in_thread_order() {
    let page = parse_comments_response(&load("comments.json"));
    assert_eq!(page.comments.len(), 20, "one page of comments");
    let first = &page.comments[0];
    assert_eq!(first.author, "@sznio");
    assert!(first.text.contains("Don't call rustc directly"));
    assert_eq!(first.comment_id, "UgwbOfn7CaLOqLDbF3t4AaABAg");
    assert!(page
        .comments
        .iter()
        .all(|c| !c.comment_id.is_empty() && !c.text.is_empty()));
    assert!(
        page.next_continuation.is_some(),
        "comments page-2 token present"
    );
}

#[test]
fn player_fixture_parses_core_details() {
    let d = parse_player_response(&load("player.json")).expect("playable");
    assert_eq!(d.video_id, "5C_HPTJg5ek");
    assert_eq!(d.title, "Rust in 100 Seconds");
    assert_eq!(d.channel_name, "Fireship");
    assert_eq!(d.length_secs, Some(149));
    assert!(d.view_count.unwrap_or(0) > 2_000_000);
    assert!(d.stream_format_count > 0, "streaming formats parsed");
    assert!(d.has_caption_tracks);
    // One caption track on this video: English ASR.
    assert_eq!(d.caption_tracks.len(), 1);
    let track = &d.caption_tracks[0];
    assert_eq!(track.language_code, "en");
    assert_eq!(track.kind.as_deref(), Some("asr"));
    assert!(track.base_url.contains("api/timedtext"));
    // The ANDROID player response has no microformat section, so
    // publish date / like count are legitimately absent here; the
    // like count is recovered from the watch-next response instead
    // (see next_fixture_extracts_like_count_and_related_continuation).
    assert!(d.published_date.is_none());
    assert!(d.like_count.is_none());
}
