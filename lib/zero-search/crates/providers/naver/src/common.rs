//! Shared helpers: HTTP behaviour, HTML/entity cleanup, URL handling and
//! the small DOM utilities the vertical parsers share.

use anyhow::anyhow;
use regex::Regex;
use scraper::{ElementRef, Selector};
use std::sync::LazyLock;

/// Android mobile UA — Naver's mobile search host only serves
/// server-rendered results to a mobile agent (APK analysis, round 1).
pub const MOBILE_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36";

pub const ACCEPT_LANGUAGE: &str = "ko,en-US;q=0.9,en;q=0.8";

/// Referer sent with search page fetches (the mobile search home).
pub const SEARCH_REFERER: &str = "https://m.search.naver.com/";

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());

static NUMERIC_ENTITY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);").unwrap());

/// Korean relative-time stamps as Naver renders them (`8시간 전`, `4주 전`,
/// `방금 전`) plus the absolute `2025.09.16.` form.
static TIME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(\d+\s*(?:분|시간|일|주|개월|년)\s*전|방금\s*전|\d{4}\.\d{1,2}\.\d{1,2}\.?)")
        .unwrap()
});

static DURATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{1,3}:\d{2}(?::\d{2})?$").unwrap());

static VIEWS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"조회수\s*[0-9.,]+\s*(?:만|억|천)?").unwrap());

/// Turn an HTTP status into the unified error vocabulary. 403/429 (and any
/// CAPTCHA wall) become `access_restricted`; the client never retries or
/// tries to evade them. The `kind:` prefix is how the server classifies the
/// error into the unified `{kind, message}` form.
pub fn status_error(status: reqwest::StatusCode) -> anyhow::Error {
    if status.as_u16() == 403 || status.as_u16() == 429 {
        anyhow!("access_restricted: HTTP {status} from Naver (rate limited or blocked)")
    } else {
        anyhow!("http_error: HTTP {status} from Naver")
    }
}

/// Map a reqwest failure onto the same `kind:` prefix vocabulary.
pub fn map_reqwest_error(e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow!("timeout: {e}")
    } else if e.is_connect() {
        anyhow!("connection_error: {e}")
    } else if e.is_decode() || e.is_body() {
        anyhow!("parse_error: {e}")
    } else {
        anyhow!("http_error: {e}")
    }
}

/// Guard shared by the HTML parsers: a page whose expected result markup is
/// entirely absent means the layout changed (or a block page was served),
/// which must surface as an error rather than a silent empty result.
pub fn layout_error(vertical: &str) -> anyhow::Error {
    anyhow!("parse_error: {vertical} result blocks not found (Naver page layout may have changed)")
}

/// Error for `page` values that cannot be served: tab pages ignore
/// page parameters (deeper batches come from the page-issued more-API
/// chain, not a URL), the integrated view has no deeper pages, and
/// chained paging is capped at `more::MAX_PAGE`.
pub fn unsupported_page(vertical: &str, page: i64) -> anyhow::Error {
    anyhow!("unsupported_page: page {page} is not available for the {vertical} search")
}

/// Normalise a piece of page text: strip tags, decode entities and
/// collapse whitespace.
pub fn clean_text(s: &str) -> String {
    let without_tags = TAG_RE.replace_all(s, "");
    let decoded = decode_entities(&without_tags);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn decode_entities(s: &str) -> String {
    let named = s
        .replace("&nbsp;", " ")
        .replace("&middot;", "·")
        .replace("&hellip;", "…")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–")
        .replace("&laquo;", "«")
        .replace("&raquo;", "»")
        .replace("&lsquo;", "\u{2018}")
        .replace("&rsquo;", "\u{2019}")
        .replace("&ldquo;", "\u{201c}")
        .replace("&rdquo;", "\u{201d}")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");

    let numeric = NUMERIC_ENTITY_RE.replace_all(&named, |caps: &regex::Captures| {
        let raw = &caps[1];
        let code = if let Some(hex) = raw.strip_prefix('x').or_else(|| raw.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            raw.parse::<u32>().ok()
        };
        code.and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_else(|| caps[0].to_string())
    });

    // `&amp;` last so `&amp;lt;` does not collapse into a real `<`.
    numeric.replace("&amp;", "&")
}

/// Percent-encode a string for use in a query value (RFC 3986 unreserved set).
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for byte in s.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

/// Clamp a caller-supplied limit into a sane range.
pub fn clamp_limit(limit: i64, max: i64) -> usize {
    limit.clamp(1, max) as usize
}

/// Look up one query parameter in a URL (percent-decoded by the URL parser).
pub fn query_param(url: &str, key: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    parsed
        .query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
}

/// Resolve a possibly-relative href against a base origin.
pub fn resolve_url(href: &str, base: &str) -> String {
    if href.starts_with("http://") || href.starts_with("https://") {
        return href.to_string();
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    if href.starts_with('/') {
        return format!("{base}{href}");
    }
    href.to_string()
}

/// All text under an element, normalised with [`clean_text`].
pub fn element_text(el: ElementRef) -> String {
    clean_text(&el.text().collect::<String>())
}

/// First Korean relative/absolute time stamp in `text`, if any.
pub fn find_time(text: &str) -> Option<String> {
    TIME_RE
        .captures(text)
        .map(|c| c[1].split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Whether `text` is a bare play duration (`08:03`, `1:02:03`).
pub fn is_duration(text: &str) -> bool {
    DURATION_RE.is_match(text.trim())
}

/// First view-count phrase (`조회수 449`, `조회수 1.2만`) in `text`, if any.
pub fn find_views(text: &str) -> Option<String> {
    VIEWS_RE
        .captures(text)
        .map(|c| c[0].split_whitespace().collect::<Vec<_>>().join(" "))
}

/// Climb from `el` to the smallest ancestor that still contains only the
/// item `el` belongs to — i.e. keep climbing while the ancestor's subtree
/// holds at most one element matching `marker`. This locates the result
/// "card" around a link without depending on Naver's hashed CSS classes.
/// At most `max_levels` ancestors are considered; the starting element
/// itself is the fallback.
pub fn card_for<'a>(el: ElementRef<'a>, marker: &Selector, max_levels: usize) -> ElementRef<'a> {
    let mut card = el;
    let mut node = el.parent();
    for _ in 0..max_levels {
        let Some(n) = node else { break };
        let Some(er) = ElementRef::wrap(n) else {
            node = n.parent();
            continue;
        };
        if er.select(marker).count() > 1 {
            break;
        }
        card = er;
        node = n.parent();
    }
    card
}

/// Like [`card_for`], but the item boundary is "at most one *distinct*
/// value of `attr` among elements matching `marker`" — for groups where one
/// item owns several marker elements (video cards carry several
/// `a[data-api]` anchors for the same video).
pub fn card_for_unique<'a>(
    el: ElementRef<'a>,
    marker: &Selector,
    attr: &str,
    max_levels: usize,
) -> ElementRef<'a> {
    fn distinct(er: ElementRef, marker: &Selector, attr: &str) -> usize {
        let mut seen = Vec::new();
        for m in er.select(marker) {
            if let Some(v) = m.value().attr(attr) {
                if !seen.contains(&v) {
                    seen.push(v);
                }
            }
        }
        seen.len()
    }
    let mut card = el;
    let mut node = el.parent();
    for _ in 0..max_levels {
        let Some(n) = node else { break };
        let Some(er) = ElementRef::wrap(n) else {
            node = n.parent();
            continue;
        };
        if distinct(er, marker, attr) > 1 {
            break;
        }
        card = er;
        node = n.parent();
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_strips_tags_and_entities() {
        assert_eq!(
            clean_text("<mark>테스트</mark> &amp; 실험"),
            "테스트 & 실험"
        );
        assert_eq!(clean_text("a\n b\t c"), "a b c");
    }

    #[test]
    fn find_time_reads_korean_stamps() {
        assert_eq!(
            find_time("조선비즈8시간 전네이버뉴스").as_deref(),
            Some("8시간 전")
        );
        assert_eq!(find_time("4주 전").as_deref(), Some("4주 전"));
        assert_eq!(find_time("방금 전").as_deref(), Some("방금 전"));
        assert_eq!(find_time("2025.09.16.").as_deref(), Some("2025.09.16."));
        assert_eq!(find_time("no time here"), None);
    }

    #[test]
    fn duration_and_views_patterns() {
        assert!(is_duration("08:03"));
        assert!(is_duration("1:02:03"));
        assert!(!is_duration("8시간 전"));
        assert_eq!(find_views("조회수 449").as_deref(), Some("조회수 449"));
        assert_eq!(
            find_views("조회수 1.2만 회").as_deref(),
            Some("조회수 1.2만")
        );
    }

    #[test]
    fn query_param_decodes_proxy_src() {
        let u = "https://search.pstatic.net/common/?src=http%3A%2F%2Fblogfiles.naver.net%2Fa.jpg&type=f54_54";
        assert_eq!(
            query_param(u, "src").as_deref(),
            Some("http://blogfiles.naver.net/a.jpg")
        );
        assert_eq!(query_param(u, "type").as_deref(), Some("f54_54"));
    }

    #[test]
    fn urlencode_handles_multibyte() {
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
        assert_eq!(urlencode("네이버"), "%EB%84%A4%EC%9D%B4%EB%B2%84");
    }

    #[test]
    fn clamp_limit_bounds() {
        assert_eq!(clamp_limit(0, 50), 1);
        assert_eq!(clamp_limit(999, 50), 50);
        assert_eq!(clamp_limit(10, 50), 10);
    }
}
