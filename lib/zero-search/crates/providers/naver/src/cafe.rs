//! Naver Cafe tab (`ssc=tab.m_cafe.all`), added in audit W-21.
//!
//! The cafe tab still uses the classic server-rendered list layout:
//! each post is an `li.bx` inside `section.sp_ncafe`'s
//! `ul.lst_view`, with the title in `.title_area a.title_link`, the
//! body excerpt in `.dsc_area a.dsc_link`, the cafe name in
//! `.user_info a.name` and the relative date next to it. Structure
//! measured live 2026-11-08 against the saved fixture
//! `fixture-cafe-mobile.html`.
//!
//! Paging: the page configures a "more" API
//! (`s.search.naver.com/p/cafe/…`) whose request URL the page's
//! JavaScript assembles from parts, so — unlike the image/news/
//! video/blog tabs — no complete next-batch URL is page-issued.
//! Only page 1 is served; deeper pages are an explicit
//! `unsupported_page` error.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{NaverSearchClient, SearchResult};

static ITEM_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("section.sp_ncafe li.bx").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".title_area a").unwrap());
static DSC_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".dsc_area a").unwrap());
static NAME_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".user_info a.name").unwrap());
static USERINFO_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".user_info").unwrap());
static THUMB_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".mod_ugc_thumb_area img").unwrap());

/// One cafe post.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CafeItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cafe_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cafe_url: Option<String>,
    /// Publish time as Naver renders it (relative, e.g. `1주 전`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
}

/// Parse a saved Naver cafe tab page. `limit` caps the items.
pub fn parse_cafe_results(html: &str, limit: usize) -> Result<Vec<CafeItem>> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for li in doc.select(&ITEM_SEL) {
        if items.len() >= limit {
            break;
        }
        let Some(title_a) = li.select(&TITLE_SEL).next() else {
            continue;
        };
        let Some(href) = title_a.value().attr("href") else {
            continue;
        };
        let title = element_text(title_a);
        if title.is_empty() {
            continue;
        }
        let rank = li
            .value()
            .attr("data-index")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(items.len() as i64 + 1);
        let name_a = li.select(&NAME_SEL).next();
        items.push(CafeItem {
            rank,
            title,
            url: resolve_url(href, "https://m.search.naver.com"),
            snippet: li
                .select(&DSC_SEL)
                .next()
                .map(element_text)
                .filter(|s| !s.is_empty()),
            cafe_name: name_a.map(element_text).filter(|s| !s.is_empty()),
            cafe_url: name_a
                .and_then(|a| a.value().attr("href"))
                .map(str::to_string),
            published: li
                .select(&USERINFO_SEL)
                .next()
                .and_then(|u| find_time(&element_text(u))),
            thumbnail: li.select(&THUMB_SEL).next().and_then(|img| {
                let src = img.value().attr("src")?;
                Some(query_param(src, "src").unwrap_or_else(|| src.to_string()))
            }),
        });
    }
    if items.is_empty() {
        return Err(layout_error("cafe"));
    }
    Ok(items)
}

impl NaverSearchClient {
    /// Search the Cafe tab. Only page 1 is served (see the module
    /// docs); `limit` is clamped to 30.
    pub async fn cafe_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<CafeItem>> {
        let page = page.max(1);
        if page > 1 {
            return Err(unsupported_page("cafe", page));
        }
        let limit = clamp_limit(limit, 30);
        let url = build_page_url(query, "cafe", 1)?;
        let html = self.get_html(&url, SEARCH_REFERER).await?;
        let items = parse_cafe_results(&html, limit)?;
        Ok(SearchResult::new("cafe", query, page, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_page() {
        let html = r#"<html><body><section class="sc sp_ncafe">
            <ul class="lst_view"><li class="bx" data-index="1">
              <div class="user_box"><div class="user_info">
                <a class="name" href="https://m.cafe.naver.com/example">예제 카페</a>
                <span class="sub">1주 전</span></div></div>
              <div class="detail_box">
                <div class="title_area"><a class="title_link" href="https://cafe.naver.com/example/123">글 제목입니다</a></div>
                <div class="dsc_area"><a class="dsc_link" href="https://cafe.naver.com/example/123">본문 미리보기 텍스트입니다</a></div>
              </div>
            </li></ul></section></body></html>"#;
        let items = parse_cafe_results(html, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "글 제목입니다");
        assert_eq!(items[0].cafe_name.as_deref(), Some("예제 카페"));
        assert_eq!(items[0].published.as_deref(), Some("1주 전"));
        assert!(items[0].snippet.as_deref().unwrap().contains("미리보기"));
    }

    #[test]
    fn parse_empty_shell_is_a_layout_error() {
        assert!(parse_cafe_results("<html><body></body></html>", 10).is_err());
    }
}
