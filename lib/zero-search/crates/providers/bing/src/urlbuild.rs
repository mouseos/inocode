//! Search URL generation, reproducing what the Bing Android app builds.
//!
//! The app (APK analysis, round 1) opens
//! `https://www.bing.com/search?q=%s&form=%s&sdkhh=1` in its WebView for web
//! search and swaps the path per vertical. The page URLs built here serve
//! image / video / news (fetched as the *desktop* rendering, so
//! `form`/`sdkhh` are omitted there) and the `bing_build_search_url`
//! tool. Image search is the exception — it only returns server-rendered
//! results with the app-style parameters *and* a mobile user agent
//! (round 2), so its URL keeps `form=L2GV01&sdkhh=1`.
//!
//! Web *search* itself does not fetch a page: it calls the app's native
//! v7 search API ([`web_api_url`]) — see `web.rs`.
//!
//! Two parameters are effectively mandatory everywhere: without
//! `mkt`/`setlang` Bing mis-detects the market and returns an empty result
//! set ("no results") instead of an error. Empty values therefore fall back
//! to `ja-JP` / `ja`.

use anyhow::{anyhow, Result};

use crate::common::urlencode;

/// Host serving all search pages.
pub const SEARCH_HOST: &str = "https://www.bing.com";
/// App-embedded public identifier for the suggestions API. It ships inside
/// the Bing app and is not a user credential.
pub const SUGGEST_APPID: &str = "6D0A9B8C5100E9ECC7E11A104ADD76C10219804B";
/// Suggestions API base (the app's own endpoint, APK `d4.java`).
pub const SUGGEST_BASE: &str = "https://www.bingapis.com/api/v7/suggestions";
/// App-embedded public identifier for the v7 search API. The app uses
/// this endpoint for related queries (APK `n38.java`); the response
/// also carries the full web result set, which `bing_web_search` uses.
/// Like the suggestions appid, it ships inside the app and is not a
/// user credential.
pub const V7_SEARCH_APPID: &str = "8B3803673894068AC59BD9DA9BC2B4D382C6227E";
/// v7 search API base (the app's own endpoint, APK `n38.java`).
pub const V7_SEARCH_BASE: &str = "https://www.bingapis.com/api/v7/search";

/// Results per page for web search (`first` advances in steps of 10).
pub const WEB_PER_PAGE: i64 = 10;
/// Results per page for image search (`first` advances in steps of 12).
pub const IMAGE_PER_PAGE: i64 = 12;
/// Batch size for video search (the page's own async config uses
/// `count=35`); `first` advances in steps of this size.
pub const VIDEO_PER_PAGE: i64 = 35;

/// Normalise the vertical name (accepts the plural forms the suggest API
/// uses, e.g. `images`).
pub fn normalize_vertical(vertical: &str) -> &str {
    match vertical.trim().to_lowercase().as_str() {
        "image" | "images" => "image",
        "video" | "videos" => "video",
        "news" => "news",
        "shopping" | "shop" => "shopping",
        "suggest" | "suggestions" => "suggest",
        _ => "web",
    }
}

fn is_search_vertical(vertical: &str) -> bool {
    matches!(vertical, "web" | "image" | "video" | "news" | "shopping")
}

/// Strictly resolve a caller-supplied vertical for URL building: unknown
/// names are an error, never a silent fall-back to web.
fn strict_vertical(vertical: &str) -> Result<&'static str> {
    match vertical.trim().to_lowercase().as_str() {
        "web" => Ok("web"),
        "image" | "images" => Ok("image"),
        "video" | "videos" => Ok("video"),
        "news" => Ok("news"),
        "shopping" | "shop" => Ok("shopping"),
        other => Err(anyhow!("unsupported vertical: {other}")),
    }
}

fn market(mkt: &str) -> String {
    let m = mkt.trim();
    if m.is_empty() {
        "ja-JP".to_string()
    } else {
        m.to_string()
    }
}

fn language(setlang: &str) -> String {
    let l = setlang.trim();
    if l.is_empty() {
        "ja".to_string()
    } else {
        l.to_string()
    }
}

/// Country code for the suggestions API, derived from the market
/// (`ja-JP` → `JP`).
fn country_code(mkt: &str) -> String {
    let m = market(mkt);
    match m.rsplit('-').next() {
        Some(cc) if cc.len() == 2 => cc.to_uppercase(),
        _ => "JP".to_string(),
    }
}

fn path_for(vertical: &str) -> &'static str {
    match vertical {
        "image" => "/images/search",
        "video" => "/videos/search",
        "news" => "/news/search",
        // The shopping vertical the app's deep links point at
        // (`https://www.bing.com/shop/search?q=`, audit W-15).
        "shopping" => "/shop/search",
        _ => "/search",
    }
}

fn per_page(vertical: &str) -> i64 {
    match vertical {
        "image" => IMAGE_PER_PAGE,
        "video" => VIDEO_PER_PAGE,
        _ => WEB_PER_PAGE,
    }
}

/// Build the search URL for a 1-based `page` of a search vertical — exactly
/// the URL the client fetches. `news` has no paging on Bing's side, so its
/// URL never carries `first`.
pub fn build_page_url(
    query: &str,
    vertical: &str,
    page: i64,
    mkt: &str,
    setlang: &str,
) -> Result<String> {
    let vertical = strict_vertical(vertical)?;
    debug_assert!(is_search_vertical(vertical));
    let page = page.max(1);
    let mut url = format!(
        "{SEARCH_HOST}{}?q={}",
        path_for(vertical),
        urlencode(query.trim())
    );
    if vertical == "image" {
        // App-style parameters: without this pair (plus a mobile UA) the
        // image page ships an empty shell and loads results via JS.
        url.push_str("&form=L2GV01&sdkhh=1");
    }
    url.push_str(&format!(
        "&mkt={}&setlang={}",
        urlencode(&market(mkt)),
        urlencode(&language(setlang))
    ));
    if vertical != "news" {
        let first = (page - 1) * per_page(vertical) + 1;
        url.push_str(&format!("&first={first}"));
    }
    Ok(url)
}

/// Build the first-page search URL for a vertical (the
/// `bing_build_search_url` tool; no network access).
pub fn build_search_url(query: &str, vertical: &str, mkt: &str, setlang: &str) -> Result<String> {
    build_page_url(query, vertical, 1, mkt, setlang)
}

/// Build the v7 search API URL for a 1-based `page` of web search —
/// the URL `bing_web_search` fetches. Paging is `count`/`offset`
/// based (10 results per page), unlike the HTML pages' `first`.
pub fn web_api_url(query: &str, page: i64, mkt: &str, setlang: &str) -> String {
    let page = page.max(1);
    let offset = (page - 1) * WEB_PER_PAGE;
    format!(
        "{V7_SEARCH_BASE}?appid={V7_SEARCH_APPID}&q={}&cc={}&setlang={}&count={WEB_PER_PAGE}&offset={offset}",
        urlencode(query.trim()),
        country_code(mkt),
        urlencode(&language(setlang))
    )
}

/// Build the suggestions API URL for a query. `vertical` is `web` (default),
/// `images` or `videos` — the app uses path variants of the same endpoint.
pub fn suggest_url(query: &str, vertical: &str, mkt: &str, setlang: &str) -> Result<String> {
    let suffix = match vertical.trim().to_lowercase().as_str() {
        "web" | "" => "",
        "image" | "images" => "/images",
        "video" | "videos" => "/videos",
        other => return Err(anyhow!("unsupported suggest vertical: {other}")),
    };
    Ok(format!(
        "{SUGGEST_BASE}{suffix}?q={}&appid={SUGGEST_APPID}&cc={}&setlang={}&history=0",
        urlencode(query.trim()),
        country_code(mkt),
        urlencode(&language(setlang))
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_url_carries_market_and_first_page() {
        let url = build_search_url("テスト", "web", "ja-JP", "ja").unwrap();
        assert_eq!(
            url,
            "https://www.bing.com/search?q=%E3%83%86%E3%82%B9%E3%83%88&mkt=ja-JP&setlang=ja&first=1"
        );
    }

    #[test]
    fn image_url_keeps_app_style_parameters() {
        let url = build_page_url("猫", "image", 2, "ja-JP", "ja").unwrap();
        assert!(url.starts_with("https://www.bing.com/images/search?q="));
        assert!(url.contains("&form=L2GV01&sdkhh=1"));
        assert!(url.contains("&first=13"));
    }

    #[test]
    fn news_url_has_no_paging_parameter() {
        let url = build_page_url("経済", "news", 3, "ja-JP", "ja").unwrap();
        assert!(!url.contains("first="));
    }

    #[test]
    fn empty_market_falls_back_to_japan() {
        let url = build_search_url("test", "web", "", "").unwrap();
        assert!(url.contains("mkt=ja-JP"));
        assert!(url.contains("setlang=ja"));
    }

    #[test]
    fn suggest_url_uses_appid_and_country() {
        let url = suggest_url("テスト", "web", "ja-JP", "ja").unwrap();
        assert!(url.starts_with("https://www.bingapis.com/api/v7/suggestions?q="));
        assert!(url.contains(&format!("appid={SUGGEST_APPID}")));
        assert!(url.contains("&cc=JP&setlang=ja&history=0"));
        let img = suggest_url("テスト", "images", "ja-JP", "ja").unwrap();
        assert!(img.contains("/api/v7/suggestions/images?q="));
    }

    #[test]
    fn web_api_url_uses_appid_country_and_offset_paging() {
        let first = web_api_url("テスト", 1, "ja-JP", "ja");
        assert_eq!(
            first,
            "https://www.bingapis.com/api/v7/search?appid=8B3803673894068AC59BD9DA9BC2B4D382C6227E&q=%E3%83%86%E3%82%B9%E3%83%88&cc=JP&setlang=ja&count=10&offset=0"
        );
        let second = web_api_url("テスト", 2, "ja-JP", "ja");
        assert!(second.contains("&count=10&offset=10"));
        let third = web_api_url("test", 3, "en-US", "en");
        assert!(third.contains("&cc=US&setlang=en&count=10&offset=20"));
    }

    #[test]
    fn unknown_vertical_is_an_error() {
        assert!(build_search_url("x", "maps", "", "").is_err());
        assert!(suggest_url("x", "news", "", "").is_err());
    }

    #[test]
    fn shopping_url_uses_shop_path() {
        let url = build_search_url("ノートパソコン", "shopping", "ja-JP", "ja").unwrap();
        assert!(url.starts_with("https://www.bing.com/shop/search?q="));
        assert!(url.contains("&mkt=ja-JP&setlang=ja&first=1"));
        let page2 = build_page_url("x", "shop", 2, "", "").unwrap();
        assert!(page2.contains("&first=11"));
    }
}
