//! Naver 지식iN (Q&A) tab (`ssc=tab.m_kin.all`), added in audit W-21.
//!
//! The kin tab is server-rendered with the SDS design system: each
//! question is a `div[data-template-id="kinItem"]` inside
//! `ul.lst_nkin`; the question title is the text of the anchor to
//! `m.kin.naver.com/qna/…`, and the answer excerpt is the longest
//! body-text span in the same item block. Structure measured live
//! 2026-11-08 against the saved fixture `fixture-kin-mobile.html`.
//!
//! Paging: the list is an infinite-scroll list (`_infinite_list`)
//! whose deeper batches are loaded by the page's JavaScript from an
//! API the page does not issue as a complete URL. Only page 1 is
//! served; deeper pages are an explicit `unsupported_page` error.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{NaverSearchClient, SearchResult};

static ITEM_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"div[data-template-id="kinItem"]"#).unwrap());
static A_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").unwrap());
static SPAN_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("span").unwrap());

/// One 지식iN question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KinItem {
    pub rank: i64,
    pub title: String,
    pub url: String,
    /// Answer/body excerpt as rendered on the tab page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// Parse a saved Naver kin tab page. `limit` caps the items.
pub fn parse_kin_results(html: &str, limit: usize) -> Result<Vec<KinItem>> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for item in doc.select(&ITEM_SEL) {
        if items.len() >= limit {
            break;
        }
        let Some(title_a) = item.select(&A_SEL).find(|a| {
            a.value()
                .attr("href")
                .map(|h| h.contains("kin.naver.com/qna/"))
                .unwrap_or(false)
        }) else {
            continue;
        };
        let title = element_text(title_a);
        if title.is_empty() {
            continue;
        }
        let href = title_a.value().attr("href").unwrap_or_default();
        // The answer excerpt is the longest substantial text span in
        // the item that is not the title itself.
        let snippet = item
            .select(&SPAN_SEL)
            .map(|s| element_text(s))
            .filter(|t| t.chars().count() >= 20 && *t != title)
            .max_by_key(|t| t.chars().count());
        items.push(KinItem {
            rank: items.len() as i64 + 1,
            title,
            url: resolve_url(href, "https://m.search.naver.com"),
            snippet,
        });
    }
    if items.is_empty() {
        return Err(layout_error("kin"));
    }
    Ok(items)
}

impl NaverSearchClient {
    /// Search the 지식iN tab. Only page 1 is served (see the module
    /// docs); `limit` is clamped to 30.
    pub async fn kin_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<KinItem>> {
        let page = page.max(1);
        if page > 1 {
            return Err(unsupported_page("kin", page));
        }
        let limit = clamp_limit(limit, 30);
        let url = build_page_url(query, "kin", 1)?;
        let html = self.get_html(&url, SEARCH_REFERER).await?;
        let items = parse_kin_results(&html, limit)?;
        Ok(SearchResult::new("kin", query, page, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_page() {
        let html = r#"<html><body><ul class="lst_nkin">
            <div data-template-id="kinItem">
              <a href="https://m.kin.naver.com/qna/dirs/1/docs/123?answerNo=1"><span>질문 제목입니다</span></a>
              <span>답변 본문의 미리보기로 충분히 긴 텍스트가 여기에 들어갑니다</span>
            </div></ul></body></html>"#;
        let items = parse_kin_results(html, 10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "질문 제목입니다");
        assert!(items[0].snippet.as_deref().unwrap().contains("답변 본문"));
    }

    #[test]
    fn parse_empty_shell_is_a_layout_error() {
        assert!(parse_kin_results("<html><body></body></html>", 10).is_err());
    }
}
