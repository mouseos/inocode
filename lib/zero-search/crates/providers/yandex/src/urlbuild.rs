//! Offline construction of Yandex search URLs (the `yandex_build_search_url`
//! tool). These are exactly the URLs the search methods request, so a URL
//! built here can be opened in a browser or fed to another tool.

use anyhow::{anyhow, Result};

use crate::common::urlencode;

/// Verticals a URL can be built for. News is deliberately absent: the news
/// vertical moved to dzen.ru and requires Yandex SSO sign-in.
pub const URL_VERTICALS: [&str; 5] = ["web", "image", "video", "suggest", "suggest_endings"];

/// Build the suggest-endings (tail-completion) URL — the second
/// autocomplete endpoint, verified live in the analysis (round 3,
/// trial 6b). Its response shape differs from suggest-ya.cgi; see
/// `suggest::parse_suggest_endings_body`.
pub fn build_suggest_endings_url(query: &str) -> String {
    format!(
        "https://yandex.ru/suggest/suggest-endings?part={}&v=4",
        urlencode(query.trim())
    )
}

/// Build the touch search URL for `vertical` without any network access.
///
/// `vertical` is one of `web` (default), `image`, `video`, `suggest`.
/// The image/video URLs carry `source=tabbar`, matching the tab links the
/// Yandex app itself follows. (The `rpt=imageview` form must not be used for
/// text queries — it opens the search-by-image landing instead.)
pub fn build_search_url(query: &str, vertical: &str) -> Result<String> {
    let query = query.trim();
    if query.is_empty() {
        return Err(anyhow!("query must not be empty"));
    }
    let q = urlencode(query);
    match vertical.trim().to_lowercase().as_str() {
        "" | "web" => Ok(format!(
            "https://yandex.ru/search/touch/?text={q}&{}",
            crate::common::UI_PARAM
        )),
        "image" | "images" => Ok(format!(
            "https://yandex.ru/images/touch/search?text={q}&source=tabbar&{}",
            crate::common::UI_PARAM
        )),
        "video" | "videos" => Ok(format!(
            "https://yandex.ru/video/touch/search?text={q}&source=tabbar&{}",
            crate::common::UI_PARAM
        )),
        "suggest" => Ok(format!(
            "https://yandex.ru/suggest/suggest-ya.cgi?part={q}&v=4&tpah=1&uil=ru"
        )),
        "suggest_endings" | "suggest-endings" => Ok(build_suggest_endings_url(query)),

        other => Err(anyhow!(
            "unsupported vertical: {other} (supported: web, image, video, suggest, suggest_endings; \
             news is unavailable — it moved to dzen.ru and requires Yandex SSO)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_each_vertical() {
        assert_eq!(
            build_search_url("test", "web").unwrap(),
            "https://yandex.ru/search/touch/?text=test&ui=webmobileapp.yandex"
        );
        assert_eq!(
            build_search_url("test", "").unwrap(),
            "https://yandex.ru/search/touch/?text=test&ui=webmobileapp.yandex"
        );
        assert_eq!(
            build_search_url("test", "image").unwrap(),
            "https://yandex.ru/images/touch/search?text=test&source=tabbar&ui=webmobileapp.yandex"
        );
        assert_eq!(
            build_search_url("test", "video").unwrap(),
            "https://yandex.ru/video/touch/search?text=test&source=tabbar&ui=webmobileapp.yandex"
        );
        assert_eq!(
            build_search_url("test", "suggest").unwrap(),
            "https://yandex.ru/suggest/suggest-ya.cgi?part=test&v=4&tpah=1&uil=ru"
        );
    }

    #[test]
    fn encodes_query_and_rejects_news_and_empty() {
        assert!(build_search_url("кот тест", "web")
            .unwrap()
            .contains("%D0%BA%D0%BE%D1%82%20%D1%82%D0%B5%D1%81%D1%82"));
        assert!(build_search_url("test", "news").is_err());
        assert!(build_search_url("  ", "web").is_err());
    }
}
