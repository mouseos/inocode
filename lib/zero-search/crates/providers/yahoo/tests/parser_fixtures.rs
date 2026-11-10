//! Offline parser tests against real result pages captured live
//! 2026-11-08 (audit W-10): one page per vertical, parsed through
//! the same `parse_*_page` functions the client methods call.

use yahoo_search::{
    parse_chiebukuro_page, parse_image_page, parse_news_page, parse_realtime_page,
    parse_video_page, parse_web_page,
};

const WEB_HTML: &str = include_str!("fixtures/fixture-web.html");
const IMAGE_HTML: &str = include_str!("fixtures/fixture-image.html");
const VIDEO_HTML: &str = include_str!("fixtures/fixture-video.html");
const NEWS_HTML: &str = include_str!("fixtures/fixture-news.html");
const REALTIME_HTML: &str = include_str!("fixtures/fixture-realtime.html");
const CHIEBUKURO_HTML: &str = include_str!("fixtures/fixture-chiebukuro.html");

#[test]
fn web_fixture_parses_results() {
    let r = parse_web_page(WEB_HTML, "天気", 1, 10).unwrap();
    assert!(!r.items.is_empty(), "web items empty");
    assert!(r.items.iter().all(|i| !i.title.is_empty()));
    println!("web items={} total={:?}", r.items.len(), r.total_hits);
}

#[test]
fn image_fixture_parses_results() {
    let r = parse_image_page(IMAGE_HTML, "猫", 1, 20).unwrap();
    assert!(!r.items.is_empty(), "image items empty");
    println!("image items={}", r.items.len());
}

#[test]
fn video_fixture_parses_results() {
    let r = parse_video_page(VIDEO_HTML, "猫", 1, 20).unwrap();
    assert!(!r.items.is_empty(), "video items empty");
    println!("video items={}", r.items.len());
}

#[test]
fn news_fixture_parses_results() {
    let r = parse_news_page(NEWS_HTML, "天気", 20).unwrap();
    assert!(!r.items.is_empty(), "news items empty");
    println!("news items={}", r.items.len());
}

#[test]
fn realtime_fixture_parses_results() {
    let r = parse_realtime_page(REALTIME_HTML, "地震", 1, 20).unwrap();
    assert!(!r.items.is_empty(), "realtime items empty");
    println!("realtime items={}", r.items.len());
}

#[test]
fn chiebukuro_fixture_parses_results() {
    let r = parse_chiebukuro_page(CHIEBUKURO_HTML, "保険", 1, 10).unwrap();
    assert!(!r.items.is_empty(), "chiebukuro items empty");
    println!("chiebukuro items={}", r.items.len());
}
