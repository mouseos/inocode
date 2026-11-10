//! Search URL generation, reproducing what the Naver Android app builds.
//!
//! The app (APK analysis, round 1) opens
//! `https://m.search.naver.com/search.naver?where=m&ie=utf8&query=...&sm=...`
//! in its WebView for integrated search; the vertical tabs use the same
//! host with an `ssc=tab.m_*.all` section code (plus `where=m_*` where the
//! tab needs it). The desktop host only ships a JavaScript shell over
//! plain HTTP, so everything here stays on the mobile host.
//!
//! Paging, as measured against the live pages: every tab page renders
//! one batch and ignores its URL's `start`/`page` parameters when
//! they are varied naively. Deeper batches of the image / news /
//! video / blog tabs are served by the "more" API whose URL each
//! page itself issues — see the `more` module; those pages therefore
//! cannot be expressed as a single URL and are an error here
//! ([`crate::common::unsupported_page`]). The web-results view is
//! the exception: it pages through its own pagination-bar hrefs
//! (`page=N+1&start=(N-1)*20+1`, audit W-22), which are expressible
//! as URLs and are built below.

use anyhow::{anyhow, Result};

use crate::common::{unsupported_page, urlencode};

/// Mobile search host + path, the app's search entry point.
pub const SEARCH_BASE: &str = "https://m.search.naver.com/search.naver";
/// Suggestions endpoint (the public `nx/ac` JSON API, verified live).
pub const SUGGEST_BASE: &str = "https://ac.search.naver.com/nx/ac";

/// Items per batch of the web-results view: its pagination bar
/// advances `start` in steps of 20 (measured live, audit W-22).
pub const WEB_PER_PAGE: i64 = 20;
/// Items in the blog tab's first batch.
pub const BLOG_PER_PAGE: i64 = 30;
/// Items in the news tab's first batch.
pub const NEWS_PER_PAGE: i64 = 15;
/// Items the video tab's "more" config asks for per batch.
pub const VIDEO_PER_PAGE: i64 = 48;

/// Normalise a vertical name for display purposes.
pub fn normalize_vertical(vertical: &str) -> &str {
    match vertical.trim().to_lowercase().as_str() {
        "integrated" | "all" | "m" => "integrated",
        "web" | "url" | "ur" => "web",
        "image" | "images" => "image",
        "news" => "news",
        "video" | "videos" => "video",
        "blog" => "blog",
        "cafe" => "cafe",
        "kin" | "qna" => "kin",
        "ai" => "ai",
        "suggest" | "suggestions" => "suggest",
        _ => "integrated",
    }
}

/// Strictly resolve a caller-supplied vertical for URL building: unknown
/// names are an error, never a silent fall-back.
fn strict_vertical(vertical: &str) -> Result<&'static str> {
    match vertical.trim().to_lowercase().as_str() {
        "integrated" | "all" => Ok("integrated"),
        "web" => Ok("web"),
        "image" | "images" => Ok("image"),
        "news" => Ok("news"),
        "video" | "videos" => Ok("video"),
        "blog" => Ok("blog"),
        "cafe" => Ok("cafe"),
        "kin" => Ok("kin"),
        "ai" => Ok("ai"),
        other => Err(anyhow!("unsupported vertical: {other}")),
    }
}

/// Build the search URL for a 1-based `page` of a vertical — exactly the
/// URL the client fetches.
pub fn build_page_url(query: &str, vertical: &str, page: i64) -> Result<String> {
    let vertical = strict_vertical(vertical)?;
    let page = page.max(1);
    let q = urlencode(query.trim());
    let url = match vertical {
        "integrated" => {
            if page > 1 {
                return Err(unsupported_page("integrated", page));
            }
            // The app's own integrated-search URL (InAppBrowser).
            format!("{SEARCH_BASE}?where=m&ie=utf8&query={q}&sm=mob_hty.sch")
        }
        "web" => {
            if page == 1 {
                format!("{SEARCH_BASE}?ssc=tab.m_ur.all&sm=mtb_jum&query={q}")
            } else {
                // The view's own pagination-bar href form (measured
                // live 2026-11-08, audit W-22): the bar's link for
                // batch N (N ≥ 2) carries `page=N+1` and
                // `start=(N-1)*20+1`. The often-cited
                // `page=2&start=1` link points back at batch 1,
                // which is how an earlier measurement concluded the
                // batches never change — they do.
                let start = (page - 1) * WEB_PER_PAGE + 1;
                format!(
                    "{SEARCH_BASE}?nso=&page={}&qdt=0&query={q}&sm=mtb_pge&ssc=tab.m_ur.all&start={start}",
                    page + 1
                )
            }
        }
        "image" => {
            if page > 1 {
                return Err(unsupported_page("image", page));
            }
            format!("{SEARCH_BASE}?ssc=tab.m_image.all&where=m_image&sm=mtb_jum&query={q}")
        }
        "news" => {
            if page > 1 {
                return Err(unsupported_page("news", page));
            }
            format!("{SEARCH_BASE}?ssc=tab.m_news.all&where=m_news&sm=mtb_jum&query={q}")
        }
        "video" => {
            if page > 1 {
                return Err(unsupported_page("video", page));
            }
            format!("{SEARCH_BASE}?ssc=tab.m_video.all&where=m_video&sm=mtb_jum&query={q}")
        }
        "blog" => {
            if page > 1 {
                return Err(unsupported_page("blog", page));
            }
            format!("{SEARCH_BASE}?ssc=tab.m_blog.all&sm=mtb_jum&query={q}")
        }
        "cafe" => {
            if page > 1 {
                return Err(unsupported_page("cafe", page));
            }
            format!("{SEARCH_BASE}?ssc=tab.m_cafe.all&sm=mtb_jum&query={q}")
        }
        "kin" => {
            if page > 1 {
                return Err(unsupported_page("kin", page));
            }
            format!("{SEARCH_BASE}?ssc=tab.m_kin.all&sm=mtb_jum&query={q}")
        }
        "ai" => {
            if page > 1 {
                return Err(unsupported_page("ai", page));
            }
            // The app's AI-tab URL (`getKeywordAiSearchURL`). The
            // page it returns is a client-rendered shell with no
            // server-rendered answer (measured live 2026-11-08), so
            // the URL is buildable here but no search method fetches
            // it (audit W-23).
            format!("{SEARCH_BASE}?ssc=tab.m_ait.all&query={q}&sm=mob_hty.aitab")
        }
        _ => unreachable!("strict_vertical only returns known verticals"),
    };
    Ok(url)
}

/// Build the first-page search URL for a vertical (the
/// `naver_build_search_url` tool; no network access).
pub fn build_search_url(query: &str, vertical: &str) -> Result<String> {
    build_page_url(query, vertical, 1)
}

/// Build the suggestions API URL for a query.
pub fn suggest_url(query: &str) -> String {
    format!(
        "{SUGGEST_BASE}?q={}&q_enc=UTF-8&st=100&frm=nv&r_format=json&r_enc=UTF-8&r_unicode=0&t_koreng=1&ans=2&rev=4",
        urlencode(query.trim())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrated_url_matches_the_app_template() {
        let url = build_search_url("테스트", "integrated").unwrap();
        assert_eq!(
            url,
            "https://m.search.naver.com/search.naver?where=m&ie=utf8&query=%ED%85%8C%EC%8A%A4%ED%8A%B8&sm=mob_hty.sch"
        );
    }

    #[test]
    fn deep_pages_are_not_expressible_as_urls() {
        // Tab pages ignore page/start parameters; deeper batches come
        // from the page-issued more-API chain (module `more`), and the
        // integrated view has no deeper pages at all. The web view is
        // the exception: it pages through its own bar hrefs (below).
        for v in [
            "image",
            "news",
            "video",
            "blog",
            "integrated",
            "cafe",
            "kin",
            "ai",
        ] {
            let err = build_page_url("x", v, 2).unwrap_err();
            assert!(
                err.to_string().starts_with("unsupported_page:"),
                "{v}: {err}"
            );
        }
        for v in ["web", "image", "news", "video", "blog", "cafe", "kin", "ai"] {
            assert!(build_page_url("x", v, 1).is_ok(), "{v} page 1");
        }
    }

    #[test]
    fn web_page_two_uses_the_pagination_bar_href() {
        // Measured live 2026-11-08 (audit W-22): batch 2 is the bar's
        // page=3&start=21 href; batch 3 is page=4&start=41.
        let url = build_page_url("테스트", "web", 2).unwrap();
        assert!(url.contains("page=3"), "{url}");
        assert!(url.contains("start=21"), "{url}");
        assert!(url.contains("sm=mtb_pge"), "{url}");
        let url3 = build_page_url("테스트", "web", 3).unwrap();
        assert!(
            url3.contains("page=4") && url3.contains("start=41"),
            "{url3}"
        );
    }

    #[test]
    fn suggest_url_uses_the_public_endpoint() {
        let url = suggest_url("네이");
        assert!(url.starts_with("https://ac.search.naver.com/nx/ac?q="));
        assert!(url.contains("&r_format=json"));
    }

    #[test]
    fn unknown_vertical_is_an_error() {
        assert!(build_search_url("x", "shopping").is_err());
        assert!(build_search_url("x", "dictionary").is_err());
    }
}
