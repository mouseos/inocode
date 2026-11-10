//! Yandex image search (`https://yandex.ru/images/touch/search`).
//!
//! Results are not in the rendered cards but in the app's initial state: a
//! JSON document in a `data-state` attribute, at
//! `initialState.serpList.items` — an ordered `keys` array plus an
//! `entities` map. Each entity carries its thumbnail (`viewerData.thumb`),
//! the original image (`origUrl`, `origWidth`/`origHeight`) and the source
//! page (`snippet.url` / `snippet.domain`).
//!
//! The request must use the tab-link form (`text` + `source=tabbar`). The
//! `rpt=imageview` form opens the search-by-image (CBIR) landing and returns
//! no text results.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::urlbuild::build_search_url;
use crate::{SearchResult, YandexSearchClient};

/// Yandex ships 20 image results per touch page.
const PER_PAGE: i64 = 20;

/// One image result: the original image, a Yandex thumbnail, pixel size and
/// the page the image appears on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageItem {
    pub rank: i64,
    pub title: String,
    pub original_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail_height: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_site: Option<String>,
}

/// The search URL the client requests for `page` (1-based; Yandex pages
/// from 0 with `p`, as on the web vertical).
pub fn image_search_url(query: &str, page: i64) -> String {
    let mut url = build_search_url(query, "image").expect("image URL always builds");
    if page > 1 {
        url.push_str(&format!("&p={}", page - 1));
    }
    url
}

/// Parse image results out of a saved touch image-search page.
pub fn parse_image_results(html: &str, limit: usize) -> Result<Vec<ImageItem>> {
    let state = extract_data_state(html)?;
    let items_node = get_path(&state, &["initialState", "serpList", "items"])
        .or_else(|| get_path(&state, &["serpList", "items"]))
        .ok_or_else(|| {
            anyhow!(
                "serpList.items not found in initial state (Yandex page layout may have changed)"
            )
        })?;
    let keys = items_node
        .get("keys")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            anyhow!("serpList.items.keys missing (Yandex page layout may have changed)")
        })?;
    let entities = items_node.get("entities").cloned().unwrap_or(Value::Null);

    let mut items = Vec::new();
    for key in keys {
        let Some(entity) = key.as_str().and_then(|k| entities.get(k)) else {
            continue;
        };
        if let Some(item) = parse_entity(entity, items.len() as i64 + 1) {
            items.push(item);
            if items.len() >= limit {
                break;
            }
        }
    }
    Ok(items)
}

fn parse_entity(entity: &Value, rank: i64) -> Option<ImageItem> {
    let snippet = entity.get("snippet").cloned().unwrap_or(Value::Null);
    let title = get_str(&snippet, "title")
        .or_else(|| get_str(entity, "alt"))
        .unwrap_or_default();
    let original_url = get_raw(entity, "origUrl")?;
    let thumb = get_path(entity, &["viewerData", "thumb"])
        .cloned()
        .unwrap_or(Value::Null);
    let thumbnail_url = get_raw(&thumb, "url")
        .map(|u| absolutize(&u))
        .or_else(|| get_raw(entity, "image").map(|u| absolutize(&u)));
    Some(ImageItem {
        rank: get_i64(entity, "pos").map(|p| p + 1).unwrap_or(rank),
        title,
        original_url,
        thumbnail_url,
        width: get_i64(entity, "origWidth").or_else(|| get_i64(entity, "width")),
        height: get_i64(entity, "origHeight").or_else(|| get_i64(entity, "height")),
        thumbnail_width: get_i64(&thumb, "w"),
        thumbnail_height: get_i64(&thumb, "h"),
        source_url: get_raw(&snippet, "url"),
        source_site: get_raw(&snippet, "domain"),
    })
}

impl YandexSearchClient {
    /// Search images. `page` is 1-based; `limit` is clamped to 20.
    /// Requires the session (established automatically from web page 1).
    pub async fn image_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<ImageItem>> {
        let page = page.max(1);
        let limit = clamp_limit(limit, PER_PAGE);
        let query = query.trim();

        self.ensure_session(query).await?;
        let html = self.get_page(&image_search_url(query, page)).await?;
        let mut items = parse_image_results(&html, usize::MAX)?;
        let total_found = items.len();
        items.truncate(limit);
        let mut result = SearchResult::new("image", query, page, items);
        if total_found >= PER_PAGE as usize {
            result = result.with_next_page(Some(page + 1));
        }
        if page > crate::VERIFIED_MAX_PAGE {
            result = result.with_note(crate::BEYOND_VERIFIED_NOTE);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_url_uses_tabbar_form() {
        assert_eq!(
            image_search_url("test", 1),
            "https://yandex.ru/images/touch/search?text=test&source=tabbar&ui=webmobileapp.yandex"
        );
        assert_eq!(
            image_search_url("test", 2),
            "https://yandex.ru/images/touch/search?text=test&source=tabbar&ui=webmobileapp.yandex&p=1"
        );
    }

    #[test]
    fn parse_entity_reads_measured_fields() {
        let entity = serde_json::json!({
            "pos": 0,
            "alt": "Alt text",
            "origUrl": "https://example.com/full.jpg",
            "origWidth": 1600,
            "origHeight": 862,
            "image": "//avatars.mds.yandex.net/i?id=x-images-thumbs&n=13",
            "snippet": {
                "title": "Snippet title",
                "domain": "example.com",
                "url": "https://example.com/page"
            },
            "viewerData": {"thumb": {"url": "//avatars.mds.yandex.net/i?id=x-images-thumbs&n=13", "w": 480, "h": 259}}
        });
        let item = parse_entity(&entity, 1).unwrap();
        assert_eq!(item.rank, 1);
        assert_eq!(item.title, "Snippet title");
        assert_eq!(item.original_url, "https://example.com/full.jpg");
        assert_eq!(
            item.thumbnail_url.as_deref(),
            Some("https://avatars.mds.yandex.net/i?id=x-images-thumbs&n=13")
        );
        assert_eq!(item.width, Some(1600));
        assert_eq!(item.thumbnail_width, Some(480));
        assert_eq!(item.source_site.as_deref(), Some("example.com"));
    }
}
