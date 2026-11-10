//! Paging through the page-issued "more" API.
//!
//! Naver's mobile tab pages render exactly one batch of results. The
//! tab URL's own `start`/`page` parameters are ignored by the server
//! (measured: page 1 and page 2 URLs return the identical batch), but
//! every tab page embeds the URL of its "more" API — the endpoint the
//! page's own JavaScript calls to load the next batch. Following that
//! chain is how deeper pages are fetched here:
//!
//! - image: `s.search.naver.com/p/c/image/...` answers JSON
//!   `{items: [...], url: <next>}` whose items have the same shape as
//!   the tab page's embedded `imageSearchTabData` items.
//! - news / video / blog: the API answers JSON
//!   `{collection: [{html: "..."}], url: <next>}` whose HTML fragment
//!   parses with the same parsers as the tab pages.
//!
//! The chain is followed from page 1 for every request (the client is
//! stateless), so page N costs N requests. Pages are capped at
//! [`MAX_PAGE`]. The web tab embeds no such API URL and its server
//! batch never changes, so web paging is not offered.

use anyhow::Result;
use serde_json::Value;

use crate::common::{layout_error, SEARCH_REFERER};
use crate::NaverSearchClient;

/// Deepest page served through the more-API chain. The chain
/// itself ends when the server stops issuing a next-batch URL, so
/// functionally paging follows the chain as far as it goes; this
/// constant is only a per-call request-volume safety bound (one
/// page = one extra request), set high enough not to truncate
/// realistic result sets (audit W-26; it was 10, which cut off
/// result sets the server would still have paged through).
pub const MAX_PAGE: i64 = 100;

/// Unescape the forms in which API URLs appear inside tab pages:
/// JS-escaped (`\/`, `&`) and HTML-escaped (`&amp;`).
fn unescape_url(raw: &str) -> String {
    raw.replace("\\/", "/")
        .replace("\\u0026", "&")
        .replace("&amp;", "&")
}

/// Whether `url` is the more-API URL for `vertical`.
fn is_more_url(vertical: &str, url: &str) -> bool {
    match vertical {
        "image" => url.contains("/p/c/image/") && url.contains("api_type=mobile_tab_more"),
        "news" => url.contains("/p/newssearch/") && url.contains("/api/tab/more"),
        "video" => url.contains("/p/video/") && url.contains("video_more=1"),
        "blog" => url.contains("/p/review/"),
        _ => false,
    }
}

/// Extract the page-issued more-API URL for `vertical` from a tab
/// page's HTML, if the page carries one. Returns the absolute URL.
pub fn extract_more_url(html: &str, vertical: &str) -> Option<String> {
    // The host appears literally in every embedding form; the path
    // slashes after it may be JS-escaped (`\/`).
    let needle = "s.search.naver.com";
    let mut from = 0;
    while let Some(rel) = html[from..].find(needle) {
        let start = from + rel;
        // The URL runs until a quote, angle bracket or whitespace in
        // every embedding form (script string, JS literal, attribute).
        let tail = &html[start..];
        let end = tail
            .find(|c: char| c == '"' || c == '\'' || c == '<' || c == '>' || c.is_whitespace())
            .unwrap_or(tail.len());
        let url = unescape_url(&tail[..end]);
        if is_more_url(vertical, &url) {
            return Some(format!("https://{url}"));
        }
        from = start + needle.len();
    }
    None
}

/// One batch payload from a more-API response.
pub enum MorePayload {
    /// Raw item objects (image API — same shape as the tab payload).
    ImageItems(Vec<Value>),
    /// An HTML fragment (news / video / blog APIs).
    Html(String),
}

/// Parse a more-API JSON response into its payload and the URL of the
/// next batch (`None` when the chain ends).
pub fn parse_more_response(vertical: &str, body: &Value) -> Result<(MorePayload, Option<String>)> {
    let next = body
        .get("url")
        .and_then(|u| u.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if vertical == "image" {
        let items = body
            .get("items")
            .and_then(|i| i.as_array())
            .ok_or_else(|| layout_error(vertical))?;
        return Ok((MorePayload::ImageItems(items.clone()), next));
    }
    let collection = body
        .get("collection")
        .and_then(|c| c.as_array())
        .ok_or_else(|| layout_error(vertical))?;
    let mut html = String::new();
    for entry in collection {
        if let Some(fragment) = entry.get("html").and_then(|h| h.as_str()) {
            html.push_str(fragment);
        }
    }
    if html.is_empty() {
        return Err(layout_error(vertical));
    }
    Ok((MorePayload::Html(html), next))
}

impl NaverSearchClient {
    /// Fetch one more-API batch: its payload and the next batch's URL.
    pub(crate) async fn more_payload(
        &self,
        url: &str,
        vertical: &str,
    ) -> Result<(MorePayload, Option<String>)> {
        let body = self.get_json(url, SEARCH_REFERER).await?;
        parse_more_response(vertical, &body)
    }
}

/// Resolve one page of a chained vertical.
///
/// `first_html` is the tab page (batch 1). For `page == 1` the parsed
/// first batch is returned as-is. For deeper pages the more-API chain
/// embedded in the page is followed batch by batch; the returned
/// batch is renumbered to continue from the items of all previous
/// batches. When the chain ends before the requested page, the page
/// is empty (the result set simply has no such page).
pub(crate) async fn chained_items<T>(
    client: &NaverSearchClient,
    vertical: &str,
    first_html: &str,
    page: i64,
    parse_first: impl Fn(&str) -> Result<Vec<T>>,
    parse_more: impl Fn(MorePayload) -> Result<Vec<T>>,
    set_rank: impl Fn(&mut T, i64),
) -> Result<Vec<T>> {
    let first = parse_first(first_html)?;
    if page <= 1 {
        return Ok(first);
    }
    let mut prior = first.len() as i64;
    let mut next = extract_more_url(first_html, vertical);
    let mut batch = 1;
    while batch < page {
        let Some(url) = next.take() else {
            return Ok(Vec::new());
        };
        let (payload, following) = client.more_payload(&url, vertical).await?;
        let items = parse_more(payload)?;
        next = following;
        batch += 1;
        if batch == page {
            let mut items = items;
            for (i, item) in items.iter_mut().enumerate() {
                set_rank(item, prior + i as i64 + 1);
            }
            return Ok(items);
        }
        prior += items.len() as i64;
    }
    Ok(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_image_url_from_escaped_script() {
        let html = r#"{"u":"s.search.naver.com\/p\/c\/image\/46\/search.naver?ac=0\u0026api_type=mobile_tab_more\u0026start=51\u0026where=m_image"}"#;
        let url = extract_more_url(html, "image").unwrap();
        assert!(url.starts_with("https://s.search.naver.com/p/c/image/46/"));
        assert!(url.contains("api_type=mobile_tab_more"));
        assert!(url.contains("start=51"));
    }

    #[test]
    fn extract_video_url_from_attribute() {
        let html = r#"<a data-api="https://s.search.naver.com/p/video/48/search.naver?ac=0&amp;start=49&amp;video_more=1">x</a>"#;
        let url = extract_more_url(html, "video").unwrap();
        assert!(url.contains("video_more=1"));
        assert!(url.contains("start=49"));
        assert!(!url.contains("&amp;"));
    }

    #[test]
    fn extract_blog_url_from_js_config() {
        let html = r#"new n({url:"https://s.search.naver.com/p/review/50/search.naver?abt=&api_type=7&page=2"})"#;
        let url = extract_more_url(html, "blog").unwrap();
        assert!(url.contains("/p/review/50/"));
    }

    #[test]
    fn extract_returns_none_without_a_match() {
        assert!(extract_more_url("<html>nothing</html>", "news").is_none());
        // The web tab has no more-API URL.
        assert!(extract_more_url("<html>nothing</html>", "web").is_none());
    }

    #[test]
    fn parse_image_response_reads_items_and_next() {
        let body = serde_json::json!({
            "items": [{"type": "image", "originalUrl": "http://x/a.jpg"}],
            "url": "https://s.search.naver.com/next"
        });
        let (payload, next) = parse_more_response("image", &body).unwrap();
        assert!(matches!(payload, MorePayload::ImageItems(ref v) if v.len() == 1));
        assert_eq!(next.as_deref(), Some("https://s.search.naver.com/next"));
    }

    #[test]
    fn parse_html_response_concatenates_collection() {
        let body = serde_json::json!({
            "collection": [{"html": "<div>a</div>"}, {"html": "<div>b</div>"}],
            "url": ""
        });
        let (payload, next) = parse_more_response("news", &body).unwrap();
        assert!(
            matches!(payload, MorePayload::Html(ref h) if h.contains("<div>a</div>") && h.contains("<div>b</div>"))
        );
        assert!(next.is_none());
    }
}
