//! Amazon.co.jp (Amazon Shopping app) provider.
//!
//! The app has no native search API: search and product pages
//! are WebView loads. This provider reproduces what the app's
//! WebView sends — the app-format User-Agent
//! (`Amazon.com/<version> (Android/<release>/<model>)`, not a
//! browser UA), the pre-navigation cookies the app syncs into the
//! WebView (`mobile-device-info`, `amzn-app-id`), and plain
//! `/s` / `/dp/<ASIN>` URLs — and extracts the product data from
//! the server-rendered HTML. No browser engine is involved.
//! `amzn-app-ctxt` is deliberately not sent: live verification
//! succeeded without it, and its value embeds device serials.
//!
//! The fetches themselves are made with libcurl (the `curl`
//! crate): the same requests sent through a different HTTP
//! client stack are answered with a robot check (HTTP 503),
//! while libcurl's requests — the requests the curl tool makes —
//! are served. Only this provider uses libcurl; the rest of the
//! workspace stays on reqwest.
//!
//! Note: Amazon serves these pages only to Japan-side access in
//! practice; from other egress the request typically times out
//! or is answered with a robot check.

use anyhow::{anyhow, Result};
use curl::easy::{Easy, HttpVersion, List};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

const HOST: &str = "https://www.amazon.co.jp";
const USER_AGENT: &str = "Amazon.com/32.20.2.100 (Android/14/Pixel 8)";
/// `mobile-device-info`: `dpi:<dpi>|w:<width>|h:<height>` built
/// from the device profile, as the app generates it.
const COOKIE_DEVICE_INFO: &str = "dpi:420|w:1080|h:2400";
/// `amzn-app-id`: `<app name>/<versionName>/<build_revision>`.
/// App name from the app's strings, build revision from the
/// APK's `res/raw/build_revision.txt` (32.20.2.100).
const COOKIE_APP_ID: &str = "AmazonShopping/32.20.2.100/18.0.700537.0";

/// Sort values for the `s` parameter (Amazon web vocabulary):
/// empty / `relevancerank`, `price-asc-rank`, `price-desc-rank`,
/// `review-rank`, `date-desc-rank`.
pub const SORTS: &[&str] = &[
    "relevancerank",
    "price-asc-rank",
    "price-desc-rank",
    "review-rank",
    "date-desc-rank",
];

// ---------------------------------------------------------------------------
// Shared result types (same shape as the other shopping providers)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Price {
    pub value: i64,
    pub currency: String,
    pub display: String,
}

impl Price {
    fn yen(v: i64) -> Self {
        Self {
            value: v,
            currency: "JPY".into(),
            display: format!("¥{v}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Item {
    /// The ASIN.
    pub product_id: String,
    pub title: String,
    pub url: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sponsored: Option<bool>,
    /// Detail: Amazon Points earned on the purchase, when the
    /// page states them.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub points: Option<i64>,
    /// Detail: product description (the feature-bullets text,
    /// one bullet per line), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Detail: availability text as served (e.g. "在庫あり。").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
    /// Detail: the seller's name, when the page names one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seller: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub provider: String,
    pub ok: bool,
    pub query: String,
    pub page: i32,
    pub limit: i32,
    pub sort: String,
    pub items: Vec<Item>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemResult {
    pub provider: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<Item>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Search URL construction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    /// Web-side `page` parameter.
    pub page: i32,
    /// Client-side cap on returned items.
    pub limit: i32,
    /// `s` parameter; see [`SORTS`]. Empty omits it.
    pub sort: String,
    /// `rank` parameter (the app's sort hand-off field).
    pub rank: String,
    /// Department alias → rh `i:`.
    pub department: String,
    /// Category node id → rh `n:`.
    pub category_node: String,
    /// Price band in JPY → rh `p_36:<min>-<max>`. The wire
    /// values are the JPY amounts ×100 (the refinement is in
    /// 1/100-yen units — measured live: an unscaled 3000-5000 band
    /// behaves as ¥30–¥50 and returns items priced in that range).
    pub price_min: Option<i64>,
    pub price_max: Option<i64>,
    /// Prime-eligible only → rh `p_85`.
    pub prime_only: bool,
    /// In-stock only → rh `p_n_availability`.
    pub in_stock_only: bool,
    /// Minimum star rating (1..=4) → rh `p_72`.
    pub min_rating: Option<i32>,
    /// Brand name → rh `p_89:`.
    pub brand: String,
    /// Seller id → `me` parameter.
    pub seller_id: String,
    /// Raw extra `rh` entries (`key:value`, comma-joined),
    /// appended after the structured ones.
    pub rh: String,
    /// ASIN-targeted search: sent as the `field-asin` /
    /// `hidden-keywords` pair the app's `WebSearchUrlBuilder`
    /// emits for an ASIN search. When set, `hidden-keywords`
    /// carries the query (or the ASIN itself when the query is
    /// empty).
    pub asin: String,
}

/// Assemble the `rh` refinement string the way the app's
/// `getRefinementsString` does (`key:value` pairs, comma-joined).
/// The facet values follow Amazon's web vocabulary (the app
/// passes the web UI's refinement map through unchanged).
pub fn build_rh(params: &SearchParams) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !params.department.is_empty() {
        parts.push(format!("i:{}", params.department));
    }
    if !params.category_node.is_empty() {
        parts.push(format!("n:{}", params.category_node));
    }
    if params.price_min.is_some() || params.price_max.is_some() {
        // p_36 is denominated in 1/100 yen: the web UI's own
        // refinement values scale the displayed JPY band by 100.
        // Sending the raw JPY amounts made the server filter to a
        // band 100× lower than intended (Z12: a 3000-5000 search
        // returned a ¥43 item; with Prime added it returned none).
        parts.push(format!(
            "p_36:{}-{}",
            params.price_min.map(|v| v.saturating_mul(100)).unwrap_or(0),
            params
                .price_max
                .map(|v| v.saturating_mul(100))
                .unwrap_or(i32::MAX as i64)
        ));
    }
    if params.prime_only {
        // The Prime facet value is marketplace-specific: the
        // search page amazon.co.jp itself serves carries
        // p_85:8070706051. The previously used 2470954011 is the
        // .com value; on .co.jp it filters by an unrelated facet,
        // which is why Prime alone returned items but Prime
        // combined with a price band returned none (Z12).
        parts.push("p_85:8070706051".into());
    }
    if params.in_stock_only {
        parts.push("p_n_availability:2494232051".into());
    }
    if let Some(r) = params.min_rating {
        let v = match r {
            4 => "2661618011",
            3 => "2661619011",
            2 => "2661620011",
            _ => "2661621011",
        };
        parts.push(format!("p_72:{v}"));
    }
    if !params.brand.is_empty() {
        parts.push(format!("p_89:{}", params.brand));
    }
    if !params.rh.is_empty() {
        parts.push(params.rh.clone());
    }
    parts.join(",")
}

/// Build the search URL. Pure; unit-tested.
pub fn build_search_url(params: &SearchParams) -> String {
    let mut url = format!("{HOST}/s?k={}", url_encode(&params.query));
    if !params.sort.is_empty() {
        url.push_str(&format!("&s={}", url_encode(&params.sort)));
    }
    if !params.rank.is_empty() {
        url.push_str(&format!("&rank={}", url_encode(&params.rank)));
    }
    let rh = build_rh(params);
    if !rh.is_empty() {
        url.push_str(&format!("&rh={}", url_encode(&rh)));
    }
    if !params.seller_id.is_empty() {
        url.push_str(&format!("&me={}", url_encode(&params.seller_id)));
    }
    if !params.asin.is_empty() {
        url.push_str(&format!("&field-asin={}", url_encode(&params.asin)));
        let hidden = if params.query.is_empty() {
            params.asin.as_str()
        } else {
            params.query.as_str()
        };
        url.push_str(&format!("&hidden-keywords={}", url_encode(hidden)));
    }
    if params.page > 1 {
        url.push_str(&format!("&page={}", params.page));
    }
    url
}

pub fn build_detail_url(asin: &str) -> String {
    format!("{HOST}/dp/{asin}")
}

/// Extract an ASIN from a `/dp/<ASIN>` URL or pass a bare ASIN
/// through.
pub fn extract_asin(product: &str) -> String {
    static DP_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"/(?:dp|gp/product)/([A-Z0-9]{10})").unwrap());
    let s = product.trim();
    if let Some(c) = DP_RE.captures(s) {
        return c[1].to_string();
    }
    s.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(s)
        .to_string()
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// HTML extraction
// ---------------------------------------------------------------------------

static ASIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-asin="([A-Z0-9]{10})""#).unwrap());
static TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<h2[^>]*aria-label="([^"]+)""#).unwrap());
static PRICE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"a-price-whole">([0-9,]+)"#).unwrap());
static S_IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<img[^>]*class="s-image"[^>]*src="([^"]+)""#).unwrap());
static RATING_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"5つ星のうち([0-9.]+)").unwrap());
static DOC_TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<title>(.*?)</title>").unwrap());
static LANDING_IMG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""landingImageUrl":"(https://[^"]+)""#).unwrap());
static OG_IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"property="og:image" content="([^"]+)""#).unwrap());
static REVIEW_COUNT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"id="acrCustomerReviewText"[^>]*>([0-9,]+)"#).unwrap());
/// Search-result rating count: the star widget's companion
/// `aria-label="4,789 レーティング"` span.
static S_RATING_COUNT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"aria-label="([0-9,]+)\s*レーティング""#).unwrap());
/// Detail points: the offer data blob carries
/// `points":{"displayString":"50ポイント(1%)"` (HTML-escaped as
/// `&quot;` in the served page, or plain after unescaping).
static POINTS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:&quot;|")points(?:&quot;|"):\{(?:&quot;|")displayString(?:&quot;|"):(?:&quot;|")([0-9,]+)ポイント"#).unwrap()
});
static AVAILABILITY_RES: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        Regex::new(r#"primary-availability-message">\s*([^<]+)"#).unwrap(),
        Regex::new(r#"<span class="a-size-base a-color-success">([^<]+)</span>"#).unwrap(),
        Regex::new(r#"<span class="a-color-attainable">([^<]+)</span>"#).unwrap(),
    ]
});
static SELLER_SENTENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"この商品は、(.+?)が販売").unwrap());
static SELLER_PROFILE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"id="sellerProfileTriggerId"[^>]*>([^<]+)"#).unwrap());
static BULLET_ITEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<span class="a-list-item">(.*?)</span>"#).unwrap());
static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

/// True when the page is a robot check / apology page rather
/// than search results.
pub fn is_block_page(html: &str) -> bool {
    html.contains("validateCaptcha")
        || html.contains("Robot Check")
        || html.contains("ロボット") && html.contains("チェック")
}

/// Whether a redirect chain (all response header lines, in order)
/// passes through Amazon's sign-in page. The reviews route answers
/// guest sessions with `302 Location: …/ap/signin?…`; the client
/// follows the redirect, so the chain is the only place the
/// detour remains visible. Pure; unit-tested.
pub fn redirected_to_signin(headers: &[String]) -> bool {
    headers.iter().any(|h| {
        let lower = h.to_lowercase();
        lower.starts_with("location:") && lower.contains("/ap/signin")
    })
}

/// Parse a search results page. Pure; unit-tested against the
/// captured live page.
pub fn parse_search_html(html: &str, limit: usize) -> Vec<Item> {
    let mut items = Vec::new();
    for chunk in html
        .split("data-component-type=\"s-search-result\"")
        .skip(1)
    {
        if items.len() >= limit {
            break;
        }
        let Some(asin) = ASIN_RE.captures(chunk).map(|c| c[1].to_string()) else {
            continue;
        };
        let raw_title = TITLE_RE
            .captures(chunk)
            .map(|c| html_unescape(&c[1]))
            .unwrap_or_default();
        let sponsored = chunk.contains("AdHolder") || raw_title.starts_with("スポンサー広告");
        let title = raw_title
            .strip_prefix("スポンサー広告 - ")
            .unwrap_or(&raw_title)
            .to_string();
        let price = PRICE_RE
            .captures(chunk)
            .and_then(|c| c[1].replace(',', "").parse::<i64>().ok());
        items.push(Item {
            product_id: asin.clone(),
            title,
            url: build_detail_url(&asin),
            source: "amazon".into(),
            price: price.map(Price::yen),
            image: S_IMAGE_RE.captures(chunk).map(|c| c[1].to_string()),
            review_score: RATING_RE
                .captures(chunk)
                .and_then(|c| c[1].parse::<f64>().ok()),
            review_count: S_RATING_COUNT_RE
                .captures(chunk)
                .and_then(|c| c[1].replace(',', "").parse::<i64>().ok()),
            sponsored: Some(sponsored),
            ..Default::default()
        });
    }
    items
}

/// Parse a product detail page. Pure; unit-tested against
/// segments of the captured live page.
pub fn parse_detail_html(html: &str, asin: &str) -> Option<Item> {
    let doc_title = DOC_TITLE_RE
        .captures(html)?
        .get(1)?
        .as_str()
        .trim()
        .to_string();
    let mut title = doc_title
        .strip_prefix("Amazon.co.jp:")
        .map(|s| s.trim().to_string())
        .unwrap_or(doc_title);
    // The <title> form is "<product title> : <department>".
    if let Some(idx) = title.rfind(" : ") {
        title = title[..idx].trim().to_string();
    }
    if title.is_empty() || title.contains("Robot Check") {
        return None;
    }
    // Prefer the price inside the core price display block.
    let price = html
        .find("corePriceDisplay")
        .and_then(|i| PRICE_RE.captures(&html[i..]))
        .or_else(|| PRICE_RE.captures(html))
        .and_then(|c| c[1].replace(',', "").parse::<i64>().ok());
    let image = LANDING_IMG_RE
        .captures(html)
        .map(|c| c[1].to_string())
        .or_else(|| OG_IMAGE_RE.captures(html).map(|c| c[1].to_string()));
    Some(Item {
        product_id: asin.to_string(),
        title,
        url: build_detail_url(asin),
        source: "amazon".into(),
        price: price.map(Price::yen),
        image,
        review_score: RATING_RE
            .captures(html)
            .and_then(|c| c[1].parse::<f64>().ok()),
        review_count: REVIEW_COUNT_RE
            .captures(html)
            .and_then(|c| c[1].replace(',', "").parse::<i64>().ok()),
        points: POINTS_RE
            .captures(html)
            .and_then(|c| c[1].replace(',', "").parse::<i64>().ok()),
        description: extract_description(html),
        availability: extract_availability(html),
        seller: SELLER_SENTENCE_RE
            .captures(html)
            .map(|c| c[1].trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| {
                SELLER_PROFILE_RE
                    .captures(html)
                    .map(|c| html_unescape(c[1].trim()))
                    .filter(|s| !s.is_empty())
            }),
        ..Default::default()
    })
}

/// The JP marketplace id the app's Realm reports (the
/// completion service accepts it as `mid`).
const MARKETPLACE_ID: &str = "A1VC38T7YXB528";
const COMPLETION_HOST: &str = "https://completion.amazon.co.jp";

/// Build the Completion Service V2 suggest URL
/// (`SearchSuggestionsV2ServiceCall`: GET
/// `/api/2017/suggestions`). Parameter names and defaults are
/// the app's: alias `aps`, site-variant `android-mshop`,
/// version 2, event `onkeypress`, lop `ja-JP`, suggestion-type
/// KEYWORD. The app's `client-id` / `session-id` are optional
/// in the service call and are omitted here (live-verified
/// without them). Pure; unit-tested.
pub fn build_suggest_url(keyword: &str, limit: i32) -> String {
    let limit = if limit > 0 { limit } else { 10 };
    format!(
        "{COMPLETION_HOST}/api/2017/suggestions?prefix={}&alias=aps&mid={MARKETPLACE_ID}&site-variant=android-mshop&version=2&limit={limit}&suggestion-type=KEYWORD&event=onkeypress&lop=ja-JP",
        url_encode(keyword)
    )
}

/// Parse a Completion Service V2 response. Pure; unit-tested
/// against the captured live response.
pub fn parse_suggest_response(text: &str, keyword: &str) -> serde_json::Value {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else {
        return serde_json::json!({
            "provider": "amazon",
            "ok": false,
            "query": keyword,
            "error": "amazon suggest: response was not JSON",
        });
    };
    let suggestions: Vec<serde_json::Value> = parsed["suggestions"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    serde_json::json!({
                        "value": s["value"].as_str().unwrap_or_default(),
                        "type": s["type"].as_str(),
                        "ref_tag": s["refTag"].as_str(),
                        "strategy_id": s["strategyId"].as_str(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "provider": "amazon",
        "ok": true,
        "query": keyword,
        "suggestions": suggestions,
        "count": suggestions.len(),
    })
}

pub fn build_reviews_url(asin: &str, page: i32) -> String {
    if page > 1 {
        format!("{HOST}/product-reviews/{asin}?pageNumber={page}")
    } else {
        format!("{HOST}/product-reviews/{asin}")
    }
}

static REVIEW_BLOCK_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"id="customer_review-([A-Z0-9]+)""#).unwrap());
static REVIEW_TITLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)data-hook="review-title".*?<span>([^<]+)</span>"#).unwrap());
static REVIEW_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-hook="review-date"[^>]*>([^<]+)"#).unwrap());
static REVIEW_BODY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)data-hook="review-body"[^>]*>\s*<span>(.*?)</span>"#).unwrap()
});
static REVIEW_HELPFUL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-hook="helpful-vote-statement"[^>]*>([^<]+)"#).unwrap());

/// Parse a `/product-reviews/<ASIN>` page into review objects.
/// Pure; unit-tested against the captured live page.
pub fn parse_reviews_html(html: &str) -> Vec<serde_json::Value> {
    let mut reviews = Vec::new();
    for chunk in html.split("data-hook=\"review\"").skip(1) {
        let Some(id) = REVIEW_BLOCK_ID_RE.captures(chunk).map(|c| c[1].to_string()) else {
            continue;
        };
        let body = REVIEW_BODY_RE.captures(chunk).map(|c| {
            html_unescape(TAG_RE.replace_all(&c[1], " ").trim())
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        });
        reviews.push(serde_json::json!({
            "review_id": id,
            "title": REVIEW_TITLE_RE.captures(chunk).map(|c| html_unescape(c[1].trim())),
            "rating": RATING_RE.captures(chunk).and_then(|c| c[1].parse::<f64>().ok()),
            "date": REVIEW_DATE_RE.captures(chunk).map(|c| html_unescape(c[1].trim())),
            "body": body,
            "verified_purchase": chunk.contains("data-hook=\"avp-badge\""),
            "helpful": REVIEW_HELPFUL_RE.captures(chunk).map(|c| html_unescape(c[1].trim())),
        }));
    }
    reviews
}

/// Availability text from a detail page. The stock message is
/// served in one of a few span shapes depending on layout; the
/// text is returned as served (trimmed). Pure.
fn extract_availability(html: &str) -> Option<String> {
    for re in AVAILABILITY_RES.iter() {
        if let Some(c) = re.captures(html) {
            let text = html_unescape(c[1].trim());
            if text.contains("在庫") || text.contains("残り") || text.contains("入荷") {
                return Some(text);
            }
        }
    }
    None
}

/// Product description from a detail page: the feature-bullets
/// list items (tags stripped, one bullet per line), falling
/// back to the `productDescription` block's text. Pure.
fn extract_description(html: &str) -> Option<String> {
    if let Some(start) = html.find("id=\"feature-bullets\"") {
        let region = &html[start..(start + 12000).min(html.len())];
        let bullets: Vec<String> = BULLET_ITEM_RE
            .captures_iter(region)
            .map(|c| html_unescape(TAG_RE.replace_all(&c[1], " ").trim()))
            .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
            .filter(|t| !t.is_empty())
            .collect();
        if !bullets.is_empty() {
            return Some(bullets.join("\n"));
        }
    }
    let start = html.find("id=\"productDescription\"")?;
    let region = &html[start..(start + 8000).min(html.len())];
    let text = html_unescape(TAG_RE.replace_all(region, " ").trim());
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// Client (manual cookie jar, mirroring the app's WebView cookies)
// ---------------------------------------------------------------------------

pub struct AmazonClient {
    cookies: Mutex<HashMap<String, String>>,
}

impl AmazonClient {
    pub fn new() -> Self {
        let mut jar = HashMap::new();
        // The cookies the app syncs into the WebView before the
        // first navigation. Server-issued cookies (session-id
        // etc.) are captured from responses and added to the jar.
        jar.insert(
            "mobile-device-info".to_string(),
            COOKIE_DEVICE_INFO.to_string(),
        );
        jar.insert("amzn-app-id".to_string(), COOKIE_APP_ID.to_string());
        Self {
            cookies: Mutex::new(jar),
        }
    }

    fn cookie_header(&self) -> String {
        let jar = self.cookies.lock().unwrap();
        let mut pairs: Vec<String> = jar.iter().map(|(k, v)| format!("{k}={v}")).collect();
        pairs.sort();
        pairs.join("; ")
    }

    /// Store the cookies from a response's `Set-Cookie` header
    /// values (the part after the header name, one per entry).
    fn store_response_cookies(&self, set_cookie_values: &[String]) {
        let mut jar = self.cookies.lock().unwrap();
        for s in set_cookie_values {
            let first = s.split(';').next().unwrap_or("");
            if let Some((k, v)) = first.split_once('=') {
                jar.insert(k.trim().to_string(), v.trim().to_string());
            }
        }
    }

    async fn get_html(&self, url: &str) -> Result<String> {
        Ok(self.get_html_full(url).await?.0)
    }

    /// [`Self::get_html`] plus the response header lines of the
    /// whole redirect chain (the fetch follows redirects, so the
    /// chain is how a sign-in redirect is visible at all).
    async fn get_html_full(&self, url: &str) -> Result<(String, Vec<String>)> {
        let cookie = self.cookie_header();
        let url = url.to_string();
        let dump_url = url.clone();
        let (status, set_cookies, headers, html) =
            tokio::task::spawn_blocking(move || fetch_blocking(&url, &cookie))
                .await
                .map_err(|e| anyhow!("amazon fetch task failed: {e}"))??;
        dump_response(&dump_url, status, &headers, &html);
        self.store_response_cookies(&set_cookies);
        if status == 503 {
            return Err(anyhow!(
                "amazon returned HTTP 503 (robot check; Japan-side access is required in practice)"
            ));
        }
        if !(200..300).contains(&status) {
            return Err(anyhow!("amazon returned HTTP {status}"));
        }
        Ok((html, headers))
    }

    pub async fn search_products(&self, params: SearchParams) -> Result<SearchResult> {
        // `p_72` only has values for 1..=4 stars-and-up; reject
        // out-of-range input instead of silently degrading it to
        // the 1-star facet (the previous behaviour).
        if let Some(r) = params.min_rating {
            if !(1..=4).contains(&r) {
                return Err(anyhow!(
                    "amazon min_rating must be between 1 and 4 (got {r})"
                ));
            }
        }
        let url = build_search_url(&params);
        let html = self.get_html(&url).await?;
        if is_block_page(&html) {
            return Err(anyhow!(
                "amazon answered with a robot check page (Japan-side access is required in practice)"
            ));
        }
        let limit = if params.limit > 0 {
            params.limit as usize
        } else {
            usize::MAX
        };
        let items = parse_search_html(&html, limit);
        Ok(SearchResult {
            provider: "amazon".into(),
            ok: true,
            query: params.query.clone(),
            page: params.page,
            limit: params.limit,
            sort: params.sort.clone(),
            count: items.len(),
            items,
        })
    }

    pub async fn get_product(&self, product: &str) -> Result<ItemResult> {
        let asin = extract_asin(product);
        let url = build_detail_url(&asin);
        let html = self.get_html(&url).await?;
        if is_block_page(&html) {
            return Ok(ItemResult {
                provider: "amazon".into(),
                ok: false,
                item: None,
                error: Some(
                    "amazon answered with a robot check page (Japan-side access is required in practice)"
                        .into(),
                ),
            });
        }
        match parse_detail_html(&html, &asin) {
            Some(item) => Ok(ItemResult {
                provider: "amazon".into(),
                ok: true,
                item: Some(item),
                error: None,
            }),
            None => Ok(ItemResult {
                provider: "amazon".into(),
                ok: false,
                item: None,
                error: Some("amazon detail: no product data in the returned page".into()),
            }),
        }
    }

    /// Keyword suggestions from the app's native Completion
    /// Service V2 (the one native API the current app still
    /// uses; search and detail are WebView loads). Fetched with
    /// the same app conditions as the page routes.
    pub async fn suggest_keywords(&self, keyword: &str, limit: i32) -> Result<serde_json::Value> {
        let url = build_suggest_url(keyword, limit);
        let text = self.get_html(&url).await?;
        Ok(parse_suggest_response(&text, keyword))
    }

    /// Review list from the `/product-reviews/<ASIN>` page (the
    /// app has no review API; the page route is the candidate
    /// the round-1 analysis identified). Returns the standard
    /// result JSON with a `reviews` array.
    ///
    /// Guest state, measured 2026-11-10: the page answers with a
    /// 302 redirect to `/ap/signin`, so reviews are not served to
    /// guest sessions at all. That case is reported as its own
    /// error (not as a parse failure) via the redirect chain.
    pub async fn get_reviews(&self, product: &str, page: i32) -> Result<serde_json::Value> {
        let asin = extract_asin(product);
        let url = build_reviews_url(&asin, page);
        let (html, headers) = self.get_html_full(&url).await?;
        if redirected_to_signin(&headers) {
            return Ok(serde_json::json!({
                "provider": "amazon",
                "ok": false,
                "asin": asin,
                "page": page,
                "error": "amazon reviews: the reviews page redirected to sign-in; reviews are not available to guest sessions",
            }));
        }
        if is_block_page(&html) {
            return Ok(serde_json::json!({
                "provider": "amazon",
                "ok": false,
                "asin": asin,
                "page": page,
                "error": "amazon answered with a robot check page (Japan-side access is required in practice)",
            }));
        }
        let reviews = parse_reviews_html(&html);
        if reviews.is_empty() {
            return Ok(serde_json::json!({
                "provider": "amazon",
                "ok": false,
                "asin": asin,
                "page": page,
                "error": "amazon reviews: no review data in the returned page",
            }));
        }
        Ok(serde_json::json!({
            "provider": "amazon",
            "ok": true,
            "asin": asin,
            "page": page,
            "reviews": reviews,
            "count": reviews.len(),
        }))
    }
}

/// Diagnostics aid: when the `ZERO_SEARCH_AMAZON_DUMP_DIR`
/// environment variable names a directory, each fetched response
/// is written there (status line + headers with cookie values
/// redacted, and the body) for offline inspection. Off by default;
/// nothing is written unless the variable is set.
fn dump_response(url: &str, status: u32, headers: &[String], html: &str) {
    let Ok(dir) = std::env::var("ZERO_SEARCH_AMAZON_DUMP_DIR") else {
        return;
    };
    if dir.is_empty() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::path::Path::new(&dir);
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let mut head = format!("url: {url}\nstatus: {status}\n");
    for h in headers {
        if let Some((name, _)) = h.split_once(':') {
            if name.trim().eq_ignore_ascii_case("set-cookie") {
                head.push_str("set-cookie: <redacted>\n");
                continue;
            }
        }
        head.push_str(h);
        head.push('\n');
    }
    let _ = std::fs::write(dir.join(format!("{stamp}.headers.txt")), head);
    let _ = std::fs::write(dir.join(format!("{stamp}.html")), html);
}

/// One blocking libcurl fetch. Returns the final HTTP status,
/// the response's `Set-Cookie` values, the response header lines,
/// and the body as text.
///
/// The request is deliberately the request the curl tool makes:
/// libcurl's own TLS stack with each platform's standard TLS
/// backend. The transfer itself runs over HTTP/1.1 (the shape
/// the integrated client is live-verified with; the bundled
/// static libcurl's HTTP/2 support is not relied on). No proxy
/// configuration of ours — libcurl applies its standard
/// environment-based proxy behaviour exactly like the curl tool
/// does.
fn fetch_blocking(
    url: &str,
    cookie_header: &str,
) -> Result<(u32, Vec<String>, Vec<String>, String)> {
    let mut easy = Easy::new();
    easy.url(url)?;
    easy.follow_location(true)?;
    // Advertise every built-in content encoding and let libcurl
    // decode the response: the service answers with
    // `Content-Encoding: gzip`, and without this the body comes
    // back as raw compressed bytes that no HTML extraction can
    // read (the cause of "ok with 0 items" in live checks).
    easy.accept_encoding("")?;
    easy.http_version(HttpVersion::V11)?;
    // The curl crate defaults to CURLOPT_NOSIGNAL, so timeouts
    // are not delivered as signals inside the multi-threaded
    // server process.
    easy.timeout(std::time::Duration::from_secs(60))?;
    easy.useragent(USER_AGENT)?;
    let mut headers = List::new();
    headers.append("Accept-Language: ja-JP")?;
    headers.append("Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")?;
    headers.append(&format!("Cookie: {cookie_header}"))?;
    easy.http_headers(headers)?;

    let mut body: Vec<u8> = Vec::new();
    let mut set_cookies: Vec<String> = Vec::new();
    let mut header_lines: Vec<String> = Vec::new();
    {
        let mut transfer = easy.transfer();
        transfer.write_function(|data| {
            body.extend_from_slice(data);
            Ok(data.len())
        })?;
        transfer.header_function(|line| {
            let line = String::from_utf8_lossy(line);
            let line = line.trim_end();
            if !line.is_empty() {
                header_lines.push(line.to_string());
            }
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("set-cookie") {
                    set_cookies.push(value.trim().to_string());
                }
            }
            true
        })?;
        transfer.perform()?;
    }
    let status = easy.response_code()?;
    let html = String::from_utf8_lossy(&body).into_owned();
    Ok((status, set_cookies, header_lines, html))
}

impl Default for AmazonClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_minimal() {
        let url = build_search_url(&SearchParams {
            query: "ワイヤレスイヤホン".into(),
            ..Default::default()
        });
        assert!(url.starts_with("https://www.amazon.co.jp/s?k="));
        assert!(!url.contains("&s="));
        assert!(!url.contains("&page="));
    }

    #[test]
    fn search_url_with_sort_filters_and_page() {
        let url = build_search_url(&SearchParams {
            query: "イヤホン".into(),
            page: 2,
            sort: "price-asc-rank".into(),
            price_min: Some(1000),
            price_max: Some(5000),
            prime_only: true,
            min_rating: Some(4),
            brand: "Anker".into(),
            seller_id: "A1EXAMPLE".into(),
            ..Default::default()
        });
        assert!(url.contains("&s=price-asc-rank"));
        assert!(url.contains("&page=2"));
        assert!(url.contains("&me=A1EXAMPLE"));
        let rh = build_rh(&SearchParams {
            price_min: Some(1000),
            price_max: Some(5000),
            prime_only: true,
            min_rating: Some(4),
            brand: "Anker".into(),
            ..Default::default()
        });
        assert_eq!(
            rh,
            "p_36:100000-500000,p_85:8070706051,p_72:2661618011,p_89:Anker"
        );
    }

    #[test]
    fn parse_search_fixture() {
        let html = include_str!("../tests/fixtures/search.html");
        let items = parse_search_html(html, 50);
        assert!(items.len() >= 5, "got {} items", items.len());
        let first = &items[0];
        assert_eq!(first.product_id.len(), 10);
        assert!(!first.title.is_empty());
        assert!(!first.title.starts_with("スポンサー広告"));
        assert_eq!(first.sponsored, Some(true));
        assert!(first.url.ends_with(&first.product_id));
        assert!(first.price.is_some());
        assert!(first.image.as_ref().unwrap().contains("media-amazon.com"));
    }

    #[test]
    fn parse_search_fixture_live2() {
        // Excerpt of the page the integrated client received on
        // its first successful live search, after the
        // content-decoding fix: the service sends the body
        // gzip-encoded, and extraction runs on the decoded text.
        let html = include_str!("../tests/fixtures/search-live2.html");
        let items = parse_search_html(html, 20);
        assert!(items.len() >= 3, "got {} items", items.len());
        let first = &items[0];
        assert_eq!(first.product_id, "B0FNRL2QMT");
        assert!(first.title.contains("Anker Soundcore P31i"));
        assert_eq!(first.price.as_ref().unwrap().value, 4990);
        assert!(first.image.as_ref().unwrap().contains("media-amazon.com"));
    }

    #[test]
    fn parse_search_fixture_review_count() {
        let html = include_str!("../tests/fixtures/search.html");
        let items = parse_search_html(html, 50);
        // The rating widget's companion span carries the count
        // ("4,789 レーティング" for the first result).
        assert_eq!(items[0].review_count, Some(4789));
        assert_eq!(items[0].review_score, Some(4.4));
    }

    #[test]
    fn parse_detail_fields_fixture() {
        // Segments of the live detail page captured in round 2
        // of the provider analysis (same product as
        // detail.html), covering the fields the original parser
        // did not extract.
        let html = include_str!("../tests/fixtures/detail-fields.html");
        let item = parse_detail_html(html, "B0FNRS5WB2").expect("detail must parse");
        assert!(item.title.contains("Anker Soundcore P31i"));
        assert_eq!(item.price.as_ref().unwrap().value, 4990);
        assert_eq!(item.points, Some(50));
        assert_eq!(item.seller.as_deref(), Some("AnkerDirect"));
        assert!(item.availability.as_deref().unwrap().contains("在庫あり"));
        let desc = item.description.as_ref().expect("description must parse");
        assert!(desc.contains("ノイズキャンセリング"));
        assert!(desc.contains('\n'), "bullets are joined with newlines");
    }

    #[test]
    fn search_url_asin_targeted() {
        let url = build_search_url(&SearchParams {
            query: String::new(),
            asin: "B0FNRS5WB2".into(),
            ..Default::default()
        });
        assert!(url.contains("&field-asin=B0FNRS5WB2"));
        assert!(url.contains("&hidden-keywords=B0FNRS5WB2"));
        let url = build_search_url(&SearchParams {
            query: "イヤホン".into(),
            asin: "B0FNRS5WB2".into(),
            ..Default::default()
        });
        assert!(url.contains("hidden-keywords=%E3%82%A4%E3%83%A4%E3%83%9B%E3%83%B3"));
    }

    #[tokio::test]
    async fn min_rating_out_of_range_is_rejected_before_any_fetch() {
        let client = AmazonClient::new();
        for bad in [0, 5, -1] {
            let err = client
                .search_products(SearchParams {
                    query: "x".into(),
                    min_rating: Some(bad),
                    ..Default::default()
                })
                .await
                .expect_err("out-of-range min_rating must error");
            assert!(err.to_string().contains("between 1 and 4"), "{err}");
        }
    }

    #[test]
    fn suggest_url_and_parse() {
        let url = build_suggest_url("イヤホン", 10);
        assert!(url.starts_with("https://completion.amazon.co.jp/api/2017/suggestions?prefix="));
        assert!(url.contains("alias=aps"));
        assert!(url.contains("mid=A1VC38T7YXB528"));
        assert!(url.contains("site-variant=android-mshop"));
        assert!(url.contains("limit=10"));
        let text = include_str!("../tests/fixtures/suggest.json");
        let out = parse_suggest_response(text, "イヤホン");
        assert_eq!(out["ok"], serde_json::json!(true));
        assert_eq!(out["count"], serde_json::json!(10));
        assert_eq!(
            out["suggestions"][0]["value"],
            serde_json::json!("イヤホン")
        );
        assert_eq!(
            out["suggestions"][1]["value"],
            serde_json::json!("イヤホン bluetooth")
        );
    }

    #[test]
    fn reviews_url_shape() {
        assert_eq!(
            build_reviews_url("B0FNRS5WB2", 1),
            "https://www.amazon.co.jp/product-reviews/B0FNRS5WB2"
        );
        assert_eq!(
            build_reviews_url("B0FNRS5WB2", 2),
            "https://www.amazon.co.jp/product-reviews/B0FNRS5WB2?pageNumber=2"
        );
    }

    #[test]
    fn parse_detail_fixture() {
        let html = include_str!("../tests/fixtures/detail.html");
        let item = parse_detail_html(html, "B0FNRS5WB2").expect("detail must parse");
        assert!(item.title.contains("Anker Soundcore P31i"));
        assert_eq!(item.price.as_ref().unwrap().value, 4990);
        assert!(item.image.as_ref().unwrap().contains("media-amazon.com"));
        assert_eq!(item.review_score, Some(4.4));
    }

    #[test]
    fn asin_extraction() {
        assert_eq!(
            extract_asin("https://www.amazon.co.jp/dp/B0FNRS5WB2"),
            "B0FNRS5WB2"
        );
        assert_eq!(
            extract_asin("https://www.amazon.co.jp/gp/product/B0FNRS5WB2/ref=x"),
            "B0FNRS5WB2"
        );
        assert_eq!(extract_asin("B0FNRS5WB2"), "B0FNRS5WB2");
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored (requires Japan-side access)"]
    async fn test_live_search() {
        let client = AmazonClient::new();
        let result = client
            .search_products(SearchParams {
                query: "ワイヤレスイヤホン".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        assert!(!result.items.is_empty());
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored (requires Japan-side access)"]
    async fn test_live_search_variants() {
        // S2-28: sort / filters / paging / min_rating against
        // the live service. Sends are serial with pauses.
        let client = AmazonClient::new();
        let sorted = client
            .search_products(SearchParams {
                query: "ワイヤレスイヤホン".into(),
                sort: "price-asc-rank".into(),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(sorted.ok);
        let prices: Vec<i64> = sorted
            .items
            .iter()
            .filter_map(|i| i.price.as_ref().map(|p| p.value))
            .collect();
        assert!(prices.len() >= 5, "too few priced items: {}", prices.len());
        assert!(
            prices.windows(2).all(|w| w[0] <= w[1]),
            "price-asc-rank not ascending: {prices:?}"
        );
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let filtered = client
            .search_products(SearchParams {
                query: "ワイヤレスイヤホン".into(),
                price_min: Some(3000),
                price_max: Some(5000),
                prime_only: true,
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(filtered.ok);
        assert!(!filtered.items.is_empty());
        for item in &filtered.items {
            if let Some(p) = item.price.as_ref().map(|p| p.value) {
                assert!((3000..=5000).contains(&p), "price {p} outside band");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let page1 = client
            .search_products(SearchParams {
                query: "ワイヤレスイヤホン".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let page2 = client
            .search_products(SearchParams {
                query: "ワイヤレスイヤホン".into(),
                page: 2,
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(page1.ok && page2.ok);
        assert!(!page2.items.is_empty());
        assert_ne!(page1.items[0].product_id, page2.items[0].product_id);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let rated = client
            .search_products(SearchParams {
                query: "ワイヤレスイヤホン".into(),
                min_rating: Some(4),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(rated.ok);
        assert!(!rated.items.is_empty());
        for item in &rated.items {
            if let Some(r) = item.review_score {
                assert!(r >= 4.0, "rating {r} below min_rating 4");
            }
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored (requires Japan-side access)"]
    async fn test_live_asin_search() {
        // S2-30: exact-ASIN search via field-asin/hidden-keywords.
        let client = AmazonClient::new();
        let result = client
            .search_products(SearchParams {
                asin: "B0FNRS5WB2".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        assert!(
            result.items.iter().any(|i| i.product_id == "B0FNRS5WB2"),
            "ASIN not in results: {:?}",
            result
                .items
                .iter()
                .map(|i| &i.product_id)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored (requires Japan-side access)"]
    async fn test_live_suggest_and_reviews() {
        // S2-26 (client-level) and S2-27.
        let client = AmazonClient::new();
        let s = client.suggest_keywords("イヤホン", 10).await.unwrap();
        assert_eq!(s["ok"], serde_json::json!(true), "{s}");
        assert!(s["count"].as_u64().unwrap() > 0);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let r = client.get_reviews("B0FNRS5WB2", 1).await.unwrap();
        if r["ok"] == serde_json::json!(true) {
            assert!(r["count"].as_u64().unwrap() > 0, "{r}");
            assert!(r["reviews"][0]["body"]
                .as_str()
                .is_some_and(|b| !b.is_empty()));
        } else {
            // Guest sessions are currently redirected to sign-in
            // (measured 2026-11-10: GET /product-reviews/<ASIN>
            // answers 302 with Location: /ap/signin?…). Reviews
            // cannot be served to guests; what this test pins is
            // that the provider reports exactly that, instead of
            // the old misleading "no review data" parse failure.
            let err = r["error"].as_str().unwrap_or("");
            assert!(
                err.contains("redirected to sign-in"),
                "unexpected reviews error: {r}"
            );
        }
    }

    #[test]
    fn signin_redirect_is_detected_in_the_chain() {
        let chain = vec![
            "HTTP/1.1 302 Found".to_string(),
            "location: https://www.amazon.co.jp/ap/signin?openid.return_to=x".to_string(),
            "HTTP/1.1 200 OK".to_string(),
        ];
        assert!(redirected_to_signin(&chain));
        let plain = vec!["HTTP/1.1 200 OK".to_string()];
        assert!(!redirected_to_signin(&plain));
    }
}
