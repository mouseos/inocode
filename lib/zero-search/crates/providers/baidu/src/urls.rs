//! URL handling — faithful port of Python `baidu_parser.urls`.
//!
//! URLs are kept verbatim as found in the HTML; relative hrefs are
//! resolved against the page URL (urljoin semantics) — the only
//! transformation applied.

use url::Url;

pub const BAIDU_SUFFIX: &str = "baidu.com";

/// Percent-decode (`unquote` semantics: `+` stays `+`).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Percent-decode for query strings (parse_qs semantics: `+` = space).
fn query_decode(s: &str) -> String {
    percent_decode(&s.replace('+', " "))
}

/// Resolve a possibly-relative href against the page URL (urljoin).
pub fn resolve(href: Option<&str>, base: Option<&str>) -> Option<String> {
    let href = href?;
    let href = href.trim();
    if href.is_empty() {
        return None;
    }
    match base {
        Some(b) => {
            // Absolute URLs (with scheme) are kept verbatim, as
            // urljoin returns them unchanged.
            if has_scheme(href) {
                return Some(href.to_string());
            }
            match Url::options().base_url(Some(&parse_lenient(b))).parse(href) {
                Ok(u) => Some(u.to_string()),
                Err(_) => Some(href.to_string()),
            }
        }
        None => Some(href.to_string()),
    }
}

fn has_scheme(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
            return false;
        }
    }
    false
}

fn parse_lenient(u: &str) -> Url {
    Url::parse(u).unwrap_or_else(|_| Url::parse("https://m.baidu.com/").unwrap())
}

pub fn host_of(url: Option<&str>) -> Option<String> {
    let u = url?;
    Url::parse(u).ok()?.host_str().map(|h| h.to_lowercase())
}

pub fn is_baidu_host(host: Option<&str>) -> bool {
    match host {
        Some(h) => h == BAIDU_SUFFIX || h.ends_with(&format!(".{}", BAIDU_SUFFIX)),
        None => false,
    }
}

/// Classify the URL a result link actually points to. See Python
/// docstring: baidu_redirect / baidu_app_scheme / baidu_internal /
/// direct_external / other_scheme.
pub fn classify_click_url(url: Option<&str>) -> Option<String> {
    let url = url?;
    if url.is_empty() {
        return None;
    }
    let scheme = url.split(':').next().unwrap_or("").to_lowercase();
    // Python urlparse: scheme is only recognised when the part before
    // ':' is a valid scheme; otherwise scheme is "" -> other_scheme.
    let scheme = if has_scheme(url) {
        scheme
    } else {
        String::new()
    };
    if scheme == "baiduboxapp" {
        return Some("baidu_app_scheme".to_string());
    }
    if scheme == "http" || scheme == "https" {
        let host = host_of(Some(url));
        if is_baidu_host(host.as_deref()) {
            let path = Url::parse(url)
                .map(|u| u.path().to_string())
                .unwrap_or_default();
            let trimmed = path.trim_end_matches('/');
            if trimmed.ends_with("/tc") {
                return Some("baidu_redirect".to_string());
            }
            return Some("baidu_internal".to_string());
        }
        return Some("direct_external".to_string());
    }
    Some("other_scheme".to_string())
}

pub fn is_baidu_redirect(url: Option<&str>) -> bool {
    classify_click_url(url).as_deref() == Some("baidu_redirect")
}

fn query_params(url: &str) -> Vec<(String, String)> {
    let q = match url.split_once('?') {
        Some((_, q)) => q,
        None => return Vec::new(),
    };
    let q = q.split('#').next().unwrap_or(q);
    let mut out = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        // parse_qs drops blank values by default.
        if v.is_empty() {
            continue;
        }
        out.push((query_decode(k), query_decode(v)));
    }
    out
}

/// Extract the `pn` query parameter of a server-issued page URL.
pub fn pn_of(url: Option<&str>) -> Option<i64> {
    let url = url?;
    for (k, v) in query_params(url) {
        if k == "pn" {
            return v.parse::<i64>().ok();
        }
    }
    None
}

/// Extract the decoded `word` (query) parameter of a page URL.
pub fn word_of(url: Option<&str>) -> Option<String> {
    let url = url?;
    for (k, v) in query_params(url) {
        if k == "word" {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_classification_matches_python() {
        assert_eq!(
            classify_click_url(Some("https://m.baidu.com/from=0/x/t=zbios/l=1/tc?a=1")).as_deref(),
            Some("baidu_redirect")
        );
        assert_eq!(
            classify_click_url(Some("baiduboxapp://v1/browser/open?url=x")).as_deref(),
            Some("baidu_app_scheme")
        );
        assert_eq!(
            classify_click_url(Some("https://baike.baidu.com/item/x")).as_deref(),
            Some("baidu_internal")
        );
        assert_eq!(
            classify_click_url(Some("https://example.com/")).as_deref(),
            Some("direct_external")
        );
        assert_eq!(classify_click_url(None), None);
        assert_eq!(pn_of(Some("https://m.baidu.com/s?pn=30&word=x")), Some(30));
        assert_eq!(pn_of(Some("https://m.baidu.com/s?word=x")), None);
        assert_eq!(
            word_of(Some("https://m.baidu.com/s?word=%E5%A4%A9%E6%B0%94&pn=10")).as_deref(),
            Some("天气")
        );
    }

    #[test]
    fn resolve_matches_python() {
        let base = Some("https://m.baidu.com/s?word=test");
        assert_eq!(
            resolve(Some("/s?word=x"), base).as_deref(),
            Some("https://m.baidu.com/s?word=x")
        );
        assert_eq!(resolve(None, base), None);
        assert_eq!(resolve(Some(""), base), None);
        assert_eq!(
            resolve(Some("https://example.com/a?b=1"), base).as_deref(),
            Some("https://example.com/a?b=1")
        );
    }
}
