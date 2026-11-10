//! Naver blog tab (`ssc=tab.m_blog.all`).
//!
//! Each post is a group of anchors sharing one
//! `m.blog.naver.com/<blog-id>/<post-no>` URL: a title anchor, a snippet
//! anchor, thumbnail anchors and a comment-count anchor. The blog's
//! display name comes from the profile anchor that precedes the group
//! (`m.blog.naver.com/<blog-id>` or `in.naver.com/<id>`), and the
//! relative date is read from the post's card. The tab serves one batch
//! (30 posts) per page load, so page ≥ 2 is an error.

use anyhow::Result;
use scraper::{ElementRef, Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{NaverSearchClient, SearchResult};

static A_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").unwrap());
static IMG_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img").unwrap());

/// One blog post.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlogItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blog_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blog_id: Option<String>,
    /// Publish time as Naver renders it (relative, e.g. `4주 전`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

/// Split a blog URL into `(blog_id, post_no)` if it is a post URL.
fn post_parts(href: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(href).ok()?;
    if parsed.host_str() != Some("m.blog.naver.com") {
        return None;
    }
    let segs: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    if segs.len() == 2 && !segs[0].is_empty() && segs[1].chars().all(|c| c.is_ascii_digit()) {
        Some((segs[0].to_string(), segs[1].to_string()))
    } else {
        None
    }
}

fn is_profile_url(href: &str) -> bool {
    let Ok(parsed) = url::Url::parse(href) else {
        return false;
    };
    let segs: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.collect())
        .unwrap_or_default();
    matches!(
        parsed.host_str(),
        Some("m.blog.naver.com") | Some("in.naver.com")
    ) && segs.len() == 1
        && !segs[0].is_empty()
}

fn distinct_posts(er: ElementRef) -> usize {
    let mut seen: Vec<String> = Vec::new();
    for a in er.select(&A_SEL) {
        if let Some(href) = a.value().attr("href") {
            if post_parts(href).is_some() && !seen.contains(&href.to_string()) {
                seen.push(href.to_string());
            }
        }
    }
    seen.len()
}

/// The post's card: smallest ancestor of the title anchor whose subtree
/// still holds only this post.
fn post_card<'a>(title_anchor: ElementRef<'a>) -> ElementRef<'a> {
    let mut card = title_anchor;
    let mut node = title_anchor.parent();
    for _ in 0..10 {
        let Some(n) = node else { break };
        let Some(er) = ElementRef::wrap(n) else {
            node = n.parent();
            continue;
        };
        if distinct_posts(er) > 1 {
            break;
        }
        card = er;
        node = n.parent();
    }
    card
}

#[derive(Default)]
struct Group {
    url: String,
    blog_id: Option<String>,
    title: Option<String>,
    snippet: Option<String>,
    thumbnail: Option<String>,
    blog_name: Option<String>,
    published: Option<String>,
}

/// Parse a saved Naver blog tab page.
pub fn parse_blog_results(html: &str, limit: usize) -> Result<Vec<BlogItem>> {
    let doc = Html::parse_document(html);
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Group> = HashMap::new();
    let mut pending_blog_name: Option<String> = None;

    for a in doc.select(&A_SEL) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        let text = element_text(a);
        if post_parts(href).is_none() {
            if is_profile_url(href) && !text.is_empty() {
                pending_blog_name = Some(text);
            }
            continue;
        }
        let (blog_id, _) = post_parts(href).unwrap();
        let group = groups.entry(href.to_string()).or_insert_with(|| {
            order.push(href.to_string());
            Group {
                url: href.to_string(),
                blog_id: Some(blog_id),
                ..Default::default()
            }
        });
        if group.thumbnail.is_none() {
            if let Some(img) = a.select(&IMG_SEL).next() {
                group.thumbnail = img
                    .value()
                    .attr("src")
                    .or_else(|| img.value().attr("data-src"))
                    .map(str::to_string);
            }
        }
        if text.is_empty() || text.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if group.title.is_none() {
            group.title = Some(text);
            group.blog_name = pending_blog_name.take();
            let card = post_card(a);
            group.published = find_time(&element_text(card));
        } else if group.snippet.is_none()
            && Some(&text) != group.title.as_ref()
            && text.chars().count() >= 10
        {
            group.snippet = Some(text);
        }
    }

    let mut items = Vec::new();
    for url in &order {
        if items.len() >= limit {
            break;
        }
        let group = &groups[url];
        let Some(title) = &group.title else { continue };
        items.push(BlogItem {
            rank: items.len() as i64 + 1,
            title: title.clone(),
            url: group.url.clone(),
            blog_name: group.blog_name.clone(),
            blog_id: group.blog_id.clone(),
            published: group.published.clone(),
            snippet: group.snippet.clone(),
            thumbnail: group.thumbnail.clone(),
        });
    }
    if items.is_empty() {
        return Err(layout_error("blog"));
    }
    Ok(items)
}

impl NaverSearchClient {
    /// Search the blog tab. Page 1 is the tab page's batch (30
    /// posts); deeper pages follow the page-issued more-API chain
    /// (see `more`), up to [`MAX_PAGE`](crate::MAX_PAGE). `limit` is
    /// clamped to 30.
    pub async fn blog_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<BlogItem>> {
        let page = page.max(1);
        if page > crate::more::MAX_PAGE {
            return Err(crate::common::unsupported_page("blog", page));
        }
        let limit = clamp_limit(limit, 30);
        let url = build_page_url(query, "blog", 1)?;
        let html = self.get_html(&url, SEARCH_REFERER).await?;
        let items = crate::more::chained_items(
            self,
            "blog",
            &html,
            page,
            |h| parse_blog_results(h, usize::MAX),
            |payload| match payload {
                crate::more::MorePayload::Html(fragment) => {
                    parse_blog_results(&fragment, usize::MAX)
                }
                _ => Err(layout_error("blog")),
            },
            |item, rank| item.rank = rank,
        )
        .await?;
        let items: Vec<BlogItem> = items.into_iter().take(limit).collect();
        Ok(SearchResult::new("blog", query, page, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_groups_post_anchors() {
        let html = r#"<html><body><div class="card">
            <a href="https://in.naver.com/seoulbumpkin">서울촌뜨기</a><span>4주 전</span>
            <a href="https://m.blog.naver.com/yjhd2020/224443896187">무료 MBTI 테스트 알아보기</a>
            <a href="https://m.blog.naver.com/yjhd2020/224443896187">이것저것 테스트를 찾아보는 게 재미가 됐어요. 본문 미리보기입니다.</a>
            <a href="https://m.blog.naver.com/yjhd2020/224443896187">11</a>
            <a href="https://m.blog.naver.com/yjhd2020/224443896187"><img src="https://search.pstatic.net/t.jpg"></a>
            </div></body></html>"#;
        let items = parse_blog_results(html, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "무료 MBTI 테스트 알아보기");
        assert_eq!(items[0].blog_name.as_deref(), Some("서울촌뜨기"));
        assert_eq!(items[0].blog_id.as_deref(), Some("yjhd2020"));
        assert_eq!(items[0].published.as_deref(), Some("4주 전"));
        assert!(items[0].snippet.as_ref().unwrap().contains("미리보기"));
        assert!(items[0].thumbnail.is_some());
    }

    #[test]
    fn parse_empty_shell_is_a_layout_error() {
        assert!(parse_blog_results("<html><body></body></html>", 10).is_err());
    }
}
