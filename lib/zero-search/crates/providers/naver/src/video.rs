//! Naver video tab (`ssc=tab.m_video.all&where=m_video`).
//!
//! Every playable result carries a trigger anchor with `data-api`
//! (the in-page player endpoint), `data-source` (the hosting platform)
//! and the watch URL as `href`; its own text is the play duration. The
//! rest of the card — title anchor (same href), channel link, relative
//! time, view count, thumbnail — is recovered from the smallest ancestor
//! that contains exactly one video. The tab serves one batch per page
//! load, so page ≥ 2 is an error.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{NaverSearchClient, SearchResult};

static TRIGGER_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[data-api]").unwrap());
static A_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").unwrap());
static IMG_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());

/// One video result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    /// Hosting platform as Naver labels it (e.g. `youtube`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// Upload time as Naver renders it (relative, e.g. `8시간 전`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    /// View count phrase as rendered (e.g. `조회수 449`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

/// Parse a saved Naver video tab page.
pub fn parse_video_results(html: &str, limit: usize) -> Result<Vec<VideoItem>> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for trigger in doc.select(&TRIGGER_SEL) {
        if items.len() >= limit {
            break;
        }
        let Some(href) = trigger.value().attr("href") else {
            continue;
        };
        if href.is_empty() || href.starts_with('#') || seen.contains(&href.to_string()) {
            continue;
        }
        seen.push(href.to_string());
        let url = resolve_url(href, "https://m.search.naver.com");
        let card = card_for_unique(trigger, &TRIGGER_SEL, "href", 10);

        let mut title: Option<String> = None;
        let mut channel: Option<String> = None;
        for a in card.select(&A_SEL) {
            let text = element_text(a);
            if text.is_empty() {
                continue;
            }
            let a_href = a.value().attr("href").unwrap_or("");
            if a_href == href {
                if title.is_none() && !is_duration(&text) {
                    title = Some(text);
                }
            } else if channel.is_none()
                && !a_href.starts_with('#')
                && !a_href.contains("keep.naver.com")
                && text != "Keep에 저장"
                && text != "Keep에 바로가기"
            {
                channel = Some(text);
            }
        }
        let Some(title) = title else { continue };
        let card_text = element_text(card);
        let duration = {
            let own = element_text(trigger);
            if is_duration(&own) {
                Some(own)
            } else {
                card_text
                    .split_whitespace()
                    .find(|t| is_duration(t))
                    .map(str::to_string)
            }
        };
        let thumbnail = card.select(&IMG_SEL).next().and_then(|img| {
            img.value()
                .attr("src")
                .or_else(|| img.value().attr("data-src"))
                .map(str::to_string)
        });
        items.push(VideoItem {
            rank: items.len() as i64 + 1,
            title,
            url,
            source: trigger
                .value()
                .attr("data-source")
                .map(str::to_string)
                .filter(|s| !s.is_empty()),
            channel,
            duration,
            published: find_time(&card_text),
            views: find_views(&card_text),
            thumbnail,
        });
    }
    if items.is_empty() {
        return Err(layout_error("video"));
    }
    Ok(items)
}

impl NaverSearchClient {
    /// Search the video tab. Page 1 is the tab page's batch; deeper
    /// pages follow the page-issued more-API chain (see `more`), up
    /// to [`MAX_PAGE`](crate::MAX_PAGE). `limit` is clamped to 48.
    pub async fn video_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<VideoItem>> {
        let page = page.max(1);
        if page > crate::more::MAX_PAGE {
            return Err(crate::common::unsupported_page("video", page));
        }
        let limit = clamp_limit(limit, 48);
        let url = build_page_url(query, "video", 1)?;
        let html = self.get_html(&url, SEARCH_REFERER).await?;
        let items = crate::more::chained_items(
            self,
            "video",
            &html,
            page,
            |h| parse_video_results(h, usize::MAX),
            |payload| match payload {
                crate::more::MorePayload::Html(fragment) => {
                    parse_video_results(&fragment, usize::MAX)
                }
                _ => Err(layout_error("video")),
            },
            |item, rank| item.rank = rank,
        )
        .await?;
        let items: Vec<VideoItem> = items.into_iter().take(limit).collect();
        Ok(SearchResult::new("video", query, page, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_card() {
        let html = r#"<html><body><div class="card">
            <a href="https://m.youtube.com/watch?v=abc" data-api="https://s.search.naver.com/p/video/48/x" data-source="youtube"><img src="https://search.pstatic.net/t.jpg"><span>08:03</span></a>
            <a href="https://m.youtube.com/watch?v=abc">에이징커브 테스트</a>
            <a href="https://m.youtube.com/channel/UC1">악어 유튜브</a>
            <span>8시간 전</span><span>조회수 449</span>
            </div></body></html>"#;
        let items = parse_video_results(html, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "에이징커브 테스트");
        assert_eq!(items[0].source.as_deref(), Some("youtube"));
        assert_eq!(items[0].channel.as_deref(), Some("악어 유튜브"));
        assert_eq!(items[0].duration.as_deref(), Some("08:03"));
        assert_eq!(items[0].published.as_deref(), Some("8시간 전"));
        assert_eq!(items[0].views.as_deref(), Some("조회수 449"));
    }

    #[test]
    fn parse_empty_shell_is_a_layout_error() {
        assert!(parse_video_results("<html><body></body></html>", 10).is_err());
    }
}
