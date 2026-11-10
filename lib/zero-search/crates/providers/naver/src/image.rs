//! Naver image search (`ssc=tab.m_image.all&where=m_image`).
//!
//! The image tab does not render results as DOM — it embeds them as a
//! JSON array inside `var imageSearchTabData = { content: { items: [...] } }`
//! in a script tag. Each entry carries the original image URL
//! (`originalUrl`), the source page (`link`), a title and Naver proxy
//! thumbnails (`thumb`, built from `search.pstatic.net/common/?src=...`).
//! When `originalUrl` is absent the original is recovered by decoding
//! the proxy's `src` parameter. Paging follows the page-issued
//! more-API chain (see the `more` module): the API's items have the
//! same shape as the tab payload's.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::*;
use crate::urlbuild::build_page_url;
use crate::{NaverSearchClient, SearchResult};

/// One image result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageItem {
    pub rank: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The original image URL (not the Naver proxy).
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    /// Page the image appears on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_page: Option<String>,
    /// Source label as Naver shows it (e.g. `네이버 블로그`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<i64>,
}

fn json_str(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

fn json_i64(v: &Value, key: &str) -> Option<i64> {
    match v.get(key) {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse::<i64>().ok(),
        _ => None,
    }
}

/// Extract the `imageSearchTabData.content.items` JSON array from a saved
/// image tab page. The array is located by bracket matching (string- and
/// escape-aware) because the surrounding object is a JS literal, not JSON.
pub fn extract_image_items(html: &str) -> Result<Vec<Value>> {
    let Some(marker) = html.find("imageSearchTabData") else {
        return Err(layout_error("image"));
    };
    let rest = &html[marker..];
    let Some(rel) = rest.find("items:") else {
        return Err(layout_error("image"));
    };
    let rest = &rest[rel + "items:".len()..];
    let Some(start) = rest.find('[') else {
        return Err(layout_error("image"));
    };
    let bytes = rest.as_bytes();
    let mut depth = 0i64;
    let mut in_string = false;
    let mut escaped = false;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Err(layout_error("image"));
    };
    let array_text = &rest[start..=end];
    let value: Value = serde_json::from_str(array_text)
        .map_err(|e| anyhow::anyhow!("parse_error: image items JSON did not parse: {e}"))?;
    Ok(value.as_array().cloned().unwrap_or_default())
}

/// Recover the original image URL from a Naver proxy thumbnail URL
/// (`.../common/?src=<percent-encoded original>&type=...`).
pub fn original_from_proxy(proxy_url: &str) -> Option<String> {
    query_param(proxy_url, "src").filter(|s| s.starts_with("http"))
}

/// Map raw image item objects (tab payload or more-API payload — the
/// shape is the same) to typed items. Non-image entries (keyword
/// chips etc.) and entries with no recoverable original URL are
/// skipped.
pub fn map_image_items(raw_items: &[Value], limit: usize) -> Vec<ImageItem> {
    let mut items = Vec::new();
    for raw in raw_items {
        if items.len() >= limit {
            break;
        }
        if raw.get("type").and_then(|t| t.as_str()) != Some("image") {
            continue;
        }
        let thumbnail = json_str(raw, "thumb")
            .or_else(|| json_str(raw, "viewerThumb"))
            .or_else(|| json_str(raw, "thumb280"));
        let url = json_str(raw, "originalUrl")
            .or_else(|| thumbnail.as_deref().and_then(original_from_proxy));
        let Some(url) = url else { continue };
        items.push(ImageItem {
            rank: json_i64(raw, "rank").unwrap_or(items.len() as i64 + 1),
            title: json_str(raw, "title"),
            url,
            thumbnail,
            source_page: json_str(raw, "link"),
            source: json_str(raw, "source"),
            width: json_i64(raw, "orgWidth"),
            height: json_i64(raw, "orgHeight"),
        });
    }
    items
}

/// Parse the embedded image items of a saved image tab page.
pub fn parse_image_results(html: &str, limit: usize) -> Result<Vec<ImageItem>> {
    let raw_items = extract_image_items(html)?;
    let items = map_image_items(&raw_items, limit);
    if items.is_empty() {
        return Err(layout_error("image"));
    }
    Ok(items)
}

impl NaverSearchClient {
    /// Search images. Page 1 is the tab page's batch; deeper pages
    /// follow the page-issued more-API chain (see `more`), up to
    /// [`MAX_PAGE`](crate::MAX_PAGE). `limit` is clamped to 100.
    pub async fn image_search(
        &self,
        query: &str,
        page: i64,
        limit: i64,
    ) -> Result<SearchResult<ImageItem>> {
        let page = page.max(1);
        if page > crate::more::MAX_PAGE {
            return Err(crate::common::unsupported_page("image", page));
        }
        let limit = clamp_limit(limit, 100);
        let url = build_page_url(query, "image", 1)?;
        let html = self.get_html(&url, SEARCH_REFERER).await?;
        let items = crate::more::chained_items(
            self,
            "image",
            &html,
            page,
            |h| parse_image_results(h, usize::MAX),
            |payload| match payload {
                crate::more::MorePayload::ImageItems(raw) => Ok(map_image_items(&raw, usize::MAX)),
                _ => Err(layout_error("image")),
            },
            |item, rank| item.rank = rank,
        )
        .await?;
        let items: Vec<ImageItem> = items.into_iter().take(limit).collect();
        Ok(SearchResult::new("image", query, page, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_and_parse_minimal_items() {
        let html = r#"<script>var imageSearchTabData = {
            content: { url: "https://s.search.naver.com/x", items: [
                {"type":"image","rank":1,"title":"고양이","link":"https://m.blog.naver.com/a/1",
                 "source":"네이버 블로그","originalUrl":"http://blogfiles.naver.net/a.jpg",
                 "thumb":"https://search.pstatic.net/common/?src=http%3A%2F%2Fblogfiles.naver.net%2Fa.jpg&type=f54_54",
                 "orgWidth":800,"orgHeight":600},
                {"type":"keyword","title":"관련"},
                {"type":"image","rank":2,"title":"둘째",
                 "thumb":"https://search.pstatic.net/common/?src=http%3A%2F%2Fexample.com%2Fb.png&type=f54_54"}
            ] }
        };</script>"#;
        let items = parse_image_results(html, 10).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].url, "http://blogfiles.naver.net/a.jpg");
        assert_eq!(items[0].width, Some(800));
        // No originalUrl: recovered from the proxy `src` parameter.
        assert_eq!(items[1].url, "http://example.com/b.png");
    }

    #[test]
    fn missing_payload_is_a_layout_error() {
        assert!(parse_image_results("<html><body>x</body></html>", 10).is_err());
    }
}
