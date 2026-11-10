//! Search URL construction for every vertical (no network).
//!
//! The templates are the ones the Yahoo Scout app (package
//! `com.yahoo.mobile.client.android.search`) carries in its JS bundle and
//! that the live SERPs themselves link to; see the analysis report in the
//! project history. Pagination differs per vertical:
//!
//! - web: 7 results per page, `b = 1, 8, 15, …` with `pz=7`
//! - image: 60 results per page, `b = 1, 61, 121, …` (page 1 is the
//!   bare URL, the form verified live)
//! - video: 60 results per page, `b = 1, 61, 121, …`
//! - news: 10 results per page, `b = 1, 11, 21, …`

use anyhow::{anyhow, Result};

use crate::common::urlencode;

/// Verticals for which a search URL can be built.
pub const URL_VERTICALS: [&str; 5] = ["web", "image", "video", "news", "suggest"];

fn page_offset(page: i64, per_page: i64) -> i64 {
    (page.max(1) - 1) * per_page + 1
}

/// SafeSearch values for the `vm` parameter, as the SERPs' own
/// SafeSearch toggle links them (measured on the image SERP):
/// `r` = SafeSearch on, `p` = SafeSearch off (audit W-29).
pub const VM_SAFESEARCH_ON: &str = "r";
/// See [`VM_SAFESEARCH_ON`].
pub const VM_SAFESEARCH_OFF: &str = "p";

fn validate_vm(vm: &str) -> Result<&str> {
    match vm {
        "r" | "p" => Ok(vm),
        other => Err(anyhow!(
            "invalid vm (SafeSearch) value: {other} (expected \"r\" = on or \"p\" = off)"
        )),
    }
}

/// Build a search URL with an explicit SafeSearch (`vm`) setting —
/// the parameter the app attaches from the viewer's preference
/// (audit W-29). `build_search_url` sends no `vm`, leaving the
/// server/cookie default in force; this variant pins it.
pub fn build_search_url_with_vm(
    query: &str,
    vertical: &str,
    page: i64,
    vm: &str,
) -> Result<String> {
    let vm = validate_vm(vm)?;
    let base = build_search_url(query, vertical, page)?;
    if vertical.trim().eq_ignore_ascii_case("suggest") {
        return Ok(base);
    }
    Ok(format!("{base}&vm={vm}"))
}

/// Image-search filters, with the value sets the image SERP's own
/// filter menu links carry (measured from a real page, audit W-28):
///
/// - `size` (`imgsz`): `small` / `medium` / `large` / `wallpaper`
/// - `color` (`imgc`): `black`, `blue`, `brown`, `bw`, `color`,
///   `gray`, `green`, `orange`, `pink`, `purple`, `red`, `teal`,
///   `white`, `yellow`
/// - `type_` (`imgty`): `clipart` / `gif` / `linedrawing` / `photo`
///   / `transparent`
/// - `layout` (`imga`): `square` / `tall` / `wide`
/// - `feature` (`imgf`): `face` / `portrait` / `nonprofit`
/// - `license` (`imgl`): `cc` / `fmsu` / `fmsuc` / `fsu` / `fsuc` /
///   `pd` (usage-rights codes, kept verbatim)
/// - `time` (`imgt`): `day` / `week` / `month` / `year`
/// - `safesearch` (`vm`): `r` (on) / `p` (off)
///
/// Any value outside its measured set is an error, never silently
/// sent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageFilters {
    pub size: Option<String>,
    pub color: Option<String>,
    pub type_: Option<String>,
    pub layout: Option<String>,
    pub feature: Option<String>,
    pub license: Option<String>,
    pub time: Option<String>,
    pub safesearch: Option<String>,
}

fn check_filter(name: &str, value: &str, allowed: &[&str]) -> Result<()> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(anyhow!(
            "invalid image filter {name}: {value} (expected one of {})",
            allowed.join(", ")
        ))
    }
}

impl ImageFilters {
    /// Validate every set field against its measured value set.
    pub fn validate(&self) -> Result<()> {
        if let Some(v) = &self.size {
            check_filter("size", v, &["small", "medium", "large", "wallpaper"])?;
        }
        if let Some(v) = &self.color {
            check_filter(
                "color",
                v,
                &[
                    "black", "blue", "brown", "bw", "color", "gray", "green", "orange", "pink",
                    "purple", "red", "teal", "white", "yellow",
                ],
            )?;
        }
        if let Some(v) = &self.type_ {
            check_filter(
                "type",
                v,
                &["clipart", "gif", "linedrawing", "photo", "transparent"],
            )?;
        }
        if let Some(v) = &self.layout {
            check_filter("layout", v, &["square", "tall", "wide"])?;
        }
        if let Some(v) = &self.feature {
            check_filter("feature", v, &["face", "portrait", "nonprofit"])?;
        }
        if let Some(v) = &self.license {
            check_filter("license", v, &["cc", "fmsu", "fmsuc", "fsu", "fsuc", "pd"])?;
        }
        if let Some(v) = &self.time {
            check_filter("time", v, &["day", "week", "month", "year"])?;
        }
        if let Some(v) = &self.safesearch {
            validate_vm(v)?;
        }
        Ok(())
    }

    /// The `&param=value` suffix for the set fields (empty when no
    /// filter is set). Call [`ImageFilters::validate`] first.
    fn query_suffix(&self) -> String {
        let mut out = String::new();
        for (param, value) in [
            ("imgsz", &self.size),
            ("imgc", &self.color),
            ("imgty", &self.type_),
            ("imga", &self.layout),
            ("imgf", &self.feature),
            ("imgl", &self.license),
            ("imgt", &self.time),
            ("vm", &self.safesearch),
        ] {
            if let Some(v) = value {
                out.push_str(&format!("&{param}={}", urlencode(v)));
            }
        }
        out
    }
}

/// Build an image-search URL with filters applied (audit W-28).
pub fn build_image_search_url(query: &str, page: i64, filters: &ImageFilters) -> Result<String> {
    filters.validate()?;
    let base = build_search_url(query, "image", page)?;
    Ok(format!("{base}{}", filters.query_suffix()))
}

/// Build the first-page (or `page`-th page) search URL for a vertical.
///
/// `vertical` is one of `web`, `image`, `video`, `news`, `suggest`
/// (case-insensitive).
pub fn build_search_url(query: &str, vertical: &str, page: i64) -> Result<String> {
    let q = urlencode(query.trim());
    let page = page.max(1);
    match vertical.trim().to_lowercase().as_str() {
        "web" => {
            let b = page_offset(page, crate::web::PER_PAGE);
            Ok(format!(
                "https://search.yahoo.com/search?p={q}&b={b}&pz={}",
                crate::web::PER_PAGE
            ))
        }
        "image" => {
            if page <= 1 {
                Ok(format!(
                    "https://images.search.yahoo.com/search/images?p={q}"
                ))
            } else {
                let b = page_offset(page, crate::image::PER_PAGE);
                Ok(format!(
                    "https://images.search.yahoo.com/search/images?p={q}&b={b}"
                ))
            }
        }
        "video" => {
            let b = page_offset(page, crate::video::PER_PAGE);
            Ok(format!(
                "https://video.search.yahoo.com/search/video?p={q}&b={b}"
            ))
        }
        "news" => {
            let b = page_offset(page, crate::news::PER_PAGE);
            Ok(format!("https://news.search.yahoo.com/search?p={q}&b={b}"))
        }
        "suggest" => Ok(format!(
            "https://search.yahoo.com/sugg/gossip/gossip-us-ura/?command={q}&output=json"
        )),
        other => Err(anyhow!(
            "unknown vertical: {other} (expected one of {})",
            URL_VERTICALS.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_url_uses_b_and_pz_paging() {
        assert_eq!(
            build_search_url("apple pie", "web", 1).unwrap(),
            "https://search.yahoo.com/search?p=apple%20pie&b=1&pz=7"
        );
        assert_eq!(
            build_search_url("apple", "web", 3).unwrap(),
            "https://search.yahoo.com/search?p=apple&b=15&pz=7"
        );
    }

    #[test]
    fn video_and_news_urls_use_their_own_offsets() {
        assert_eq!(
            build_search_url("apple", "video", 2).unwrap(),
            "https://video.search.yahoo.com/search/video?p=apple&b=61"
        );
        assert_eq!(
            build_search_url("apple", "news", 2).unwrap(),
            "https://news.search.yahoo.com/search?p=apple&b=11"
        );
    }

    #[test]
    fn image_url_pages_with_b_offset() {
        assert_eq!(
            build_search_url("apple", "image", 1).unwrap(),
            "https://images.search.yahoo.com/search/images?p=apple"
        );
        assert_eq!(
            build_search_url("apple", "image", 2).unwrap(),
            "https://images.search.yahoo.com/search/images?p=apple&b=61"
        );
    }

    #[test]
    fn suggest_url_is_the_gossip_endpoint() {
        assert_eq!(
            build_search_url("app", "suggest", 1).unwrap(),
            "https://search.yahoo.com/sugg/gossip/gossip-us-ura/?command=app&output=json"
        );
    }

    #[test]
    fn unknown_vertical_is_an_error() {
        assert!(build_search_url("x", "maps", 1).is_err());
    }

    #[test]
    fn vm_builder_pins_safesearch() {
        assert_eq!(
            build_search_url_with_vm("apple", "web", 1, "r").unwrap(),
            "https://search.yahoo.com/search?p=apple&b=1&pz=7&vm=r"
        );
        assert!(build_search_url_with_vm("apple", "web", 1, "x").is_err());
    }

    #[test]
    fn image_filters_build_measured_params() {
        let filters = ImageFilters {
            size: Some("large".to_string()),
            color: Some("bw".to_string()),
            type_: Some("photo".to_string()),
            time: Some("week".to_string()),
            safesearch: Some("r".to_string()),
            ..Default::default()
        };
        let url = build_image_search_url("apple", 1, &filters).unwrap();
        assert_eq!(
            url,
            "https://images.search.yahoo.com/search/images?p=apple&imgsz=large&imgc=bw&imgty=photo&imgt=week&vm=r"
        );
        let bad = ImageFilters {
            size: Some("huge".to_string()),
            ..Default::default()
        };
        assert!(build_image_search_url("apple", 1, &bad).is_err());
    }
}
