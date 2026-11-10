//! Search-URL generation — faithful port of Python
//! `baidu_search.urlbuild` (APK `bq0.m` SearchUrlGenerator +
//! BaiduIdentityManager.processWebSearchUrl).
//!
//! Device-derived parameters (cuid, cua, pkgname, …) are never
//! added, exactly as in the validated Python implementation.

use crate::antct::make_ant_ct;

pub const APP_VERSION: &str = "15.57.1.10";
pub const USER_AGENT: &str = "Dalvik/2.1.0 (Linux; U; Android 13; SM-S918B Build/TP1A.220624.014) baiduboxapp/15.57.1.10 (Baidu; P1 13)";
pub const DEFAULT_WEB_TEMPLATE: &str =
    "http://m.baidu.com/s?tn=zbios&pu=sz%401320_480&bd_page_type=1&word=";
pub const TNCONFIG_VALUE: &str = "1020164n";

/// Results per page in the `pn` offset scheme used by server-issued
/// page URLs (page 2 is `pn=10`, page 3 is `pn=20`, …), as observed
/// in the saved `tianqi_pn10.html` / `tianqi_pn20.html` pages.
pub const PN_PER_PAGE: i64 = 10;

/// Build the search URL for a 1-based `page` number directly
/// (audit W-06). Page 1 is the plain initial URL; deeper pages add
/// `pn=(page-1)*10`, the same offset the server-issued next-page
/// URLs carry. Returns `None` for `page < 1`.
pub fn build_search_url_for_page(query: &str, page: i64, now_ms: i64) -> Option<String> {
    if page < 1 {
        return None;
    }
    let url = build_search_url(query, now_ms);
    if page == 1 {
        return Some(url);
    }
    Some(add_param(
        &url,
        "pn",
        Some(&((page - 1) * PN_PER_PAGE).to_string()),
    ))
}

/// Vertical (tab) search kinds the app exposes through its
/// `sf/vsearch` URL template, mapped to the template's `pd` value
/// (APK bundled configuration / DEX strings; see
/// `baidu-search-analysis/native-protocol-audit.md` §4).
pub fn vertical_pd(vertical: &str) -> Option<&'static str> {
    Some(match vertical {
        "video" => "video",
        "image" => "image_content",
        "realtime" => "realtime",
        "wenda" => "wenda_tab",
        _ => return None,
    })
}

/// Build a vertical search URL:
/// `https://m.baidu.com/sf/vsearch?tn=vsearch&pd=<pd>&word=<query>`
/// (audit W-02). Returns `None` for an unknown vertical name.
pub fn build_vertical_search_url(vertical: &str, query: &str) -> Option<String> {
    let pd = vertical_pd(vertical)?;
    Some(format!(
        "https://m.baidu.com/sf/vsearch?tn=vsearch&pd={}&word={}",
        pd,
        java_urlencode(query)
    ))
}

/// java.net.URLEncoder.encode(s, "UTF-8") semantics.
pub fn java_urlencode(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii() && (ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '*' | '_')) {
            out.push(ch);
        } else if ch == ' ' {
            out.push('+');
        } else {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// android.net.Uri.encode(s) semantics
/// (allowed: A-Za-z0-9 and `_-!.~'()*`).
pub fn uri_encode(s: &str) -> String {
    const ALLOW: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-!.~'()*";
    let mut out = String::new();
    for ch in s.chars() {
        if ALLOW.contains(ch) {
            out.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for b in ch.encode_utf8(&mut buf).as_bytes() {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

/// UrlUtil.addParam: append `key=value` verbatim; None adds nothing.
pub fn add_param(url: &str, key: &str, value: Option<&str>) -> String {
    match value {
        None => url.to_string(),
        Some(v) => {
            let sep = if url.contains('?') { "&" } else { "?" };
            format!("{}{}{}={}", url, sep, key, v)
        }
    }
}

fn get_pu(url: &str) -> Option<String> {
    // Python: re.search(r"[?&]pu=([^&]*)", url)
    let bytes = url.as_bytes();
    let mut i = 0usize;
    while i + 4 <= bytes.len() {
        if (bytes[i] == b'?' || bytes[i] == b'&') && &url[i + 1..i + 4] == "pu=" {
            let start = i + 4;
            let end = url[start..]
                .find('&')
                .map(|j| start + j)
                .unwrap_or(url.len());
            return Some(url[start..end].to_string());
        }
        i += 1;
    }
    None
}

/// BaiduIdentityManager.o(url, key, value) reproduction.
pub fn pu_merge(url: &str, key: &str, value: &str) -> String {
    let field = get_pu(url);
    match field {
        None => add_param(url, "pu", Some(&uri_encode(&format!("{}@{}", key, value)))),
        Some(f) if f.is_empty() => {
            add_param(url, "pu", Some(&uri_encode(&format!("{}@{}", key, value))))
        }
        Some(f) => {
            let probe = format!("{}@", key);
            if f.contains(&uri_encode(&probe)) || f.contains(&probe) {
                return url.to_string();
            }
            let needle = format!("pu={}", f);
            let replacement = format!("pu={}{}", f, uri_encode(&format!(",{}{}", probe, value)));
            // Python str.replace(old, new, 1): first occurrence only.
            match url.find(&needle) {
                Some(idx) => {
                    let mut out = String::with_capacity(url.len() + replacement.len());
                    out.push_str(&url[..idx]);
                    out.push_str(&replacement);
                    out.push_str(&url[idx + needle.len()..]);
                    out
                }
                None => url.to_string(),
            }
        }
    }
}

/// processWebSearchUrl static part: osname, then cfrom, in APK order.
pub fn process_web_search_url_static(url: &str) -> String {
    let url = pu_merge(url, "osname", "baiduboxapp");
    pu_merge(&url, "cfrom", TNCONFIG_VALUE)
}

/// bq0.m.q(): fixed keys only.
pub fn build_t_samp() -> String {
    let items = [
        ("tcspeedup", "0"),
        ("fixhttpdns", "0"),
        ("load_async_k", "0"),
        ("Prefetch_NA", "101"),
    ];
    items
        .iter()
        .map(|(k, v)| format!("{}_{}", k, v))
        .collect::<Vec<_>>()
        .join("-")
}

/// bq0.m.d(query, null, null). `now_ms` is milliseconds; the ant_ct
/// `t` is *seconds* (now_ms // 1000), the KWD placeholder keeps ms.
pub fn build_search_url(query: &str, now_ms: i64) -> String {
    let placeholder = format!("KWD{}", now_ms);
    let tpl = format!("{}{}", DEFAULT_WEB_TEMPLATE, placeholder);
    let tpl = process_web_search_url_static(&tpl);
    let tpl = if let Some(rest) = tpl.strip_prefix("http://") {
        format!("https://{}", rest)
    } else {
        tpl
    };
    let url = tpl.replace(&placeholder, &java_urlencode(query));
    let url = pu_merge(&url, "csrc", "app_mainbox_txt");
    let ant_ct = make_ant_ct(now_ms / 1000, query);
    let url = add_param(&url, "ant_ct", Some(&ant_ct));
    add_param(&url, "t_samp", Some(&build_t_samp()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected values measured from the Python implementation
    // (offline, fixed clock) on 2026-11-04 — do not regenerate by hand.
    #[test]
    fn python_vectors_full_url() {
        assert_eq!(
            build_search_url("天气", 1700000000000),
            "https://m.baidu.com/s?tn=zbios&pu=sz%401320_480%2Cosname%40baiduboxapp%2Ccfrom%401020164n%2Ccsrc%40app_mainbox_txt&bd_page_type=1&word=%E5%A4%A9%E6%B0%94&ant_ct=VXS6k29dOFmVHg7SWvmG3J6Y8CeXI2eFCwutMExxN6M5c%2BIoEt0zOUks78JD6IrB&t_samp=tcspeedup_0-fixhttpdns_0-load_async_k_0-Prefetch_NA_101"
        );
        assert_eq!(
            build_search_url("札幌 天気", 1700000000000),
            "https://m.baidu.com/s?tn=zbios&pu=sz%401320_480%2Cosname%40baiduboxapp%2Ccfrom%401020164n%2Ccsrc%40app_mainbox_txt&bd_page_type=1&word=%E6%9C%AD%E5%B9%8C+%E5%A4%A9%E6%B0%97&ant_ct=2qmbIEq7RXPuCtxKB3ILBomwBdFGMLm3NgyIGzg0bVlsdW4KqIZ86jN5yS6Vu9Kk&t_samp=tcspeedup_0-fixhttpdns_0-load_async_k_0-Prefetch_NA_101"
        );
        assert_eq!(
            build_search_url("weather", 1700000000000),
            "https://m.baidu.com/s?tn=zbios&pu=sz%401320_480%2Cosname%40baiduboxapp%2Ccfrom%401020164n%2Ccsrc%40app_mainbox_txt&bd_page_type=1&word=weather&ant_ct=qQ38baUcZUWjQJQmu%2F2OafwiFJYx3MbRsJQUsZOhPVEyubaezgiYLPPiKDEVUYvc&t_samp=tcspeedup_0-fixhttpdns_0-load_async_k_0-Prefetch_NA_101"
        );
        assert_eq!(
            build_search_url("C++ 入門 & テスト?", 1700000000999),
            "https://m.baidu.com/s?tn=zbios&pu=sz%401320_480%2Cosname%40baiduboxapp%2Ccfrom%401020164n%2Ccsrc%40app_mainbox_txt&bd_page_type=1&word=C%2B%2B+%E5%85%A5%E9%96%80+%26+%E3%83%86%E3%82%B9%E3%83%88%3F&ant_ct=%2F8fMhYsgWuoRBrClsOEyy5F2HPmCTTW%2F5nlwLrrsbArD%2FYX2ljky3Rzixqo4N5YD&t_samp=tcspeedup_0-fixhttpdns_0-load_async_k_0-Prefetch_NA_101"
        );
        assert_eq!(
            build_search_url("天气", 1761400000123),
            "https://m.baidu.com/s?tn=zbios&pu=sz%401320_480%2Cosname%40baiduboxapp%2Ccfrom%401020164n%2Ccsrc%40app_mainbox_txt&bd_page_type=1&word=%E5%A4%A9%E6%B0%94&ant_ct=ToBopGssBSqtKn0Ma1wi6fk4BYjZYGt0%2F8I%2B%2F%2FlIsWI8Osxo8l%2BsihY3WAyvpo4P&t_samp=tcspeedup_0-fixhttpdns_0-load_async_k_0-Prefetch_NA_101"
        );
    }

    #[test]
    fn encoders_match_python() {
        assert_eq!(java_urlencode("天气 test"), "%E5%A4%A9%E6%B0%94+test");
        assert_eq!(uri_encode("天气 test"), "%E5%A4%A9%E6%B0%94%20test");
        assert_eq!(java_urlencode("a+b"), "a%2Bb");
        assert_eq!(java_urlencode("~-._*x"), "%7E-._*x");
        assert_eq!(uri_encode("~-._*x"), "~-._*x");
        assert_eq!(java_urlencode("it's (test)!"), "it%27s+%28test%29%21");
        assert_eq!(uri_encode("it's (test)!"), "it's%20(test)!");
        assert_eq!(java_urlencode(" "), "+");
        assert_eq!(uri_encode(" "), "%20");
        assert_eq!(java_urlencode(""), "");
    }

    #[test]
    fn url_shape_and_forbidden_params() {
        let url = build_search_url("天气", 1700000000000);
        assert!(url.starts_with("https://m.baidu.com/s?tn=zbios&"));
        assert!(url.contains("word=%E5%A4%A9%E6%B0%94"));
        assert!(url.contains("ant_ct="));
        assert!(url.contains("t_samp="));
        for forbidden in ["cuid=", "cua=", "pkgname="] {
            assert!(
                !url.contains(forbidden),
                "forbidden {} in {}",
                forbidden,
                url
            );
        }
        assert_eq!(
            USER_AGENT,
            "Dalvik/2.1.0 (Linux; U; Android 13; SM-S918B Build/TP1A.220624.014) baiduboxapp/15.57.1.10 (Baidu; P1 13)"
        );
        assert_eq!(
            build_t_samp(),
            "tcspeedup_0-fixhttpdns_0-load_async_k_0-Prefetch_NA_101"
        );
    }

    #[test]
    fn page_number_urls_use_pn_offsets() {
        let p1 = build_search_url_for_page("天气", 1, 1700000000000).unwrap();
        assert_eq!(p1, build_search_url("天气", 1700000000000));
        assert!(!p1.contains("pn="));
        let p3 = build_search_url_for_page("天气", 3, 1700000000000).unwrap();
        assert!(p3.ends_with("&pn=20"), "{}", p3);
        assert_eq!(build_search_url_for_page("天气", 0, 1700000000000), None);
        assert_eq!(build_search_url_for_page("天气", -2, 1700000000000), None);
    }

    #[test]
    fn vertical_urls_match_app_template() {
        assert_eq!(
            build_vertical_search_url("video", "猫 视频").as_deref(),
            Some("https://m.baidu.com/sf/vsearch?tn=vsearch&pd=video&word=%E7%8C%AB+%E8%A7%86%E9%A2%91")
        );
        assert_eq!(
            build_vertical_search_url("image", "cat").as_deref(),
            Some("https://m.baidu.com/sf/vsearch?tn=vsearch&pd=image_content&word=cat")
        );
        assert_eq!(
            build_vertical_search_url("realtime", "news").as_deref(),
            Some("https://m.baidu.com/sf/vsearch?tn=vsearch&pd=realtime&word=news")
        );
        assert_eq!(
            build_vertical_search_url("wenda", "why").as_deref(),
            Some("https://m.baidu.com/sf/vsearch?tn=vsearch&pd=wenda_tab&word=why")
        );
        assert_eq!(build_vertical_search_url("maps", "x"), None);
    }

    #[test]
    fn pu_merge_and_add_param_match_python() {
        assert_eq!(
            pu_merge(
                "http://m.baidu.com/s?tn=zbios&pu=sz%401320_480&bd_page_type=1&word=KWD1",
                "osname",
                "baiduboxapp"
            ),
            "http://m.baidu.com/s?tn=zbios&pu=sz%401320_480%2Cosname%40baiduboxapp&bd_page_type=1&word=KWD1"
        );
        assert_eq!(
            pu_merge("http://x/s?a=1", "k", "v"),
            "http://x/s?a=1&pu=k%40v"
        );
        assert_eq!(add_param("http://x/s", "k", Some("v")), "http://x/s?k=v");
        assert_eq!(add_param("http://x/s?a=1", "k", None), "http://x/s?a=1");
    }
}
