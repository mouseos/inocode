//! Yandex web search (`https://yandex.ru/search/touch/`).
//!
//! The touch SERP is server-rendered HTML. Each result card is a
//! `div.serp-item`; the ordinary (organic) results are the cards whose inner
//! `div.Organic` carries an `a.OrganicTitle-Link`. Cards with a
//! `data-fast-name` (the AI-answer card, the `adv_unisearch` ad block, …) and
//! ads labelled `Organic_withAdvLabel` are not organic results: the organic
//! parser skips them, and [`parse_special_cards`] surfaces them as their own
//! kind (audit W-37). Title links that point at Yandex click counters are
//! never treated as destination pages.

use anyhow::Result;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::LazyLock;

use crate::common::*;
use crate::urlbuild::build_search_url;
use crate::{SearchResult, YandexSearchClient};

/// Yandex returns ~10 organic results per touch page.
const PER_PAGE: i64 = 10;

static TITLE_LINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"OrganicTitle-Link[^>]*href="([^"]+)""#).unwrap());
static TITLE_TEXT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)OrganicTitle-LinkText[^>]*>(.*?)</h2>"#).unwrap());
static PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)Path-Item[^>]*>(.*?)</span>"#).unwrap());
static SNIPPET_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)OrganicText[^>]*>(.*?)</div>"#).unwrap());
static APPROX_TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"нашлось\s+([0-9][0-9\s]*(?:тыс\.|млн)?)\s*результатов").unwrap());

/// One organic web result: rank, title, destination URL, the displayed
/// breadcrumb path and the snippet text.
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

/// The search URL the client requests for `page` (1-based). Yandex pages
/// with `p`, counted from 0.
pub fn web_search_url(query: &str, page: i64) -> String {
    let mut url = build_search_url(query, "web").expect("web URL always builds");
    if page > 1 {
        url.push_str(&format!("&p={}", page - 1));
    }
    url
}

/// Parse organic results out of a saved touch SERP page.
pub fn parse_web_results(html: &str, limit: usize) -> Vec<WebItem> {
    let starts: Vec<usize> = html
        .match_indices("<div class=\"serp-item")
        .map(|(i, _)| i)
        .collect();
    let mut items = Vec::new();
    for (idx, &start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(html.len());
        let block = &html[start..end];
        if let Some(item) = parse_card(block) {
            items.push(item);
            if items.len() >= limit {
                break;
            }
        }
    }
    for (i, item) in items.iter_mut().enumerate() {
        item.rank = (i + 1) as i64;
    }
    items
}

fn parse_card(block: &str) -> Option<WebItem> {
    // Special cards (AI answer, ad blocks, …) carry a data-fast-name on the
    // serp-item opening tag; labelled ads carry Organic_withAdvLabel.
    let tag_end = block.find('>')?;
    if block[..tag_end].contains("data-fast-name") || block.contains("Organic_withAdvLabel") {
        return None;
    }
    let href = TITLE_LINK_RE.captures(block)?.get(1)?.as_str();
    let url = decode_entities(href);
    // Ad cards that slip past the markers link to click counters, never to a
    // destination page.
    if url.contains("/an/count/") || url.contains("yabs.yandex.ru/count") {
        return None;
    }
    let title = TITLE_TEXT_RE
        .captures(block)
        .map(|c| clean_text(&c[1]))
        .filter(|t| !t.is_empty())?;
    let site = PATH_RE.captures(block).map(|c| clean_text(&c[1]));
    let description = SNIPPET_RE
        .captures(block)
        .map(|c| clean_text(&c[1]))
        .filter(|t| !t.is_empty());
    Some(WebItem {
        rank: 0,
        title,
        url,
        description,
        site,
    })
}

/// A non-organic card on the web SERP (audit W-37): the AI-answer
/// card (`neuro_answer`), ad blocks (`adv_unisearch`, or a labelled
/// `Organic_withAdvLabel` card, reported as kind `ad`), the
/// related-searches card (`related`) and any other
/// `data-fast-name` card. These are not organic results, so
/// [`parse_web_results`] excludes them; this type surfaces them as
/// their own kind instead of dropping them silently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecialCard {
    /// The card's `data-fast-name` value, or `ad` for a labelled ad
    /// card without one.
    pub kind: String,
    /// The card's `data-fast-subtype`, when it carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The card's visible text (tags stripped), capped at 1000
    /// characters — for the AI-answer card this is the answer text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// First non-counter destination link in the card, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

static FAST_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-fast-name="([^"]+)""#).unwrap());
static FAST_SUBTYPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-fast-subtype="([^"]+)""#).unwrap());
static HREF_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"href="([^"]+)""#).unwrap());

/// Parse the non-organic cards out of a saved touch SERP page, in
/// page order (audit W-37).
pub fn parse_special_cards(html: &str) -> Vec<SpecialCard> {
    let starts: Vec<usize> = html
        .match_indices("<div class=\"serp-item")
        .map(|(i, _)| i)
        .collect();
    let mut cards = Vec::new();
    for (idx, &start) in starts.iter().enumerate() {
        let end = starts.get(idx + 1).copied().unwrap_or(html.len());
        let block = &html[start..end];
        let tag_end = block.find('>').unwrap_or(block.len());
        let opening = &block[..tag_end];
        let fast_name = FAST_NAME_RE.captures(opening).map(|c| c[1].to_string());
        let is_labelled_ad = block.contains("Organic_withAdvLabel");
        if fast_name.is_none() && !is_labelled_ad {
            continue;
        }
        let kind = fast_name.unwrap_or_else(|| "ad".to_string());
        let subtype = FAST_SUBTYPE_RE.captures(opening).map(|c| c[1].to_string());
        let title = TITLE_TEXT_RE
            .captures(block)
            .map(|c| clean_text(&c[1]))
            .filter(|t| !t.is_empty());
        let text = {
            let cleaned = clean_text(block);
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned.chars().take(1000).collect())
            }
        };
        let url = HREF_RE.captures_iter(block).find_map(|c| {
            let url = decode_entities(&c[1]);
            if url.starts_with("http")
                && !url.contains("/an/count/")
                && !url.contains("yabs.yandex.ru/count")
            {
                Some(url)
            } else {
                None
            }
        });
        cards.push(SpecialCard {
            kind,
            subtype,
            title,
            text,
            url,
        });
    }
    cards
}

/// A web search page in full: the organic results envelope plus the
/// page's non-organic cards (audit W-37).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchPage {
    pub result: SearchResult<WebItem>,
    pub special_cards: Vec<SpecialCard>,
}

/// The approximate hit count Yandex prints on the page («нашлось 195 тыс.
/// результатов»), as a display string. It is an estimate, which is why the
/// envelope surfaces it as a note instead of `total_hits`.
pub fn approx_total_text(html: &str) -> Option<String> {
    APPROX_TOTAL_RE
        .captures(html)
        .map(|c| format!("нашлось {} результатов", c[1].trim()))
}

impl YandexSearchClient {
    /// Search the web index. `page` is 1-based; `limit` is clamped to 10.
    ///
    /// Page 1 doubles as the session-establishing request; deeper pages are
    /// fetched only after the session exists (without one, Yandex answers
    /// page 2+ with a captcha redirect).
    pub async fn web_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<WebItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);
        let query = query.trim();

        let html = if page == 1 {
            let body = self.get_page(&web_search_url(query, 1)).await?;
            self.mark_session_started().await;
            body
        } else {
            self.ensure_session(query).await?;
            self.get_page(&web_search_url(query, page)).await?
        };

        let mut items = parse_web_results(&html, usize::MAX);
        let total_found = items.len();
        items.truncate(limit);
        let mut result = SearchResult::new("web", query, page, items);
        if total_found >= PER_PAGE as usize {
            result = result.with_next_page(Some(page + 1));
        }
        let mut notes: Vec<String> = Vec::new();
        if let Some(text) = approx_total_text(&html) {
            notes.push(format!(
                "Yandex reports approximately: {text} (estimate printed on the page; not exposed as total_hits)"
            ));
        }
        if page > crate::VERIFIED_MAX_PAGE {
            notes.push(crate::BEYOND_VERIFIED_NOTE.to_string());
        }
        if !notes.is_empty() {
            result = result.with_note(&notes.join(" "));
        }
        Ok(result)
    }

    /// Like [`Self::web_search`], but also returns the page's
    /// non-organic cards (AI answer, ad blocks, related searches)
    /// that the organic parser sets aside (audit W-37).
    pub async fn web_search_detailed(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<WebSearchPage> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);
        let query = query.trim();

        let html = if page == 1 {
            let body = self.get_page(&web_search_url(query, 1)).await?;
            self.mark_session_started().await;
            body
        } else {
            self.ensure_session(query).await?;
            self.get_page(&web_search_url(query, page)).await?
        };

        let special_cards = parse_special_cards(&html);
        let mut items = parse_web_results(&html, usize::MAX);
        let total_found = items.len();
        items.truncate(limit);
        let mut result = SearchResult::new("web", query, page, items);
        if total_found >= PER_PAGE as usize {
            result = result.with_next_page(Some(page + 1));
        }
        let mut notes: Vec<String> = Vec::new();
        if let Some(text) = approx_total_text(&html) {
            notes.push(format!(
                "Yandex reports approximately: {text} (estimate printed on the page; not exposed as total_hits)"
            ));
        }
        if page > crate::VERIFIED_MAX_PAGE {
            notes.push(crate::BEYOND_VERIFIED_NOTE.to_string());
        }
        if !notes.is_empty() {
            result = result.with_note(&notes.join(" "));
        }
        Ok(WebSearchPage {
            result,
            special_cards,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_url_pages_from_zero() {
        assert_eq!(
            web_search_url("test", 1),
            "https://yandex.ru/search/touch/?text=test&ui=webmobileapp.yandex"
        );
        assert_eq!(
            web_search_url("test", 2),
            "https://yandex.ru/search/touch/?text=test&ui=webmobileapp.yandex&p=1"
        );
        assert_eq!(
            web_search_url("test", 3),
            "https://yandex.ru/search/touch/?text=test&ui=webmobileapp.yandex&p=2"
        );
    }

    #[test]
    fn parse_card_skips_special_and_ad_cards() {
        let ai =
            r#"<div class="serp-item" data-fast-name="neuro_answer"><div class="Futuris">x</div>"#;
        assert!(parse_card(ai).is_none());
        let ad = r#"<div class="serp-item"><div class="Organic Organic_withAdvLabel"><a class="OrganicTitle-Link" href="https://yabs.yandex.ru/count/1"><h2 class="OrganicTitle-LinkText">Ad</h2></a></div>"#;
        assert!(parse_card(ad).is_none());
        let counter = r#"<div class="serp-item"><div class="Organic"><a class="OrganicTitle-Link" href="https://yandex.ru/an/count/1"><h2 class="OrganicTitle-LinkText">Ad</h2></a></div>"#;
        assert!(parse_card(counter).is_none());
    }

    #[test]
    fn special_cards_are_surfaced_with_their_kind() {
        let html = r#"<div class="serp-item" data-fast-name="neuro_answer" data-fast-subtype="offline_bsa"><div>Быстрый ответ Алисы AI</div><div>Текст ответа здесь</div></div><div class="serp-item"><div class="Organic Organic_withAdvLabel"><a class="OrganicTitle-Link" href="https://yabs.yandex.ru/count/1"><h2 class="OrganicTitle-LinkText">Ad Title</h2></a></div></div><div class="serp-item"><div class="Organic"><a class="OrganicTitle-Link" href="https://example.com/"><h2 class="OrganicTitle-LinkText">Organic</h2></a></div></div>"#;
        let cards = parse_special_cards(html);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].kind, "neuro_answer");
        assert_eq!(cards[0].subtype.as_deref(), Some("offline_bsa"));
        assert!(cards[0].text.as_deref().unwrap().contains("Текст ответа"));
        assert_eq!(cards[1].kind, "ad");
        assert_eq!(cards[1].title.as_deref(), Some("Ad Title"));
        assert!(cards[1].url.is_none(), "counter links are not destinations");
    }

    #[test]
    fn parse_card_reads_organic_fields() {
        let card = r#"<div class="serp-item"><div class="Organic"><a class="OrganicTitle-Link" href="https://example.com/page"><h2 class="OrganicTitle-LinkText"><span>Example <b>Test</b></span></h2><div class="Path Organic-Path"><span class="Path-Item"><b>example.com</b> › page</span></div></a><div class="TextContainer OrganicText"><span>Snippet <b>text</b> here</span></div></div>"#;
        let item = parse_card(card).unwrap();
        assert_eq!(item.title, "Example Test");
        assert_eq!(item.url, "https://example.com/page");
        assert_eq!(item.site.as_deref(), Some("example.com › page"));
        assert_eq!(item.description.as_deref(), Some("Snippet text here"));
    }
}
