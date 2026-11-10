//! Yandex video search (`https://yandex.ru/video/touch/search`).
//!
//! Two sources are combined, as the page itself combines them:
//! the rendered `.VideoSnippet` cards (document order, title, thumbnail,
//! on-card duration, host name, date) and the embedded JSON metadata map
//! (`"dups"` — every result and its duplicates on other hosts), keyed by the
//! video id in each card's preview URL, which carries the canonical watch
//! URL (`host.href`), the provider and the duration in seconds.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_search_url;
use crate::{SearchResult, YandexSearchClient};

static TITLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)VideoSnippet-Title[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap()
});
static POSTER_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"poster="([^"]+)""#).unwrap());
static DURATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"VideoThumb3Meta-Duration">(?:<[^>]+>)*([^<]+)"#).unwrap());
static HOST_TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"VideoHostExtended-Host" title="([^"]+)""#).unwrap());
static HOST_TEXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"VideoHostExtended-Host"[^>]*>([^<]+)"#).unwrap());
static DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)VideoSnippet-MetaPoint_kind_date">(.*?)</div>"#).unwrap());
static VIEWS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)VideoSnippetMetaPoint_views">(.*?)</div>"#).unwrap());
static A11Y_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<span class="A11yHidden">.*?</span>"#).unwrap());

/// One video result: title, canonical watch URL on the hosting service,
/// Yandex preview URL, duration, provider and publish date.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoItem {
    pub rank: i64,
    pub title: String,
    pub video_id: String,
    pub preview_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<String>,
}

/// The search URL the client requests for `page` (1-based; Yandex pages
/// from 0 with `p`, as on the web vertical).
pub fn video_search_url(query: &str, page: i64) -> String {
    let mut url = build_search_url(query, "video").expect("video URL always builds");
    if page > 1 {
        url.push_str(&format!("&p={}", page - 1));
    }
    url
}

/// Parse video results out of a saved touch video-search page.
pub fn parse_video_results(html: &str, limit: usize) -> Vec<VideoItem> {
    let dups = extract_json_after(html, "\"dups\":{").unwrap_or(Value::Null);
    let starts: Vec<usize> = html
        .match_indices("<div class=\"VideoSnippet2 VideoSnippet")
        .map(|(i, _)| i)
        .collect();
    let mut items = Vec::new();
    for (idx, &start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(html.len());
        if let Some(item) = parse_card(&html[start..end], &dups, items.len() as i64 + 1) {
            items.push(item);
            if items.len() >= limit {
                break;
            }
        }
    }
    items
}

fn parse_card(block: &str, dups: &Value, rank: i64) -> Option<VideoItem> {
    let caps = TITLE_RE.captures(block)?;
    let preview_href = decode_entities(caps.get(1)?.as_str());
    let title = clean_text(caps.get(2)?.as_str());
    if title.is_empty() {
        return None;
    }
    let video_id = preview_href
        .split("/preview/")
        .nth(1)
        .unwrap_or("")
        .split(['?', '/'])
        .next()
        .unwrap_or("")
        .to_string();
    let visible = A11Y_RE.replace_all(block, "");

    let mut duration = DURATION_RE
        .captures(&visible)
        .map(|c| clean_text(&c[1]))
        .filter(|s| !s.is_empty());
    let mut duration_seconds = None;
    let mut url = None;
    let mut provider = None;
    let mut provider_name = HOST_TITLE_RE
        .captures(&visible)
        .map(|c| clean_text(&c[1]))
        .or_else(|| HOST_TEXT_RE.captures(&visible).map(|c| clean_text(&c[1])));
    let mut published_date = DATE_RE
        .captures(&visible)
        .map(|c| clean_text(&c[1]))
        .filter(|s| !s.is_empty());

    if let Some(meta) = dups.get(&video_id) {
        let host = meta.get("host").cloned().unwrap_or(Value::Null);
        url = get_raw(&host, "href");
        provider = get_raw(&host, "providerName").or_else(|| get_raw(&host, "sourceHost"));
        if provider_name.is_none() {
            provider_name = get_str(&host, "title");
        }
        let dur = meta.get("duration").cloned().unwrap_or(Value::Null);
        duration_seconds = get_i64(&dur, "value");
        if duration.is_none() {
            duration = get_str(&dur, "text");
        }
        if published_date.is_none() {
            published_date = get_str(meta, "date");
        }
    }

    Some(VideoItem {
        rank,
        title,
        video_id,
        preview_url: absolutize(&preview_href),
        url,
        thumbnail_url: POSTER_RE
            .captures(&visible)
            .map(|c| absolutize(&decode_entities(&c[1]))),
        duration,
        duration_seconds,
        provider,
        provider_name,
        published_date,
        views: VIEWS_RE
            .captures(&visible)
            .map(|c| clean_text(&c[1]))
            .filter(|s| !s.is_empty()),
    })
}

/// Results on a full touch video page; a page that fills it has a
/// successor (the same rule the web and image verticals use).
const PER_PAGE: i64 = 20;

impl YandexSearchClient {
    /// Search videos. `page` is 1-based; `limit` is clamped to 20.
    /// Requires the session (established automatically from web page 1).
    pub async fn video_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<VideoItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);
        let query = query.trim();

        self.ensure_session(query).await?;
        let html = self.get_page(&video_search_url(query, page)).await?;
        let mut items = parse_video_results(&html, usize::MAX);
        let total_found = items.len();
        items.truncate(limit);
        let mut result = SearchResult::new("video", query, page, items);
        if total_found >= PER_PAGE as usize {
            result = result.with_next_page(Some(page + 1));
        }
        if page > crate::VERIFIED_MAX_PAGE {
            result = result.with_note(crate::BEYOND_VERIFIED_NOTE);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_url_uses_tabbar_form() {
        assert_eq!(
            video_search_url("test", 1),
            "https://yandex.ru/video/touch/search?text=test&source=tabbar&ui=webmobileapp.yandex"
        );
    }

    #[test]
    fn parse_card_merges_dom_and_json_metadata() {
        let card = r#"<div class="VideoSnippet2 VideoSnippet VideoSnippet_preset_serp"><a class="Link VideoThumb3 VideoSnippet-Thumb" href="/video/touch/preview/123?text=test"><video poster="//avatars.mds.yandex.net/get-vthumb/1/abc/564x318_1"></video><span class="VideoThumb3Meta-Duration"><span aria-label="длительность"></span>18:00</span></a><div class="VideoSnippet-Content"><a title="T" class="Link VideoSnippet-Title" href="/video/touch/preview/123?text=test">Some <b>Title</b></a><div class="VideoSnippet-Meta"><div class="VideoHostExtended-Host" title="VK Видео">VK Видео</div></div><div class="VideoSnippet-Meta"><div class="VideoSnippetMetaPoint VideoSnippet-MetaPoint_kind_date"><span class="A11yHidden">дата публикации</span>13 авг 2024</div></div></div>"#;
        let dups = serde_json::json!({
            "123": {
                "host": {"href": "http://vk.com/video1_2", "providerName": "vk.com", "title": "VK Видео"},
                "duration": {"value": 1080, "text": "18:00"},
                "date": "13 авг 2024"
            }
        });
        let item = parse_card(card, &dups, 1).unwrap();
        assert_eq!(item.title, "Some Title");
        assert_eq!(item.video_id, "123");
        assert_eq!(item.url.as_deref(), Some("http://vk.com/video1_2"));
        assert_eq!(item.duration.as_deref(), Some("18:00"));
        assert_eq!(item.duration_seconds, Some(1080));
        assert_eq!(item.provider.as_deref(), Some("vk.com"));
        assert_eq!(item.provider_name.as_deref(), Some("VK Видео"));
        assert_eq!(item.published_date.as_deref(), Some("13 авг 2024"));
        assert_eq!(
            item.thumbnail_url.as_deref(),
            Some("https://avatars.mds.yandex.net/get-vthumb/1/abc/564x318_1")
        );
    }
}
