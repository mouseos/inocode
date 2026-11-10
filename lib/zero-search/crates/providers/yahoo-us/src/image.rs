//! Yahoo (US) image search (`https://images.search.yahoo.com/search/images`).
//!
//! Results are server-rendered tiles (`ul.image-results__list > li.tile`),
//! laid out exactly like the video vertical: the real source-page URL is
//! the tile link's `data-referenceurl` attribute, the original image URL
//! is `data-origurl`, and totals live in the page's `resultsMeta` JSON
//! blob.
//!
//! The catch is transport. Yahoo's image host answers plain HTTP clients
//! with HTTP 500 and an empty body — under every condition tested (UA,
//! cookies, Yahoo's own SERP-issued pivot URL, US and Japanese egress)
//! — while a real Firefox on the same connection renders the results
//! fine. The edge is discriminating on the request fingerprint below the
//! application layer (TLS handshake / HTTP/2 behaviour). This vertical's
//! pages are therefore fetched by the bundled helper process running a
//! browser-grade client (wreq with a Firefox emulation profile): same
//! URL, same server-rendered page, no browser driven. The helper is a
//! separate binary because its TLS stack (BoringSSL) cannot link into
//! the server alongside OpenSSL — see [`crate::helper`]. All other
//! verticals keep using the shared reqwest client.

use anyhow::{anyhow, Result};
use regex::Regex;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::{SearchResult, YahooUsSearchClient};

/// Yahoo returns 60 image tiles per page and paginates `b` by that step
/// (the page's `resultsMeta` reports `numResults: "60"` and, for the
/// first page, `nextOffset: "60"` — the same convention the video
/// vertical verified live).
pub const PER_PAGE: i64 = 60;

static TILE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("ul.image-results__list > li.tile").unwrap());
static LINK_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("a.media-tile").unwrap());
static IMG_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("img.tile-image").unwrap());
static DOMAIN_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".tile-domain").unwrap());
static TITLE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".tile-title").unwrap());
static DETAIL_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".tile-more-detail").unwrap());

static TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""resultsMeta":\{"totalResults":"(\d+)""#).unwrap());
static NEXT_OFFSET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""nextOffset":"(\d+)""#).unwrap());
static DETAIL_DIMS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)x(\d+)$").unwrap());

/// One image result: title, the page the image appears on, the original
/// image itself, and display metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageItem {
    pub rank: i64,
    pub title: String,
    /// URL of the page the image was found on (`data-referenceurl`).
    pub url: String,
    /// URL of the original image file (`data-origurl`).
    pub image_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
}

impl YahooUsSearchClient {
    /// Search images. `page` is 1-based; each page holds [`PER_PAGE`]
    /// tiles. `total_hits` comes from the page's `resultsMeta` blob.
    ///
    /// Unlike the other verticals this goes through the helper
    /// process (see the module docs): Yahoo's image host refuses
    /// plain HTTP clients with HTTP 500.
    pub async fn image_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<ImageItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);

        let url = crate::build_search_url(query, "image", page)?;
        self.fetch_image_page(query, page, limit, &url).await
    }

    /// Search images with the SERP's own filters applied (audit
    /// W-28; see [`crate::ImageFilters`] for the measured value
    /// sets). Invalid filter values are an error before any request
    /// is made.
    pub async fn image_search_filtered(
        &self,
        query: &str,
        page: i64,
        limit: i64,
        filters: &crate::ImageFilters,
    ) -> Result<SearchResult<ImageItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);
        let url = crate::build_image_search_url(query, page, filters)?;
        self.fetch_image_page(query, page, limit, &url).await
    }

    async fn fetch_image_page(
        &self,
        query: &str,
        page: i64,
        limit: usize,
        url: &str,
    ) -> Result<SearchResult<ImageItem>> {
        let html = self.get_image_html(url).await?;
        if !html.contains("image-results") && parse_image_total(&html).is_none() {
            return Err(anyhow!(
                "image results container not found (Yahoo page layout may have changed)"
            ));
        }

        let base_rank = (page - 1) * PER_PAGE;
        let mut items = parse_image_results(&html, limit);
        for item in &mut items {
            item.rank += base_rank;
        }
        let total = parse_image_total(&html);
        let has_next = match (parse_image_next_offset(&html), total) {
            (Some(next), Some(total)) => next > 0 && next < total,
            (Some(next), None) => next > 0,
            _ => false,
        };

        Ok(SearchResult::new("image", query, page, items)
            .with_total(total)
            .with_next_page(has_next.then_some(page + 1))
            .with_related(parse_related_queries(&html)))
    }
}

/// Parse image tiles out of a saved image SERP page. Offline.
pub fn parse_image_results(html: &str, limit: usize) -> Vec<ImageItem> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for tile in doc.select(&TILE_SEL) {
        if items.len() >= limit {
            break;
        }
        let Some(link) = tile.select(&LINK_SEL).next() else {
            continue;
        };
        let Some(url) = link
            .value()
            .attr("data-referenceurl")
            .map(decode_entities)
            .filter(|u| u.starts_with("http"))
        else {
            continue;
        };
        let image_url = link
            .value()
            .attr("data-origurl")
            .map(decode_entities)
            .filter(|u| u.starts_with("http"))
            .or_else(|| image_url_from_href(link.value().attr("href")?));
        let Some(image_url) = image_url else {
            continue;
        };
        let img = tile.select(&IMG_SEL).next();
        let title = tile
            .select(&TITLE_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| url.clone());
        let thumbnail = img
            .and_then(|el| el.value().attr("src"))
            .map(decode_entities)
            .filter(|s| !s.is_empty());
        let source = tile
            .select(&DOMAIN_SEL)
            .next()
            .map(|el| element_text(&el))
            .filter(|s| !s.is_empty());
        let (width, height) = img
            .and_then(|el| el.value().attr("data-meta"))
            .and_then(|m| serde_json::from_str::<serde_json::Value>(&decode_entities(m)).ok())
            .map(|v| {
                (
                    v.get("ow").and_then(serde_json::Value::as_i64),
                    v.get("oh").and_then(serde_json::Value::as_i64),
                )
            })
            .filter(|(w, h)| w.is_some() || h.is_some())
            .unwrap_or_else(|| {
                tile.select(&DETAIL_SEL)
                    .next()
                    .map(|el| element_text(&el))
                    .and_then(|t| {
                        DETAIL_DIMS_RE
                            .captures(&t)
                            .map(|c| (c[1].parse::<i64>().ok(), c[2].parse::<i64>().ok()))
                    })
                    .unwrap_or((None, None))
            });
        items.push(ImageItem {
            rank: items.len() as i64 + 1,
            title,
            url,
            image_url,
            thumbnail,
            source,
            width,
            height,
        });
    }
    items
}

/// Recover the original image URL from a tile link's `href` (the Yahoo
/// viewer URL carries it as the `imgurl` query parameter) when the
/// `data-origurl` attribute is absent.
fn image_url_from_href(href: &str) -> Option<String> {
    let decoded = decode_entities(href);
    let start = decoded.find("imgurl=")? + "imgurl=".len();
    let rest = &decoded[start..];
    let end = rest.find('&').unwrap_or(rest.len());
    let url = urldecode(&rest[..end], false);
    url.starts_with("http").then_some(url)
}

/// Total result count from the page's `resultsMeta` blob, if present.
pub fn parse_image_total(html: &str) -> Option<i64> {
    TOTAL_RE.captures(html).and_then(|c| c[1].parse().ok())
}

/// Offset of the next page from `resultsMeta` (`0` when there is none).
pub fn parse_image_next_offset(html: &str) -> Option<i64> {
    NEXT_OFFSET_RE
        .captures(html)
        .and_then(|c| c[1].parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE: &str = include_str!("../tests/fixtures/fixture-image.html");

    #[test]
    fn parses_all_tiles_from_fixture() {
        let items = parse_image_results(PAGE, PER_PAGE as usize);
        assert_eq!(items.len(), 60, "fixture page should yield 60 tiles");
        for item in &items {
            assert!(item.url.starts_with("http"), "bad page url: {}", item.url);
            assert!(
                item.image_url.starts_with("http"),
                "bad image url: {}",
                item.image_url
            );
            assert!(
                !item.url.contains("images.search.yahoo.com"),
                "viewer URL leaked instead of reference URL: {}",
                item.url
            );
            assert!(!item.title.is_empty());
        }
    }

    #[test]
    fn first_tile_fields_are_populated() {
        let items = parse_image_results(PAGE, PER_PAGE as usize);
        let first = &items[0];
        assert_eq!(
            first.url,
            "https://www.archyde.com/ios-27-introduces-wi-fi-setup-renaming-and-enhanced-connectivity/"
        );
        assert_eq!(
            first.image_url,
            "https://media.cnn.com/api/v1/images/stellar/prod/111006055359-apple-logo-new-york.jpg?q=x_3,y_158,h_1684,w_2994,c_crop"
        );
        assert_eq!(first.source.as_deref(), Some("www.archyde.com"));
        assert_eq!(first.width, Some(2994));
        assert_eq!(first.height, Some(1684));
        assert!(first.thumbnail.is_some());
    }

    #[test]
    fn totals_come_from_results_meta() {
        assert_eq!(parse_image_total(PAGE), Some(536));
        assert_eq!(parse_image_next_offset(PAGE), Some(60));
        assert_eq!(parse_image_total("<html></html>"), None);
    }

    #[test]
    fn limit_is_respected() {
        assert_eq!(parse_image_results(PAGE, 5).len(), 5);
    }

    #[test]
    fn href_fallback_recovers_imgurl() {
        let href = "https://images.search.yahoo.com/search/images;_ylt=X?p=apple&amp;imgurl=https%3A%2F%2Fexample.com%2Fa.jpg%3Fq%3D1&amp;fr2=piv-web";
        assert_eq!(
            image_url_from_href(href).as_deref(),
            Some("https://example.com/a.jpg?q=1")
        );
        assert_eq!(image_url_from_href("https://example.com/"), None);
    }

    /// Live check (network): the browser-grade client must get real
    /// tiles out of Yahoo's image host, which refuses plain HTTP.
    #[tokio::test]
    #[ignore = "live network test"]
    async fn live_image_search_returns_items() {
        let client = YahooUsSearchClient::new();
        let result = client.image_search("apple", 1, 5).await.unwrap();
        assert!(result.ok);
        assert_eq!(result.count, 5);
        assert!(result.items.iter().all(|i| i.image_url.starts_with("http")));
        assert!(result.total_hits.unwrap_or(0) > 60);
    }
}
