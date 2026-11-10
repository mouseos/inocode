//! Pagination — port of Python `baidu_parser.pagination`
//! (extraction from `div.new-pagenav`) and `baidu_search.pagination`
//! (client-side validation of server-issued next URLs).
//!
//! Server-issued URLs are kept verbatim; the client never rebuilds
//! a `pn=` URL and never normalises the string.

use crate::dom::{self, NRef};
use crate::errors::{BaiduError, KIND_INVALID_NEXT_URL, KIND_PAGINATION_CYCLE};
use crate::models::PageInfo;
use crate::urls as urltools;
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

fn nowpage_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"第\s*(\d+)\s*页").unwrap())
}

fn first_anchor<'a>(nav: NRef<'a>, cls: &str) -> Option<NRef<'a>> {
    dom::descendant_elements(nav)
        .into_iter()
        .find(|n| dom::tag_name(*n).as_deref() == Some("a") && dom::class_contains(*n, cls))
}

/// Python `baidu_parser.pagination.extract_pagination`.
pub fn extract_pagination(root: NRef<'_>, source_url: Option<&str>) -> PageInfo {
    let mut page = PageInfo::default();
    let navs: Vec<NRef> = dom::all_elements(root)
        .into_iter()
        .filter(|n| dom::class_contains(*n, "new-pagenav"))
        .collect();
    let nav = navs.first().copied();

    let mut next_url: Option<String> = None;
    let mut prev_url: Option<String> = None;
    let mut first_url: Option<String> = None;
    let mut page_number: Option<i64> = None;

    if let Some(nav) = nav {
        let mut anchor = first_anchor(nav, "new-nextpage");
        if anchor.is_none() {
            anchor = first_anchor(nav, "new-nextpage-only");
        }
        if let Some(a) = anchor {
            next_url = urltools::resolve(dom::attr(a, "href").as_deref(), source_url);
        }
        if let Some(a) = first_anchor(nav, "new-prepage") {
            prev_url = urltools::resolve(dom::attr(a, "href").as_deref(), source_url);
        }
        if let Some(a) = first_anchor(nav, "new-firstpage") {
            first_url = urltools::resolve(dom::attr(a, "href").as_deref(), source_url);
        }
        let labels: Vec<NRef> = dom::descendant_elements(nav)
            .into_iter()
            .filter(|n| dom::class_contains(*n, "new-nowpage"))
            .collect();
        if !labels.is_empty() {
            let text = dom::raw_text(labels[0]);
            if let Some(m) = nowpage_re().captures(&text) {
                page_number = m.get(1).and_then(|g| g.as_str().parse::<i64>().ok());
            }
        }
    }

    let source_pn = urltools::pn_of(source_url);
    let page_number = match page_number {
        Some(n) => n,
        None => match source_pn {
            Some(pn) if pn != 0 => pn / 10 + 1,
            _ => 1,
        },
    };

    page.page_number = page_number;
    page.pn = source_pn.unwrap_or((page_number - 1) * 10);
    page.is_first_page = page.pn == 0;
    page.next_url = next_url;
    page.previous_url = prev_url;
    page.first_url = first_url;
    page.next_pn = urltools::pn_of(page.next_url.as_deref());
    page.previous_pn = urltools::pn_of(page.previous_url.as_deref());
    if page.previous_url.is_some() && page.previous_pn.is_none() {
        page.previous_pn = Some(0); // server omits pn for the first page
    }
    page
}

// ------------------------------------------------------- client-side

const ALLOWED_SCHEMES: &[&str] = &["http", "https"];

/// Return `url` unchanged, or raise InvalidNextUrlError (Python
/// `baidu_search.pagination.validate_next_url`).
pub fn validate_next_url(url: &str) -> Result<String, BaiduError> {
    if url.is_empty() {
        return Err(BaiduError::new(
            KIND_INVALID_NEXT_URL,
            "next-page URL is empty or not a string",
        ));
    }
    let parsed = url::Url::parse(url);
    let (scheme, host) = match &parsed {
        Ok(u) => (
            u.scheme().to_lowercase(),
            u.host_str().map(|h| h.to_lowercase()),
        ),
        Err(_) => {
            // Python urlparse still yields a scheme for inputs like
            // "javascript:void(0)"; mirror that for the message.
            let scheme = url.split(':').next().unwrap_or("").to_lowercase();
            (scheme, None)
        }
    };
    if !ALLOWED_SCHEMES.contains(&scheme.as_str()) {
        // Python message uses repr(): 'javascript'
        return Err(BaiduError::new(
            KIND_INVALID_NEXT_URL,
            format!("next-page URL has unexpected scheme: '{}'", scheme),
        )
        .with_url(url.to_string()));
    }
    let host_ok = match &host {
        Some(h) => {
            h == urltools::BAIDU_SUFFIX || h.ends_with(&format!(".{}", urltools::BAIDU_SUFFIX))
        }
        None => false,
    };
    if !host_ok {
        // Python message uses repr(): 'evil.example.com' or None
        let host_repr = match &host {
            Some(h) => format!("'{}'", h),
            None => "None".to_string(),
        };
        return Err(BaiduError::new(
            KIND_INVALID_NEXT_URL,
            format!("next-page URL has unexpected host: {}", host_repr),
        )
        .with_url(url.to_string()));
    }
    Ok(url.to_string())
}

/// Exact-string set of already-fetched URLs (cycle detection).
#[derive(Default)]
pub struct SeenUrls {
    seen: HashSet<String>,
}

impl SeenUrls {
    pub fn new() -> Self {
        SeenUrls {
            seen: HashSet::new(),
        }
    }
    pub fn add(&mut self, url: &str) {
        self.seen.insert(url.to_string());
    }
    pub fn contains(&self, url: &str) -> bool {
        self.seen.contains(url)
    }
    pub fn check(&self, url: &str) -> Result<(), BaiduError> {
        if self.seen.contains(url) {
            return Err(BaiduError::new(
                KIND_PAGINATION_CYCLE,
                "pagination cycle: URL re-appeared",
            )
            .with_url(url.to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_verbatim_and_hosts() {
        let url = "https://m.baidu.com/s?pn=10&ant_ct=Ab%2FCd%3D";
        assert_eq!(validate_next_url(url).unwrap(), url);
        assert_eq!(
            validate_next_url("https://baidu.com.evil.example/s")
                .unwrap_err()
                .kind,
            KIND_INVALID_NEXT_URL
        );
        assert_eq!(
            validate_next_url("ftp://m.baidu.com/s").unwrap_err().kind,
            KIND_INVALID_NEXT_URL
        );
        assert_eq!(
            validate_next_url("").unwrap_err().kind,
            KIND_INVALID_NEXT_URL
        );
        assert_eq!(
            validate_next_url("javascript:void(0)").unwrap_err().kind,
            KIND_INVALID_NEXT_URL
        );
    }

    #[test]
    fn cycle_detection() {
        let mut seen = SeenUrls::new();
        seen.add("https://m.baidu.com/s?pn=10");
        assert!(seen.contains("https://m.baidu.com/s?pn=10"));
        assert_eq!(
            seen.check("https://m.baidu.com/s?pn=10").unwrap_err().kind,
            KIND_PAGINATION_CYCLE
        );
        assert!(seen.check("https://m.baidu.com/s?pn=20").is_ok());
    }
}
