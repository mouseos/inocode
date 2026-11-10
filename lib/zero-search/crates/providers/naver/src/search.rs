//! Naver integrated search (`where=m`, the app's own search URL).
//!
//! The integrated page is a stack of subject sections
//! (`.api_subject_bx`), each with a heading (`<h2>`) and its own result
//! style. This parser returns the sections in page order; inside a
//! section it prefers the web-result anchors (`data-nlog-area="*.w.link"`)
//! when the section has them and otherwise folds the section's outbound
//! links into generic items. Utility links (Keep, help, login, the search
//! host itself) are never results.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::NaverSearchClient;

static SECTION_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".api_subject_bx").unwrap());
static H2_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h2").unwrap());
static H3_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("h3").unwrap());
static A_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a").unwrap());
static WLINK_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(r#"a[data-nlog-area$=".w.link"]"#).unwrap());
static P_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("p").unwrap());

/// Hosts whose links are page furniture, never search results.
const UTILITY_HOSTS: &[&str] = &[
    "keep.naver.com",
    "m.keep.naver.com",
    "help.naver.com",
    "nid.naver.com",
    "m.search.naver.com",
    "search.naver.com",
    "m.naver.com",
    "www.naver.com",
];

const UTILITY_TEXTS: &[&str] = &["자세히 보기", "더보기", "Keep에 저장", "Keep에 바로가기"];

/// One generic integrated-search result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegratedItem {
    pub title: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snippet: Option<String>,
}

/// One subject section of the integrated page (news, images, dictionary…).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegratedSection {
    pub name: String,
    pub items: Vec<IntegratedItem>,
}

/// Response envelope for [`NaverSearchClient::search`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegratedResult {
    pub provider: String,
    pub vertical: String,
    pub ok: bool,
    pub query: String,
    pub page: i64,
    /// Total items across all sections.
    pub count: usize,
    pub sections: Vec<IntegratedSection>,
}

/// Body paragraph for a generic-branch item: climb from the item's
/// anchor while the ancestor still links only to this item, and
/// take the first substantial `<p>` text found (audit W-27).
fn card_snippet(anchor: scraper::ElementRef, own_href: &str) -> Option<String> {
    let mut node = anchor.parent();
    for _ in 0..6 {
        let n = node?;
        let er = scraper::ElementRef::wrap(n)?;
        let links_elsewhere = er.select(&A_SEL).any(|a| {
            let Some(h) = a.value().attr("href") else {
                return false;
            };
            if !h.starts_with("http") || h == own_href {
                return false;
            }
            url::Url::parse(h)
                .ok()
                .and_then(|u| u.host_str().map(|host| !UTILITY_HOSTS.contains(&host)))
                .unwrap_or(false)
        });
        if links_elsewhere {
            return None;
        }
        if let Some(text) = er
            .select(&P_SEL)
            .map(|p| element_text(p))
            .find(|t| t.chars().count() >= 20 && !t.contains("네이버가 AI"))
        {
            return Some(text);
        }
        node = n.parent();
    }
    None
}

fn section_name(section: scraper::ElementRef) -> String {
    if let Some(h) = section
        .select(&H2_SEL)
        .next()
        .or_else(|| section.select(&H3_SEL).next())
    {
        let name = element_text(h);
        if !name.is_empty() {
            return name;
        }
    }
    // Fall back to the wrapping <section class="sc sp_n…"> marker.
    let mut node = Some(section);
    for _ in 0..4 {
        let Some(er) = node else { break };
        let classes = er.value().classes().collect::<Vec<_>>().join(" ");
        for (marker, label) in [
            ("sp_nnews", "뉴스"),
            ("sp_nblog", "블로그"),
            ("sp_nimage", "이미지"),
            ("sp_nvideo", "동영상"),
            ("sp_nclip", "네이버 클립"),
            ("sp_ncafe", "카페"),
            ("sp_nkin", "지식iN"),
            ("sp_ndic", "어학사전"),
        ] {
            if classes.contains(marker) {
                return label.to_string();
            }
        }
        node = er.parent().and_then(scraper::ElementRef::wrap);
    }
    "기타".to_string()
}

/// Parse a saved Naver integrated search page. `per_section_limit` caps
/// the items kept per section.
pub fn parse_integrated_results(
    html: &str,
    per_section_limit: usize,
) -> Result<Vec<IntegratedSection>> {
    let doc = Html::parse_document(html);
    let mut sections = Vec::new();
    for section in doc.select(&SECTION_SEL) {
        // A section nested inside another is counted through its parent;
        // processing it again would duplicate its items.
        let nested = section.ancestors().any(|n| {
            scraper::ElementRef::wrap(n)
                .map(|er| {
                    er.value()
                        .has_class("api_subject_bx", scraper::CaseSensitivity::CaseSensitive)
                })
                .unwrap_or(false)
        });
        if nested {
            continue;
        }
        let mut items: Vec<IntegratedItem> = Vec::new();
        let wlinks: Vec<_> = section.select(&WLINK_SEL).collect();
        if !wlinks.is_empty() {
            for link in wlinks {
                if items.len() >= per_section_limit {
                    break;
                }
                let Some(href) = link.value().attr("href") else {
                    continue;
                };
                let title = element_text(link);
                if title.is_empty() || href.starts_with('#') {
                    continue;
                }
                let card = card_for(link, &WLINK_SEL, 10);
                let snippet = card
                    .select(&P_SEL)
                    .map(|p| element_text(p))
                    .find(|t| t.chars().count() >= 20 && !t.contains("네이버가 AI"));
                items.push(IntegratedItem {
                    title,
                    url: resolve_url(href, "https://m.search.naver.com"),
                    snippet,
                });
            }
        } else {
            let mut seen: Vec<String> = Vec::new();
            for a in section.select(&A_SEL) {
                let Some(href) = a.value().attr("href") else {
                    continue;
                };
                if !href.starts_with("http") {
                    continue;
                }
                if let Ok(parsed) = url::Url::parse(href) {
                    if let Some(host) = parsed.host_str() {
                        if UTILITY_HOSTS.contains(&host) {
                            continue;
                        }
                    }
                }
                let text = element_text(a);
                // A repeated anchor for an item already collected:
                // cards link the same URL from the title, the
                // thumbnail and the body text, so a long body text on
                // a repeat is the item's snippet (the same grouping
                // rule the news parser uses; audit W-27).
                if let Some(pos) = seen.iter().position(|h| h == href) {
                    let item = &mut items[pos];
                    if item.snippet.is_none()
                        && text.chars().count() >= 20
                        && text != item.title
                        && !text.contains("네이버가 AI")
                    {
                        item.snippet = Some(text);
                    }
                    continue;
                }
                if items.len() >= per_section_limit {
                    break;
                }
                if text.chars().count() < 4 || UTILITY_TEXTS.contains(&text.as_str()) {
                    continue;
                }
                seen.push(href.to_string());
                // Fallback: a body paragraph inside the item's card
                // (the web branch's technique), bounded by the rule
                // that the card holds no other item's link.
                let snippet = card_snippet(a, href);
                items.push(IntegratedItem {
                    title: text,
                    url: href.to_string(),
                    snippet,
                });
            }
        }
        if !items.is_empty() {
            sections.push(IntegratedSection {
                name: section_name(section),
                items,
            });
        }
    }
    if sections.is_empty() {
        return Err(layout_error("integrated"));
    }
    Ok(sections)
}

impl NaverSearchClient {
    /// Run an integrated search and return the subject sections in page
    /// order. `limit` caps the items kept per section (clamped to 10).
    pub async fn search(&self, query: &str, limit: i64) -> Result<IntegratedResult> {
        let limit = clamp_limit(limit, 10);
        let url = build_page_url(query, "integrated", 1)?;
        let html = self.get_html(&url, SEARCH_REFERER).await?;
        let sections = parse_integrated_results(&html, limit)?;
        let count = sections.iter().map(|s| s.items.len()).sum();
        Ok(IntegratedResult {
            provider: crate::PROVIDER.to_string(),
            vertical: "integrated".to_string(),
            ok: true,
            query: query.trim().to_string(),
            page: 1,
            count,
            sections,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sections_with_headings() {
        let html = r#"<html><body>
            <div class="api_subject_bx"><h2>뉴스</h2>
              <a href="https://n.news.naver.com/article/001/0000000001">뉴스 기사 제목입니다</a>
              <a href="https://help.naver.com/x">자세히 보기</a>
            </div>
            <div class="api_subject_bx"><h2>웹문서</h2>
              <a href="https://example.com/page" data-nlog-area="x.w.link">예제 페이지 제목</a>
            </div>
            </body></html>"#;
        let sections = parse_integrated_results(html, 10).unwrap();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "뉴스");
        assert_eq!(sections[0].items.len(), 1);
        assert_eq!(sections[1].items[0].url, "https://example.com/page");
    }

    #[test]
    fn generic_branch_collects_snippets() {
        // News-style card: the same URL is linked from the title and
        // from the body text; the body text becomes the snippet.
        // Blog-style card: the body is a <p> inside the item's card.
        let html = r#"<html><body>
            <div class="api_subject_bx"><h2>뉴스</h2>
              <a href="https://n.news.naver.com/article/001/0000000002">기사 제목입니다</a>
              <a href="https://n.news.naver.com/article/001/0000000002">이것은 기사의 본문 미리보기 텍스트로 충분히 긴 문장입니다</a>
            </div>
            <div class="api_subject_bx"><h2>블로그</h2>
              <div class="card">
                <a href="https://blog.naver.com/x/123">블로그 글 제목</a>
                <p>블로그 본문의 미리보기 문장으로 길이가 충분한 텍스트입니다</p>
              </div>
            </div>
            </body></html>"#;
        let sections = parse_integrated_results(html, 10).unwrap();
        assert_eq!(sections.len(), 2);
        let news_snippet = sections[0].items[0].snippet.as_deref().unwrap();
        assert!(news_snippet.contains("본문 미리보기"));
        let blog_snippet = sections[1].items[0].snippet.as_deref().unwrap();
        assert!(blog_snippet.contains("블로그 본문"));
    }

    #[test]
    fn parse_empty_shell_is_a_layout_error() {
        assert!(parse_integrated_results("<html><body></body></html>", 10).is_err());
    }
}
