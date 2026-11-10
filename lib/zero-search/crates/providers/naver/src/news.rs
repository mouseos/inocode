//! Naver news tab (`ssc=tab.m_news.all&where=m_news`).
//!
//! The tab renders each article as a group of anchors sharing one URL:
//! a title anchor, a snippet anchor and a thumbnail anchor, preceded by
//! the publisher's press-page links (`media.naver.com/press/<id>`). The
//! parser walks the anchors in document order and folds each group into
//! one item. Articles Naver hosts (`n.news.naver.com/article/...`),
//! its entertainment/sports subdomains and external publisher URLs all
//! appear; related-story links of a cluster are listed as their own
//! items, exactly as the page lists them.
//!
//! The tab serves one batch (15 stories) per page load; deeper batches
//! need the page's own session API, so page ≥ 2 is an error.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{NaverSearchClient, SearchResult};

static A_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").unwrap());
static IMG_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());

/// One news article.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewsItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// Publish time as Naver renders it (relative, e.g. `8시간 전`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

fn is_article_url(href: &str) -> bool {
    (href.contains("/article/") || href.contains("/articleView"))
        && !href.contains("media.naver.com")
        && !href.starts_with('?')
        && !href.starts_with('#')
}

#[derive(Default)]
struct Group {
    url: String,
    title: Option<String>,
    snippet: Option<String>,
    thumbnail: Option<String>,
    publisher: Option<String>,
    published: Option<String>,
}

/// Parse a saved Naver news tab page.
pub fn parse_news_results(html: &str, limit: usize) -> Result<Vec<NewsItem>> {
    let doc = Html::parse_document(html);
    let mut groups: Vec<Group> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut pending_publisher: Option<String> = None;
    let mut pending_time: Option<String> = None;

    for a in doc.select(&A_SEL) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        let text = element_text(a);
        if href.contains("media.naver.com/press/") {
            if !text.is_empty() {
                pending_publisher = Some(text);
                // The relative time sits next to the publisher name in
                // the card header; look at the anchor's neighbourhood.
                let mut node = a.parent();
                for _ in 0..3 {
                    let Some(n) = node else { break };
                    if let Some(er) = scraper::ElementRef::wrap(n) {
                        if let Some(t) = find_time(&element_text(er)) {
                            pending_time = Some(t);
                            break;
                        }
                    }
                    node = n.parent();
                }
            }
            continue;
        }
        if !is_article_url(href) {
            continue;
        }
        let url = resolve_url(href, "https://m.search.naver.com");
        let gi = *index.entry(url.clone()).or_insert_with(|| {
            groups.push(Group {
                url: url.clone(),
                ..Default::default()
            });
            groups.len() - 1
        });
        let group = &mut groups[gi];
        if group.thumbnail.is_none() {
            if let Some(img) = a.select(&IMG_SEL).next() {
                group.thumbnail = img
                    .value()
                    .attr("src")
                    .or_else(|| img.value().attr("data-src"))
                    .map(str::to_string);
            }
        }
        if text.is_empty() {
            continue;
        }
        if group.title.is_none() {
            group.title = Some(text);
            group.publisher = pending_publisher.take();
            group.published = pending_time.take();
        } else if group.snippet.is_none() && Some(&text) != group.title.as_ref() {
            group.snippet = Some(text);
        }
    }

    let mut items = Vec::new();
    for group in groups {
        if items.len() >= limit {
            break;
        }
        let Some(title) = group.title else { continue };
        items.push(NewsItem {
            rank: items.len() as i64 + 1,
            title,
            url: group.url,
            publisher: group.publisher,
            published: group.published,
            snippet: group.snippet,
            thumbnail: group.thumbnail,
        });
    }
    if items.is_empty() {
        return Err(layout_error("news"));
    }
    Ok(items)
}

impl NaverSearchClient {
    /// Search the news tab. Page 1 is the tab page's batch; deeper
    /// pages follow the page-issued more-API chain (see `more`), up
    /// to [`MAX_PAGE`](crate::MAX_PAGE). `limit` is clamped to 30.
    pub async fn news_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<NewsItem>> {
        let page = page.max(1);
        if page > crate::more::MAX_PAGE {
            return Err(crate::common::unsupported_page("news", page));
        }
        let limit = clamp_limit(limit, 30);
        let url = build_page_url(query, "news", 1)?;
        let html = self.get_html(&url, SEARCH_REFERER).await?;
        let items = crate::more::chained_items(
            self,
            "news",
            &html,
            page,
            |h| parse_news_results(h, usize::MAX),
            |payload| match payload {
                crate::more::MorePayload::Html(fragment) => {
                    parse_news_results(&fragment, usize::MAX)
                }
                _ => Err(layout_error("news")),
            },
            |item, rank| item.rank = rank,
        )
        .await?;
        let items: Vec<NewsItem> = items.into_iter().take(limit).collect();
        Ok(SearchResult::new("news", query, page, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_groups_anchors_by_url() {
        let html = r#"<html><body>
            <a href="https://media.naver.com/press/366"></a>
            <a href="https://media.naver.com/press/366">조선비즈</a>
            <span>8시간 전</span>
            <a href="https://n.news.naver.com/article/366/0001203132?sid=004">기사 제목입니다</a>
            <a href="https://n.news.naver.com/article/366/0001203132?sid=004">본문 스니펫이 여기에 들어갑니다</a>
            <a href="https://n.news.naver.com/article/366/0001203132?sid=004"><img src="https://search.pstatic.net/t.jpg"></a>
            </body></html>"#;
        let items = parse_news_results(html, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "기사 제목입니다");
        assert_eq!(items[0].publisher.as_deref(), Some("조선비즈"));
        assert_eq!(
            items[0].snippet.as_deref(),
            Some("본문 스니펫이 여기에 들어갑니다")
        );
        assert!(items[0].thumbnail.is_some());
    }

    #[test]
    fn parse_empty_shell_is_a_layout_error() {
        assert!(parse_news_results("<html><body></body></html>", 10).is_err());
    }
}
