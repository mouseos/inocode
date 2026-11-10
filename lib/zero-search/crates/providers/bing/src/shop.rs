//! Bing shopping search (`https://www.bing.com/shop/search`),
//! the vertical the app's deep links point at (round1 report §4.2;
//! audit W-15).
//!
//! The page is server-rendered: each product is an `li.br-item`
//! carrying `data-url` (the Bing entity-details path),
//! `data-offerId` and `data-index`; inside, the product name is in
//! `.br-pdItemName` (the main image's `alt` holds the untruncated
//! name), the price in `.pd-price` and the seller in
//! `.br-sellersCite`. Structure measured live 2026-11-08 against
//! the saved fixture `fixture-shop-ja.html` (26 cards).
//!
//! Paging: page 1 links only `first=1`, so no next-page link
//! scheme is observable from it; `has_next_page` is derived from
//! `first=N` links strictly greater than the current page's, and
//! is therefore `false` until a deeper page proves otherwise.

use anyhow::Result;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{BingSearchClient, SearchResult};

static CARD_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse("li.br-item").unwrap());
static NAME_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".br-pdItemName").unwrap());
static PRICE_SEL: LazyLock<Selector> = LazyLock::new(|| Selector::parse(".pd-price").unwrap());
static SELLER_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".br-sellersCite").unwrap());
static IMG_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse(".br-pdMainImg img").unwrap());

/// One shopping result (a single seller's offer card).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShopItem {
    pub rank: i64,
    pub title: String,
    /// Price as displayed (e.g. `¥290,180`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_text: Option<String>,
    /// Numeric price parsed from `price_text`, when it is a plain
    /// amount (currency symbol and grouping commas stripped).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_value: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seller: Option<String>,
    /// Bing entity-details URL for this offer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offer_id: Option<String>,
}

/// Parsed shopping results page.
#[derive(Debug, Clone)]
pub struct ShopResults {
    pub items: Vec<ShopItem>,
    pub has_next_page: bool,
}

fn element_text(el: scraper::ElementRef) -> String {
    clean_text(&el.text().collect::<Vec<_>>().join(" "))
}

fn parse_price_value(price_text: &str) -> Option<i64> {
    let digits: String = price_text.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Parse a saved Bing shopping results page (offline-testable).
pub fn parse_shop_results(html: &str, page: i64, limit: usize) -> Result<ShopResults> {
    let doc = Html::parse_document(html);
    let mut items = Vec::new();
    for card in doc.select(&CARD_SEL) {
        if items.len() >= limit {
            break;
        }
        let v = card.value();
        let img = card.select(&IMG_SEL).next();
        let title = card
            .select(&NAME_SEL)
            .next()
            .map(element_text)
            .filter(|s| !s.is_empty())
            .or_else(|| {
                img.and_then(|i| i.value().attr("alt"))
                    .map(clean_text)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let price_text = card
            .select(&PRICE_SEL)
            .next()
            .map(element_text)
            .filter(|s| !s.is_empty());
        items.push(ShopItem {
            rank: items.len() as i64 + 1,
            title,
            price_value: price_text.as_deref().and_then(parse_price_value),
            price_text,
            seller: card
                .select(&SELLER_SEL)
                .next()
                .map(element_text)
                .filter(|s| !s.is_empty()),
            url: v
                .attr("data-url")
                .map(|u| resolve_url(u, "https://www.bing.com")),
            image_url: img.and_then(|i| i.value().attr("src")).map(str::to_string),
            // The page spells it `data-offerId`; HTML parsing
            // normalises attribute names to lowercase.
            offer_id: v.attr("data-offerid").map(str::to_string),
        });
    }
    if items.is_empty() {
        return Err(layout_error("shopping"));
    }
    // Next-page evidence: a `first=N` link beyond this page's own
    // offset (the shared page-URL scheme advances in steps of 10).
    let current_first = (page.max(1) - 1) * 10 + 1;
    let has_next_page = html
        .match_indices("first=")
        .filter_map(|(i, _)| {
            html[i + 6..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<i64>()
                .ok()
        })
        .any(|n| n > current_first);
    Ok(ShopResults {
        items,
        has_next_page,
    })
}

impl BingSearchClient {
    /// Search Bing Shopping. `page` is 1-based; `limit` is clamped
    /// to 40 (a desktop page carries ~26 offer cards).
    pub async fn shopping_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
        mkt: &str,
        setlang: &str,
    ) -> Result<SearchResult<ShopItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, 40);
        let url = build_page_url(query, "shopping", page, mkt, setlang)?;
        let html = self.get_html(&url, "https://www.bing.com/").await?;
        let parsed = parse_shop_results(&html, page, limit)?;
        Ok(SearchResult::new("shopping", query, page, parsed.items)
            .with_next_page(parsed.has_next_page.then_some(page + 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_price_values() {
        assert_eq!(parse_price_value("¥290,180"), Some(290180));
        assert_eq!(parse_price_value("$1,299.00"), Some(129900));
        assert_eq!(parse_price_value("価格未定"), None);
    }

    #[test]
    fn parse_shell_is_a_layout_error() {
        assert!(parse_shop_results("<html><body></body></html>", 1, 10).is_err());
    }
}
