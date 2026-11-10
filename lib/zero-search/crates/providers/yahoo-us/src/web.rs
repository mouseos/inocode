//! Yahoo (US) web search (`https://search.yahoo.com/search`).
//!
//! Unlike Yahoo! JAPAN, the US SERP ships no JSON payload — results are
//! classic server-rendered HTML, so they are read from the DOM:
//! `#web div.dd.algo` blocks, with the real destination URL recovered from
//! the `RU=` segment of Yahoo's `r.search.yahoo.com` redirect links.
//! This SERP variant shows no total hit count.

use anyhow::{anyhow, Result};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::{SearchResult, YahooUsSearchClient};

const REFERER: &str = "https://www.yahoo.com/";
/// Yahoo returns 7 organic results per page and paginates `b` by that step.
pub const PER_PAGE: i64 = 7;

static BLOCK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("#web div.dd.algo").unwrap());
static TITLE_LINK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".compTitle a").unwrap());
static TITLE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".compTitle h3.title").unwrap());
static SITE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".compTitle span.fc-141414").unwrap());
static SNIPPET_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".compText p").unwrap());
static NEXT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.next").unwrap());

/// One organic web result: rank, title, destination URL and snippet.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub site: Option<String>,
}

impl YahooUsSearchClient {
    /// Search the web index. `page` is 1-based; each page holds
    /// [`PER_PAGE`] results. The US SERP reports no total hit count, so
    /// `total_hits` is always `None`.
    pub async fn web_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<WebItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);

        let url = crate::build_search_url(query, "web", page)?;
        let html = self.get_html(&url, REFERER).await?;
        if !html.contains("id=\"web\"") {
            return Err(anyhow!(
                "web results container not found (Yahoo page layout may have changed)"
            ));
        }

        let base_rank = (page - 1) * PER_PAGE;
        let mut items = parse_web_results(&html, limit);
        for item in &mut items {
            item.rank += base_rank;
        }
        let has_next = web_has_next_page(&html);

        Ok(SearchResult::new("web", query, page, items)
            .with_next_page(has_next.then_some(page + 1))
            .with_related(parse_related_queries(&html)))
    }
}

/// Parse organic results out of a saved web SERP page. Ranks are 1-based
/// within the page. Offline: no network involved.
pub fn parse_web_results(html: &str, limit: usize) -> Vec<WebItem> {
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
        let title = block
            .select(&TITLE_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| element_text(&link));
        let site = block
            .select(&SITE_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        let description = block
            .select(&SNIPPET_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        items.push(WebItem {
            rank: items.len() as i64 + 1,
            title,
            url,
            description,
            site,
        });
    }
    items
}

/// Whether the saved page links to a further results page.
pub fn web_has_next_page(html: &str) -> bool {
    let doc = Html::parse_document(html);
    doc.select(&NEXT_SEL).next().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE1: &str = include_str!("../tests/fixtures/fixture-web.html");
    const PAGE2: &str = include_str!("../tests/fixtures/fixture-web-p2.html");

    #[test]
    fn parses_all_organic_results_from_fixture() {
        let items = parse_web_results(PAGE1, PER_PAGE as usize);
        assert_eq!(items.len(), 7, "page 1 should yield 7 organic results");
        for item in &items {
            assert!(item.url.starts_with("http"), "bad url: {}", item.url);
            assert!(
                !item.url.contains("r.search.yahoo.com"),
                "redirect URL was not decoded: {}",
                item.url
            );
            assert!(!item.title.is_empty());
        }
        assert_eq!(items[0].rank, 1);
        assert_eq!(items[6].rank, 7);
    }

    #[test]
    fn first_result_fields_are_populated() {
        let items = parse_web_results(PAGE1, PER_PAGE as usize);
        let first = &items[0];
        assert!(first.description.is_some(), "first result lacks a snippet");
        assert!(first.site.is_some(), "first result lacks a site name");
    }

    #[test]
    fn page_two_returns_different_results() {
        let p1 = parse_web_results(PAGE1, PER_PAGE as usize);
        let p2 = parse_web_results(PAGE2, PER_PAGE as usize);
        assert_eq!(p2.len(), 7);
        let urls1: Vec<&str> = p1.iter().map(|i| i.url.as_str()).collect();
        assert!(
            p2.iter().any(|i| !urls1.contains(&i.url.as_str())),
            "page 2 duplicated page 1"
        );
    }

    #[test]
    fn next_page_link_is_detected() {
        assert!(web_has_next_page(PAGE1));
        assert!(web_has_next_page(PAGE2));
        assert!(!web_has_next_page("<html><body></body></html>"));
    }

    #[test]
    fn limit_is_respected() {
        assert_eq!(parse_web_results(PAGE1, 3).len(), 3);
    }
}
