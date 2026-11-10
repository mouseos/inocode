//! Yahoo (US) news search (`https://news.search.yahoo.com/search`).
//!
//! Articles are `div.dd.NewsArticle` blocks; only blocks carrying an
//! `h4.s-title` headline link are real articles (the page also ships
//! module shells without one). Headline links are Yahoo redirect URLs —
//! the article URL is recovered from the `RU=` segment, like web search.

use anyhow::{anyhow, Result};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::{SearchResult, YahooUsSearchClient};

const REFERER: &str = "https://www.yahoo.com/";
/// Yahoo returns 10 articles per page and paginates `b` by that step.
pub const PER_PAGE: i64 = 10;

static BLOCK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.dd.NewsArticle").unwrap());
static TITLE_LINK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("h4.s-title a").unwrap());
static SOURCE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".s-source").unwrap());
static TIME_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".s-time").unwrap());
static DESC_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("p.s-desc").unwrap());
static THUMB_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("ul.compArticleList a.thmb img").unwrap());
static NEXT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.next").unwrap());

/// One news article: headline, article URL, publisher and publish time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Relative publish time as Yahoo phrases it ("37 minutes ago").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

impl YahooUsSearchClient {
    /// Search news articles. `page` is 1-based; each page holds
    /// [`PER_PAGE`] articles.
    pub async fn news_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<NewsItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);

        let url = crate::build_search_url(query, "news", page)?;
        let html = self.get_html(&url, REFERER).await?;
        if !html.contains("NewsArticle") {
            return Err(anyhow!(
                "news results container not found (Yahoo page layout may have changed)"
            ));
        }

        let base_rank = (page - 1) * PER_PAGE;
        let mut items = parse_news_results(&html, limit);
        for item in &mut items {
            item.rank += base_rank;
        }
        let has_next = news_has_next_page(&html);

        Ok(SearchResult::new("news", query, page, items)
            .with_next_page(has_next.then_some(page + 1))
            .with_related(parse_related_queries(&html)))
    }
}

/// Parse articles out of a saved news SERP page. Offline.
pub fn parse_news_results(html: &str, limit: usize) -> Vec<NewsItem> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for block in doc.select(&BLOCK_SEL) {
        if items.len() >= limit {
            break;
        }
        let Some(link) = block.select(&TITLE_LINK_SEL).next() else {
            continue;
        };
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let url = decode_redirect_url(href);
        if !url.starts_with("http") {
            continue;
        }
        let title = element_text(&link);
        if title.is_empty() {
            continue;
        }
        let source = block
            .select(&SOURCE_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        let published = block
            .select(&TIME_SEL)
            .next()
            .map(|el| element_text(&el).trim_end_matches('·').trim().to_string())
            .filter(|s| !s.is_empty());
        let description = block
            .select(&DESC_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        let thumbnail = block
            .select(&THUMB_SEL)
            .next()
            .and_then(|el| el.value().attr("src"))
            .map(decode_entities)
            .filter(|s| !s.is_empty());
        items.push(NewsItem {
            rank: items.len() as i64 + 1,
            title,
            url,
            source,
            published,
            description,
            thumbnail,
        });
    }
    items
}

/// Whether the saved page links to a further results page.
pub fn news_has_next_page(html: &str) -> bool {
    let doc = Html::parse_document(html);
    doc.select(&NEXT_SEL).next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!("../tests/fixtures/fixture-news.html");

    #[test]
    fn parses_articles_from_fixture() {
        let items = parse_news_results(PAGE, PER_PAGE as usize);
        assert_eq!(items.len(), 10, "fixture page should yield 10 articles");
        for item in &items {
            assert!(item.url.starts_with("http"), "bad url: {}", item.url);
            assert!(
                !item.url.contains("r.search.yahoo.com"),
                "redirect URL was not decoded: {}",
                item.url
            );
            assert!(!item.title.is_empty());
        }
    }

    #[test]
    fn first_article_fields_are_populated() {
        let items = parse_news_results(PAGE, PER_PAGE as usize);
        let first = &items[0];
        assert_eq!(
            first.url,
            "https://macdailynews.com/2026/11/06/apple-tv-announces-season-three-for-knife-edge-chasing-michelin-stars/"
        );
        assert_eq!(first.source.as_deref(), Some("MacDailyNews"));
        assert_eq!(first.published.as_deref(), Some("37 minutes ago"));
        assert!(first.description.is_some());
        assert!(first.thumbnail.is_some());
    }

    #[test]
    fn next_page_link_is_detected() {
        assert!(news_has_next_page(PAGE));
        assert!(!news_has_next_page("<html><body></body></html>"));
    }

    #[test]
    fn limit_is_respected() {
        assert_eq!(parse_news_results(PAGE, 4).len(), 4);
    }
}
