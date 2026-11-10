//! Bing news search (`https://www.bing.com/news/search`).
//!
//! Each result is a `div.news-card` that carries its destination, title
//! and publisher in attributes (`url`, `data-title`, `data-author`), so the
//! parser reads those first and only falls back to child elements.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{BingSearchClient, SearchResult};

static CARD_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".news-card").unwrap());
static SNIPPET_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".snippet").unwrap());
static TIME_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".t_t span[aria-label]").unwrap());
static IMG_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());
static LINK_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a[href]").unwrap());

/// One news result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Publication time text as shown by Bing (often relative, e.g. `3 時間前`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

/// Parse a saved Bing news results page. See [`crate::NewsItem`].
pub fn parse_news_results(html: &str, limit: usize) -> Result<Vec<NewsItem>> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for card in doc.select(&CARD_SEL) {
        if items.len() >= limit {
            break;
        }
        let v = card.value();
        let url = v.attr("url").map(|u| u.to_string()).or_else(|| {
            card.select(&LINK_SEL)
                .next()
                .and_then(|a| a.value().attr("href"))
                .map(|h| resolve_url(h, "https://www.bing.com"))
        });
        let Some(url) = url.filter(|u| !u.is_empty()) else {
            continue;
        };
        let title = v
            .attr("data-title")
            .map(clean_text)
            .filter(|t| !t.is_empty());
        let Some(title) = title else { continue };
        let snippet = card
            .select(&SNIPPET_SEL)
            .next()
            .map(|s| {
                let text = clean_text(&s.text().collect::<String>());
                if text.is_empty() {
                    s.value().attr("title").map(clean_text).unwrap_or_default()
                } else {
                    text
                }
            })
            .filter(|s| !s.is_empty());
        let thumbnail = card.select(&IMG_SEL).find_map(|img| {
            img.value()
                .attr("data-src-hq")
                .map(|src| resolve_url(src, "https://www.bing.com"))
        });
        items.push(NewsItem {
            rank: items.len() as i64 + 1,
            title,
            url,
            publisher: v
                .attr("data-author")
                .map(clean_text)
                .filter(|s| !s.is_empty()),
            published: card
                .select(&TIME_SEL)
                .next()
                .and_then(|t| t.value().attr("aria-label"))
                .map(clean_text)
                .filter(|s| !s.is_empty()),
            snippet,
            thumbnail,
        });
    }
    if items.is_empty() {
        return Err(layout_error("news"));
    }
    Ok(items)
}

impl BingSearchClient {
    /// Search news. Bing's news page has no paging parameter, so this
    /// returns the first page; `limit` is clamped to 50.
    pub async fn news_search(
        &self,
        query: &str,
        limit: i64,
        mkt: &str,
        setlang: &str,
    ) -> Result<SearchResult<NewsItem>> {
        let limit = clamp_limit(limit, 50);
        let url = build_page_url(query, "news", 1, mkt, setlang)?;
        let html = self.get_html(&url, "https://www.bing.com/news").await?;
        let items = parse_news_results(&html, limit)?;
        Ok(SearchResult::new("news", query, 1, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_shell_is_a_layout_error() {
        assert!(parse_news_results("<html><body></body></html>", 5).is_err());
    }
}
