//! Shared helpers: HTTP constants, HTML/entity cleanup, URL codecs and the
//! Yahoo redirect-URL decoder.

use regex::Regex;
use std::sync::LazyLock;

pub const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
pub const ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]*>").unwrap());

static NUMERIC_ENTITY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);").unwrap());

/// Strip markup, decode entities and collapse the result to a single line.
///
/// Yahoo US marks matched query terms with `<b>` tags inside snippets and
/// titles; those are noise for an LLM, as are the `\u{2}`/`\u{3}` highlight
/// markers some Yahoo properties still emit.
pub fn clean_text(s: &str) -> String {
    let without_tags = TAG_RE.replace_all(s, "");
    let without_markers = without_tags.replace(['\u{2}', '\u{3}'], "");
    let decoded = decode_entities(&without_markers);
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

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode a string (`+` is treated as a space only when `plus_space`
/// is set, i.e. for `application/x-www-form-urlencoded` query values).
pub fn urldecode(s: &str, plus_space: bool) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi * 16 + lo);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' if plus_space => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Resolve a Yahoo click-tracking URL to the real destination.
///
/// SERP links point at `https://r.search.yahoo.com/_ylt=.../RV=2/RE=.../RO=10/
/// RU=<percent-encoded destination>/RK=2/RS=...`. The destination is the
/// `RU=` segment, percent-decoded. Non-redirect URLs (and redirect URLs whose
/// `RU=` segment cannot be found) are returned unchanged.
pub fn decode_redirect_url(href: &str) -> String {
    if !href.contains("r.search.yahoo.com") {
        return href.to_string();
    }
    let Some(start) = href.find("/RU=") else {
        return href.to_string();
    };
    let rest = &href[start + 4..];
    let end = rest.find("/RK=").unwrap_or(rest.len());
    let encoded = &rest[..end];
    if encoded.is_empty() {
        return href.to_string();
    }
    urldecode(encoded, false)
}

/// Clamp a caller-supplied limit into a sane range.
pub fn clamp_limit(limit: i64, max: i64) -> usize {
    limit.clamp(1, max) as usize
}

/// Related searches for a SERP, from the page's own related-search
/// configuration (audit W-31). Yahoo embeds an `AlgoRsItem` plug-in
/// config whose `relItems` holds one group of related terms per
/// result (the same group repeated); the first group is the page's
/// related-query set. Pages without the config yield an empty list.
pub fn parse_related_queries(html: &str) -> Vec<String> {
    let Some(start) = html.find("\"relItems\":") else {
        return Vec::new();
    };
    let rest = &html[start + "\"relItems\":".len()..];
    // Bracket-match the outer JSON array, then decode it.
    let bytes = rest.as_bytes();
    let mut depth = 0i32;
    let mut end = None;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(i + 1);
                    break;
                }
            }
            _ => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    let Ok(groups) = serde_json::from_str::<Vec<Vec<String>>>(&rest[..end]) else {
        return Vec::new();
    };
    groups
        .into_iter()
        .next()
        .unwrap_or_default()
        .into_iter()
        .map(|s| clean_text(&s))
        .filter(|s| !s.is_empty())
        .collect()
}

/// Concatenated text of an element, cleaned the same way as [`clean_text`]
/// but starting from already-parsed DOM text (no tags to strip).
pub fn element_text(el: &scraper::ElementRef) -> String {
    let raw: String = el.text().collect();
    decode_entities(&raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_text_strips_tags_and_entities() {
        assert_eq!(clean_text("<b>Apple</b> &middot; Store"), "Apple · Store");
        assert_eq!(clean_text("a &amp;lt; b"), "a &lt; b");
        assert_eq!(clean_text("&#x41;&#66;"), "AB");
        assert_eq!(clean_text("multi\n  line\t text"), "multi line text");
    }

    #[test]
    fn urlencode_handles_multibyte() {
        assert_eq!(urlencode("café"), "caf%C3%A9");
        assert_eq!(urlencode("a b&c"), "a%20b%26c");
    }

    #[test]
    fn urldecode_reverses_percent_encoding() {
        assert_eq!(
            urldecode("https%3a%2f%2fexample.com%2Fa%20b", false),
            "https://example.com/a b"
        );
        assert_eq!(urldecode("a+b", true), "a b");
        assert_eq!(urldecode("a+b", false), "a+b");
        assert_eq!(urldecode("100%zz", false), "100%zz");
    }

    #[test]
    fn decode_redirect_url_extracts_ru_segment() {
        let href = "https://r.search.yahoo.com/_ylt=ABC;_ylu=XYZ/RV=2/RE=1795192589/RO=10/RU=https%3a%2f%2fasw.apple.com%2fhome/RK=2/RS=kbrR-";
        assert_eq!(decode_redirect_url(href), "https://asw.apple.com/home");
    }

    #[test]
    fn decode_redirect_url_passes_through_plain_urls() {
        assert_eq!(
            decode_redirect_url("https://example.com/x"),
            "https://example.com/x"
        );
        // A redirect host without an RU= segment is returned unchanged.
        assert_eq!(
            decode_redirect_url("https://r.search.yahoo.com/foo"),
            "https://r.search.yahoo.com/foo"
        );
    }

    #[test]
    fn related_queries_come_from_the_first_relitems_group() {
        let html =
            r#"{"relItems":[["apple music","apple store"],["apple music","apple store"]],"x":1}"#;
        assert_eq!(parse_related_queries(html), ["apple music", "apple store"]);
        assert!(parse_related_queries("<html></html>").is_empty());
    }

    #[test]
    fn clamp_limit_bounds() {
        assert_eq!(clamp_limit(0, 50), 1);
        assert_eq!(clamp_limit(999, 50), 50);
        assert_eq!(clamp_limit(10, 50), 10);
    }
}
