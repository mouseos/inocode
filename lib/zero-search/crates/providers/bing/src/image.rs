//! Bing image search (`https://www.bing.com/images/search`).
//!
//! Images are the one vertical where the desktop page is useless to a
//! scraper: it ships an empty shell and loads results with JavaScript.
//! Round 2 of the APK analysis found the working recipe — the page the
//! Bing app's WebView receives: a **mobile user agent** plus the app-style
//! parameters `form=L2GV01&sdkhh=1` (already present in the URLs from
//! [`crate::build_page_url`]) and the pinned `mkt`/`setlang`. That page
//! server-renders 12 results per page as `a.iusc` units whose `m`
//! attribute holds a JSON object with the original image URL (`murl`),
//! the thumbnail (`turl`), the page it appears on (`purl`), a title (`t`)
//! and a description (`desc`). This form has no total-count display, so
//! the result envelope carries no `total`.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::{build_page_url, IMAGE_PER_PAGE};
use crate::{BingSearchClient, SearchResult};

static UNIT_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.iusc").unwrap());

/// One image result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageItem {
    pub rank: i64,
    pub title: String,
    /// Original image URL (`murl`).
    pub url: String,
    /// Page the image appears on (`purl`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_url: Option<String>,
    /// Bing-hosted thumbnail URL (`turl`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Bing detail-view link for this image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail_url: Option<String>,
}

/// Parsed image results page.
#[derive(Debug, Clone)]
pub struct ImageResults {
    pub items: Vec<ImageItem>,
    pub has_next_page: bool,
}

/// Parse a saved Bing image results page (mobile/app-style form).
/// `page` is the 1-based page the HTML came from; it only affects the
/// next-page detection (the page links to `first = page * 12 + 1`).
pub fn parse_image_results(html: &str, page: i64, limit: usize) -> Result<ImageResults> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for unit in doc.select(&UNIT_SEL) {
        if items.len() >= limit {
            break;
        }
        let v = unit.value();
        let Some(m) = v.attr("m") else { continue };
        let Ok(meta) = serde_json::from_str::<serde_json::Value>(m) else {
            continue;
        };
        let get = |key: &str| {
            meta.get(key)
                .and_then(|x| x.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        let Some(murl) = get("murl") else { continue };
        items.push(ImageItem {
            rank: items.len() as i64 + 1,
            title: get("t").map(|t| clean_text(&t)).unwrap_or_default(),
            url: murl,
            page_url: get("purl"),
            thumbnail: get("turl"),
            description: get("desc")
                .map(|d| clean_text(&d))
                .filter(|s| !s.is_empty()),
            detail_url: v
                .attr("href")
                .map(|h| resolve_url(h, "https://www.bing.com")),
        });
    }
    if items.is_empty() {
        return Err(layout_error("image"));
    }
    let next_first = page.max(1) * IMAGE_PER_PAGE + 1;
    let has_next_page = html.contains(&format!("first={next_first}"));
    Ok(ImageResults {
        items,
        has_next_page,
    })
}

impl BingSearchClient {
    /// Search images. This is the only method that fetches with the
    /// mobile user agent — see the module docs. `page` is 1-based
    /// (12 results per page); `limit` is clamped to 12.
    pub async fn image_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
        mkt: &str,
        setlang: &str,
    ) -> Result<SearchResult<ImageItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, IMAGE_PER_PAGE);
        let url = build_page_url(query, "image", page, mkt, setlang)?;
        let html = self
            .get_html_mobile(&url, "https://www.bing.com/images")
            .await?;
        let parsed = parse_image_results(&html, page, limit)?;
        let mut items = parsed.items;
        for item in &mut items {
            item.rank += (page - 1) * IMAGE_PER_PAGE;
        }
        Ok(SearchResult::new("image", query, page, items)
            .with_next_page(parsed.has_next_page.then_some(page + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_desktop_shell_is_a_layout_error() {
        // The desktop form has no a.iusc units at all.
        let html = "<html><body><div class=\"dg_b\"></div></body></html>";
        assert!(parse_image_results(html, 1, 12).is_err());
    }

    /// Audit W-18: page 2 (`first=13`) was implemented from the
    /// in-page link alone; verify it live, once, on demand.
    #[tokio::test]
    #[ignore = "live network test"]
    async fn live_image_second_page() {
        let client = BingSearchClient::new().unwrap();
        let result = client
            .image_search("猫", 2, 12, "ja-JP", "ja")
            .await
            .unwrap();
        assert!(!result.items.is_empty(), "page 2 returned no items");
        assert!(
            result.items.iter().all(|i| i.rank > 12),
            "page 2 ranks must continue from page 1"
        );
    }

    #[test]
    fn parse_one_unit_from_m_json() {
        let html = r##"<html><body><a class="iusc" href="/images/search?view=detailV2" m="{&quot;murl&quot;:&quot;https://example.com/a.jpg&quot;,&quot;turl&quot;:&quot;https://ts1.mm.bing.net/th?id=OIP.x&amp;pid=15.1&quot;,&quot;purl&quot;:&quot;https://example.com/post&quot;,&quot;t&quot;:&quot;Title&quot;}">x</a><a href="/images/search?first=13">next</a></body></html>"##;
        let parsed = parse_image_results(html, 1, 12).unwrap();
        assert_eq!(parsed.items.len(), 1);
        assert_eq!(parsed.items[0].url, "https://example.com/a.jpg");
        assert_eq!(
            parsed.items[0].thumbnail.as_deref(),
            Some("https://ts1.mm.bing.net/th?id=OIP.x&pid=15.1")
        );
        assert!(parsed.has_next_page);
    }
}
