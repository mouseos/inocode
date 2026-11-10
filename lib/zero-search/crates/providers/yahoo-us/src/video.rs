//! Yahoo (US) video search (`https://video.search.yahoo.com/search/video`).
//!
//! Results are server-rendered tiles (`ul.video-results__list > li.tile`).
//! The real watch URL is the tile link's `data-referenceurl` attribute —
//! the `href` itself is a Yahoo preview page. Totals live in the page's
//! `resultsMeta` JSON blob, not in the visible markup.

use anyhow::{anyhow, Result};
use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::{SearchResult, YahooUsSearchClient};

const REFERER: &str = "https://www.yahoo.com/";
/// Yahoo returns 60 video tiles per page and paginates `b` by that step
/// (verified live: `b=61` reports `currentOffset: "60"`).
pub const PER_PAGE: i64 = 60;

static TILE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("ul.video-results__list > li.tile").unwrap());
static LINK_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.media-tile").unwrap());
static IMG_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img.tile-image").unwrap());
static TIME_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("p.time").unwrap());
static DOMAIN_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".tile-domain").unwrap());
static DESC_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".tile-description").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".tile-title").unwrap());

static TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""resultsMeta":\{"totalResults":"(\d+)""#).unwrap());
static NEXT_OFFSET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""nextOffset":"(\d+)""#).unwrap());

/// One video result: title, watch page URL and playback metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embed_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

impl YahooUsSearchClient {
    /// Search videos. `page` is 1-based; each page holds [`PER_PAGE`]
    /// tiles. `total_hits` comes from the page's `resultsMeta` blob.
    pub async fn video_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<VideoItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);

        let url = crate::build_search_url(query, "video", page)?;
        let html = self.get_html(&url, REFERER).await?;
        if !html.contains("video-results") && parse_video_total(&html).is_none() {
            return Err(anyhow!(
                "video results container not found (Yahoo page layout may have changed)"
            ));
        }

        let base_rank = (page - 1) * PER_PAGE;
        let mut items = parse_video_results(&html, limit);
        for item in &mut items {
            item.rank += base_rank;
        }
        let total = parse_video_total(&html);
        let has_next = match (parse_video_next_offset(&html), total) {
            (Some(next), Some(total)) => next > 0 && next < total,
            (Some(next), None) => next > 0,
            _ => false,
        };

        Ok(SearchResult::new("video", query, page, items)
            .with_total(total)
            .with_next_page(has_next.then_some(page + 1))
            .with_related(parse_related_queries(&html)))
    }
}

/// Parse video tiles out of a saved video SERP page. Offline.
pub fn parse_video_results(html: &str, limit: usize) -> Vec<VideoItem> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for tile in doc.select(&TILE_SEL) {
        if items.len() >= limit {
            break;
        }
        let Some(link) = tile.select(&LINK_SEL).next() else {
            continue;
        };
        let Some(url) = link
            .value()
            .attr("data-referenceurl")
            .map(decode_entities)
            .filter(|u| u.starts_with("http"))
        else {
            continue;
        };
        let img = tile.select(&IMG_SEL).next();
        let title = tile
            .select(&TITLE_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|t| !t.is_empty())
            .or_else(|| {
                img.and_then(|el| el.value().attr("alt").map(decode_entities))
                    .filter(|t| !t.is_empty())
            })
            .unwrap_or_else(|| url.clone());
        let thumbnail = img
            .and_then(|el| el.value().attr("src"))
            .map(decode_entities)
            .filter(|s| !s.is_empty());
        let embed_url = link
            .value()
            .attr("data-origurl")
            .map(decode_entities)
            .filter(|s| !s.is_empty());
        let duration = tile
            .select(&TIME_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        let source = tile
            .select(&DOMAIN_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        let description = tile
            .select(&DESC_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        items.push(VideoItem {
            rank: items.len() as i64 + 1,
            title,
            url,
            embed_url,
            duration,
            source,
            description,
            thumbnail,
        });
    }
    items
}

/// Total result count from the page's `resultsMeta` blob, if present.
pub fn parse_video_total(html: &str) -> Option<i64> {
    TOTAL_RE.captures(html).and_then(|c| c[1].parse().ok())
}

/// Offset of the next page from `resultsMeta` (`0` when there is none).
pub fn parse_video_next_offset(html: &str) -> Option<i64> {
    NEXT_OFFSET_RE
        .captures(html)
        .and_then(|c| c[1].parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!("../tests/fixtures/fixture-video.html");

    #[test]
    fn parses_all_tiles_from_fixture() {
        let items = parse_video_results(PAGE, PER_PAGE as usize);
        assert_eq!(items.len(), 60, "fixture page should yield 60 tiles");
        for item in &items {
            assert!(item.url.starts_with("http"), "bad url: {}", item.url);
            assert!(
                !item.url.contains("video.search.yahoo.com"),
                "preview URL leaked instead of reference URL: {}",
                item.url
            );
            assert!(!item.title.is_empty());
        }
    }

    #[test]
    fn first_tile_fields_are_populated() {
        let items = parse_video_results(PAGE, PER_PAGE as usize);
        let first = &items[0];
        assert_eq!(first.url, "https://www.youtube.com/watch?v=hnZI5uUSdEo");
        assert_eq!(first.duration.as_deref(), Some("01:57"));
        assert_eq!(first.source.as_deref(), Some("YouTube"));
        assert!(first.thumbnail.is_some());
        assert!(first.embed_url.is_some());
    }

    #[test]
    fn totals_come_from_results_meta() {
        assert_eq!(parse_video_total(PAGE), Some(70));
        assert_eq!(parse_video_next_offset(PAGE), Some(60));
        assert_eq!(parse_video_total("<html></html>"), None);
    }

    #[test]
    fn limit_is_respected() {
        assert_eq!(parse_video_results(PAGE, 5).len(), 5);
    }
}
