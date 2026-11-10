//! Bing video search (`https://www.bing.com/videos/search`).
//!
//! Result units are `div.mc_vtvc`. The visible links point at Bing's own
//! related-video pages; the real destination travels in the `churl` query
//! parameter of those links and is what this parser returns as `url`.
//! Duration hides in the `vrhm` JSON attribute of the `.vrhdata` element.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::{build_page_url, VIDEO_PER_PAGE};
use crate::{BingSearchClient, SearchResult};

static UNIT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".mc_vtvc").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".mc_vtvc_title").unwrap());
static LINK_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());
static THUMB_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());
static VRHDATA_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".vrhdata").unwrap());
static VIEWS_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".meta_vc_content").unwrap());
static DATE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".meta_pd_content").unwrap());
static CHANNEL_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".mc_vtvc_meta_row_channel").unwrap());
static META_ROW_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".mc_vtvc_meta_row").unwrap());

/// One video result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoItem {
    pub rank: i64,
    pub title: String,
    /// Real destination URL, taken from the link's `churl` parameter.
    pub url: String,
    /// Hosting site (e.g. `YouTube`, `TikTok`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Channel / uploader name when Bing shows one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uploader: Option<String>,
    /// Playback length as shown by Bing (e.g. `0:31`, `12:05`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// View count text as shown by Bing (e.g. `視聴回数: 25.8万 回`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub views: Option<String>,
    /// Upload date text as shown by Bing (often relative, e.g. `1 週間前`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

/// Parsed video results page. `total_parsed` counts every unit on the page
/// before the caller's `limit` was applied.
#[derive(Debug, Clone)]
pub struct VideoResults {
    pub items: Vec<VideoItem>,
    pub total_parsed: usize,
}

fn text_of(el: &scraper::ElementRef) -> String {
    clean_text(&el.text().collect::<String>())
}

fn normalize_duration(raw: &str) -> Option<String> {
    let d = raw.trim();
    if d.is_empty() {
        return None;
    }
    // Bing reports "00:31" for 31 seconds; show it as "0:31".
    let d = d.strip_prefix("00:").map(|rest| format!("0:{rest}"));
    Some(d.unwrap_or_else(|| raw.to_string()))
}

fn parse_duration(unit: &scraper::ElementRef) -> Option<String> {
    let el = unit.select(&VRHDATA_SEL).next()?;
    let raw = el.value().attr("vrhm")?;
    let value: serde_json::Value = serde_json::from_str(&decode_entities(raw)).ok()?;
    let du = value.get("du")?.as_str()?;
    normalize_duration(du)
}

/// The hosting site name lives in the meta row that has no view-count /
/// date element, as the row's first span; the same row may also carry the
/// channel span, which is excluded here.
fn parse_source(unit: &scraper::ElementRef) -> Option<String> {
    for row in unit.select(&META_ROW_SEL) {
        if row.select(&VIEWS_SEL).next().is_some() || row.select(&DATE_SEL).next().is_some() {
            continue;
        }
        let mut parts = String::new();
        for child in row.children().filter_map(scraper::ElementRef::wrap) {
            if CHANNEL_SEL.matches(&child) {
                continue;
            }
            parts.push_str(&text_of(&child));
        }
        let t = clean_text(&parts);
        if !t.is_empty() {
            return Some(t);
        }
    }
    None
}

/// Parse a saved Bing video results page (see [`crate::VideoResults`]).
pub fn parse_video_results(html: &str, limit: usize) -> Result<VideoResults> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    let mut total_parsed = 0usize;
    for unit in doc.select(&UNIT_SEL) {
        // Real destination: the first link carrying a `churl` parameter.
        let mut real_url: Option<String> = None;
        let mut fallback_url: Option<String> = None;
        for a in unit.select(&LINK_SEL) {
            let Some(href) = a.value().attr("href") else {
                continue;
            };
            if fallback_url.is_none() {
                fallback_url = Some(resolve_url(href, "https://www.bing.com"));
            }
            if let Some(churl) = query_param(&resolve_url(href, "https://www.bing.com"), "churl") {
                real_url = Some(churl);
                break;
            }
        }
        let Some(url) = real_url.or(fallback_url) else {
            continue;
        };
        total_parsed += 1;
        if items.len() >= limit {
            continue;
        }
        let title = unit
            .select(&TITLE_SEL)
            .next()
            .map(|t| {
                let attr = t.value().attr("title").unwrap_or("");
                if attr.trim().is_empty() {
                    text_of(&t)
                } else {
                    clean_text(attr)
                }
            })
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let thumbnail = unit
            .select(&THUMB_SEL)
            .filter_map(|img| img.value().attr("src"))
            .find(|src| src.starts_with("http"))
            .map(|s| s.to_string());
        items.push(VideoItem {
            rank: items.len() as i64 + 1,
            title,
            url,
            source: parse_source(&unit),
            uploader: unit
                .select(&CHANNEL_SEL)
                .next()
                .map(|c| text_of(&c))
                .filter(|s| !s.is_empty()),
            duration: parse_duration(&unit),
            views: unit
                .select(&VIEWS_SEL)
                .next()
                .map(|v| text_of(&v))
                .filter(|s| !s.is_empty()),
            upload_date: unit
                .select(&DATE_SEL)
                .next()
                .map(|d| text_of(&d))
                .filter(|s| !s.is_empty()),
            thumbnail,
        });
    }
    if total_parsed == 0 {
        return Err(layout_error("video"));
    }
    Ok(VideoResults {
        items,
        total_parsed,
    })
}

impl BingSearchClient {
    /// Search videos. `page` advances Bing's `first` offset by the page's
    /// own batch size (35); `limit` is clamped to 35.
    pub async fn video_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
        mkt: &str,
        setlang: &str,
    ) -> Result<SearchResult<VideoItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, VIDEO_PER_PAGE);
        let url = build_page_url(query, "video", page, mkt, setlang)?;
        let html = self.get_html(&url, "https://www.bing.com/videos").await?;
        let parsed = parse_video_results(&html, limit)?;
        let mut items = parsed.items;
        for item in &mut items {
            item.rank += (page - 1) * VIDEO_PER_PAGE;
        }
        let has_next = parsed.total_parsed >= VIDEO_PER_PAGE as usize;
        Ok(SearchResult::new("video", query, page, items)
            .with_next_page(has_next.then_some(page + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shell_is_a_layout_error() {
        assert!(parse_video_results("<html><body></body></html>", 5).is_err());
    }

    #[test]
    fn duration_normalises_leading_zero_hour() {
        assert_eq!(normalize_duration("00:31").as_deref(), Some("0:31"));
        assert_eq!(normalize_duration("12:05").as_deref(), Some("12:05"));
        assert!(normalize_duration("  ").is_none());
    }
}
