use anyhow::{Context, Result};
use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;

/// Web base, used only for canonical item URLs and the seller rating page
/// (reviews have no app-API equivalent and stay on the Web route).
const WEB_BASE: &str = "https://auctions.yahoo.co.jp";
/// Official app API base (APK 8.48.1, interface `hm1`).
const API_BASE: &str = "https://auctions.yahooapis.jp/app/";
/// Root base for the auxiliary API (interface `yne`), whose paths carry
/// the `app/` prefix themselves.
const API_ROOT_BASE: &str = "https://auctions.yahooapis.jp/";
/// Public app identifier the app's `nd1` interceptor attaches to every API
/// request. Without it the gateway returns 401. It is not a user credential:
/// no Authorization header is sent for anonymous access.
const APPID: &str = "dj00aiZpPXVkeDRaNE5IY1B1OCZzPWNvbnN1bWVyc2VjcmV0Jng9ZmM-";
const APP_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8 Build/PSR1.180720.093; wv) AppleWebKit/537.36 (KHTML, like Gecko) Version/4.0 Chrome/126.0.0.0 Mobile Safari/537.36 YJApp-ANDROID jp.co.yahoo.android.yauction/8.48.1";
const WEB_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

static REVIEW_BLOCK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r#"(?s)<!--\s*RATING INFO\s*-->"#,
        r#".*?<IMG[^>]*alt="([^"]*)"[^>]*/?>.*?"#,
        r#"(出品者です|落札者です)"#,
        r#".*?評価者：[^<]*?</b>\s*([^（(\s<]+)\s*[（(](\d+)[)）]"#,
        r#".*?<!--\s*ITEM INFO\s*-->"#,
        r#".*?/auction/([^"]*)"[^>]*>([^<]*)</A>"#,
        r#".*?<!--\s*COMMENT\s*-->"#,
        r#".*?<td[^>]*>：</td>\s*<td[^>]*><small>(.*?)(?:\(評価日時：([^)]*)\))?\s*</small>"#,
    ))
    .unwrap()
});

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Seller {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub product_id: String,
    pub auction_id: String,
    pub title: String,
    pub url: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buy_now_price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bid_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub watchers: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_path: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_shipping: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seller: Option<Seller>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shipping_method: Option<String>,
    // ── [S1-29] fields the app API returns but the provider used to drop ──
    /// Starting price (`initPrice`, detail only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_price: Option<Price>,
    /// Tax-excluded current price (`priceNoTax`, detail only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_no_tax: Option<Price>,
    /// Tax-excluded buy-now price (`buyNowPriceNoTax`, search + detail).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buy_now_price_no_tax: Option<Price>,
    /// Number of questions (`qaCount`, detail only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub qa_count: Option<i64>,
    /// Accepted payment methods (`payment`, detail only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment: Option<PaymentInfo>,
    /// Bidder-relative bid state (`bidInfo`, detail only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bid_info: Option<BidInfo>,
    /// Listed quantity (`quantity`, detail; also present in search payloads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quantity: Option<i64>,
    /// Last update time (`updateTime`, detail only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_time: Option<String>,
    /// Fixed-price (定額) listing (`isFixedPrice`, search + detail).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_fixed_price: Option<bool>,
    /// Cross-listed from Yahoo!フリマ (`isFleamarketItem`, search).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_fleamarket_item: Option<bool>,
    /// Already sold (`isSold`, search).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_sold: Option<bool>,
    /// Yahoo! Shopping category linkage (`shpCategory`, search).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shp_category: Option<ShpCategory>,
}

/// Payment methods accepted for an item (`ItemDetail.Response.Payment`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_cash_registration: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_credit_card: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_installment: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_paypay_balance: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allow_paypay_deferred: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_easy_payment_available: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub banks: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub others: Option<Vec<String>>,
}

/// Bidder-relative bid state (`ItemDetail.Response.BidInfo`). The price
/// fields flatten `lastBidInfo.price` / `nextBidInfo.price`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BidInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_bidder: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_deleted_winner: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_highest_bidder: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_next_winner: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_winner: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_winner_limit_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_bid_price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_bid_price: Option<Price>,
}

/// Yahoo! Shopping category linkage (`Search.Response.ShpCategory`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShpCategory {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genre_id: Option<i64>,
}

/// Aggregate seller rating, shaped like the `ratings_summary` object the
/// other providers' `get_reviews` return (good / bad / total). The profile
/// API's `Rating` carries only `total` + `goodRatio`, so `good` / `bad`
/// stay `None` there; the detail payload's seller rating has all three.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatingsSummary {
    pub total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub good: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bad: Option<i64>,
    /// `goodRatio` as returned by the API (0.0–1.0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub good_ratio: Option<f64>,
    /// `goodRatio` formatted the way the app shows it (e.g. `"99.2%"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_url: Option<String>,
}

/// Seller profile (`GET user/v2/users/{aucUserId}/profile`,
/// `UserOther.Response`). Anonymous HTTP 200 verified 2026-11-01.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SellerProfile {
    pub provider: String,
    pub ok: bool,
    pub seller_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_store: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_display_store_profile: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_ekyc_verified: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_speed_shipping: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub myself: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub following: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratings_summary: Option<RatingsSummary>,
}

/// One store-profile banner (`StoreProfile.Response.Banner`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreBanner {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_url: Option<String>,
}

/// Store profile (`GET user/v1/users/{aucUserId}/store-profile`,
/// `StoreProfile.Response`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreProfileResult {
    pub provider: String,
    pub ok: bool,
    pub seller_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub announcement: Option<String>,
    pub banners: Vec<StoreBanner>,
}

/// One day of closed-price statistics
/// (`ItemStatistics.Response.Statistics.Day`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatisticsDay {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    /// `YYYY-MM-DD` rendered from year/month/day.
    pub date: String,
    pub count: i64,
    pub avg_price: Price,
    pub max_price: Price,
    pub min_price: Price,
}

/// Most recent closed item in a statistics response
/// (`ItemStatistics.Response.LastClosedItem`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LastClosedItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_condition: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
}

/// Closed-auction price statistics
/// (`GET search/v1/search/items/statistics`, `ItemStatistics.Response`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemStatisticsResult {
    pub provider: String,
    pub ok: bool,
    pub catalog_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_results_available: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_closed_item: Option<LastClosedItem>,
    pub days: Vec<StatisticsDay>,
}

/// Parameters for the statistics endpoints ([S1-27]).
///
/// `conditions` holds raw ItemCondition ids (`NEW`, `USED10`, …). How the
/// API expects several ids joined inside the single `conditions` query
/// value is NOT confirmed (round-1 report §8 open point 4); the builder
/// comma-joins them, matching every other list parameter of the search
/// API, and the choice is flagged for live verification.
#[derive(Debug, Clone, Default)]
pub struct StatisticsParams {
    pub catalog_id: String,
    pub conditions: Vec<String>,
    /// yne variant only: `shpSpec` value, passed through verbatim.
    pub shp_spec: String,
    /// yne variant only: `sellerType` value, passed through verbatim.
    pub seller_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub rating: String,
    pub role: String,
    pub message: String,
    pub reviewer_name: String,
    pub reviewer_score: i64,
    pub item_title: String,
    pub auction_id: String,
    pub date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewsResult {
    pub provider: String,
    pub ok: bool,
    pub seller_id: String,
    pub reviews: Vec<Review>,
    pub count: usize,
    pub page: i32,
    /// [S1-28] Rating aggregate from the seller-profile API, matching the
    /// `ratings_summary` the other providers' review results carry.
    /// `None` when the profile lookup failed (reviews still return).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ratings_summary: Option<RatingsSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub provider: String,
    pub ok: bool,
    pub query: String,
    pub page: i32,
    pub limit: i32,
    pub sort: String,
    pub status: String,
    pub items: Vec<Item>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_available: Option<i64>,
    pub search_mode: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Suggestion {
    pub keyword: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_path: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestResult {
    pub provider: String,
    pub ok: bool,
    pub query: String,
    pub suggestions: Vec<Suggestion>,
    pub count: usize,
}

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    pub limit: i32,
    pub page: i32,
    pub sort: String,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    pub category_id: String,
    pub status: String,
    pub keywords_and: String,
    pub keywords_not: String,
    pub seller_id: String,
    pub buynow_only: bool,
    pub free_shipping: bool,
    pub condition: String,
    // ── [S1-25] remaining keys of Search.Request.toQueryMap() (round-1
    // report §4.4). List values are comma-joined exactly as the app joins
    // them; callers pass already-joined strings (same style as
    // `category_id` / `seller_id` above). Empty / `None` = not sent.
    /// `brandIds` (comma-joined numeric ids).
    pub brand_ids: String,
    /// `catalogIds` (comma-joined).
    pub catalog_ids: String,
    /// `minBuyNowPrice`.
    pub min_buy_now_price: Option<i64>,
    /// `maxBuyNowPrice`.
    pub max_buy_now_price: Option<i64>,
    /// `sellerType` (comma-joined SellerType ids:
    /// `AUC_STORE` / `AUCTION` / `FLEAMARKET`).
    pub seller_type: String,
    /// `prefectureCode` (comma-joined Prefecture values, e.g. `TOKYO,OSAKA`).
    pub prefecture_code: String,
    /// `destinationPrefectureCode` (single Prefecture value).
    pub destination_prefecture_code: String,
    /// `isFeatured`.
    pub is_featured: Option<bool>,
    /// `isAppraisal`.
    pub is_appraisal: Option<bool>,
    /// `isFinishSoon`.
    pub is_finish_soon: Option<bool>,
    /// `isConvenienceStore`.
    pub is_convenience_store: Option<bool>,
    /// `isNewArrival`.
    pub is_new_arrival: Option<bool>,
    /// `exceptSuspectedFake`.
    pub except_suspected_fake: Option<bool>,
    /// `canPrivacyDelivery`.
    pub can_privacy_delivery: Option<bool>,
    /// `isAdultOk`.
    pub is_adult_ok: Option<bool>,
    /// `auctionIds` (comma-joined).
    pub auction_ids: String,
    /// `exceptAuctionIds` (comma-joined).
    pub except_auction_ids: String,
    /// `exceptAucUserIds` (comma-joined).
    pub except_auc_user_ids: String,
    /// `exceptCatalogIds` (comma-joined).
    pub except_catalog_ids: String,
    /// `shpSpec`: spec tokens joined with single spaces, as the app joins
    /// its ShpSpec list for this key. Passed through verbatim.
    pub shp_spec: String,
    /// `specSizeType` (SizeGenderAndType value, e.g. `MENS_SHOES`).
    pub spec_size_type: String,
    /// `specSizeIds` (comma-joined numeric ids).
    pub spec_size_ids: String,
    /// `conditions` as raw ItemCondition ids (comma-joined). When set,
    /// takes precedence over the single-value `condition` vocabulary.
    pub conditions: String,
    /// `submitType` raw value (`ALL` / `AUCTION` / `FIXED`). When set,
    /// takes precedence over `buynow_only`.
    pub submit_type: String,
    /// `priority` (Priority enum value, passed through verbatim).
    pub priority: String,
    /// `imageUrl` (image search).
    pub image_url: String,
    /// `searchType` (SearchType enum value, passed through verbatim).
    pub search_type: String,
    /// `timeFromFirstStart` (unix seconds).
    pub time_from_first_start: Option<i64>,
    /// `rewriteQueryType` (comma-joined RewriteQueryType values).
    pub rewrite_query_type: String,
    /// `campaignType` (CampaignType enum value, passed through verbatim).
    pub campaign_type: String,
    /// `campaignStartTime` (unix seconds).
    pub campaign_start_time: Option<i64>,
    /// `campaignEndTime` (unix seconds).
    pub campaign_end_time: Option<i64>,
    /// `module` (comma-joined module names).
    pub module: String,
    /// `src` (Source enum value, passed through verbatim).
    pub src: String,
}

/// Sort values valid for open search (SortOrder.Search.Open, round-1 §4.4).
const OPEN_SORTS: &[&str] = &[
    "-RANKING",
    "+PRICE",
    "-PRICE",
    "+TOTAL_PRICE",
    "-TOTAL_PRICE",
    "+END_TIME",
    "-END_TIME",
    "+BID_COUNT",
    "-BID_COUNT",
    "-WATCH_COUNT",
    "-FEATURED",
    "-FIRST_START_TIME",
];
/// Sort values valid for closed search (SortOrder.Search.Closed).
const CLOSED_SORTS: &[&str] = &[
    "+PRICE",
    "-PRICE",
    "+START_PRICE",
    "-START_PRICE",
    "+END_TIME",
    "-END_TIME",
    "+BID_COUNT",
    "-BID_COUNT",
];

/// Map Shopping_MCP sort vocabulary to the app API `sort` values
/// (SortOrder.Search in the APK). `newest` differs by status: open sorts by
/// first start time, closed by end time.
///
/// [S1-26] A literal API value (e.g. `+TOTAL_PRICE`, `-WATCH_COUNT`,
/// `-FEATURED`, `+START_PRICE`) is accepted as-is when it belongs to the
/// status's sort set; a value from the other status's set falls back to
/// the default instead of sending a sort the app never sends there.
fn api_sort(s: &str, is_closed: bool) -> &'static str {
    let valid: &[&str] = if is_closed { CLOSED_SORTS } else { OPEN_SORTS };
    let upper = s.trim().to_uppercase();
    if let Some(v) = valid.iter().find(|v| **v == upper) {
        return v;
    }
    match s.trim().to_lowercase().as_str() {
        "price_asc" | "lowest_price" => "+PRICE",
        "price_desc" | "highest_price" => "-PRICE",
        "bids" | "bid_count_desc" => "-BID_COUNT",
        "bids_asc" | "bid_count_asc" => "+BID_COUNT",
        "end_time" | "ending_soonest" | "end" => "+END_TIME",
        "end_time_desc" | "ending_latest" => "-END_TIME",
        "newest" | "new" => {
            if is_closed {
                "-END_TIME"
            } else {
                "-FIRST_START_TIME"
            }
        }
        // Open-only sorts ([S1-26]).
        "total_price_asc" | "total_price" => {
            if is_closed {
                "-PRICE"
            } else {
                "+TOTAL_PRICE"
            }
        }
        "total_price_desc" => {
            if is_closed {
                "-PRICE"
            } else {
                "-TOTAL_PRICE"
            }
        }
        "watch_count" | "watchers" | "most_watched" => {
            if is_closed {
                "-BID_COUNT"
            } else {
                "-WATCH_COUNT"
            }
        }
        "featured" => {
            if is_closed {
                "-RANKING"
            } else {
                "-FEATURED"
            }
        }
        // Closed-only sorts ([S1-26]): start price has no open equivalent,
        // so open falls back to the plain price sort on the same axis.
        "start_price_asc" => {
            if is_closed {
                "+START_PRICE"
            } else {
                "+PRICE"
            }
        }
        "start_price_desc" => {
            if is_closed {
                "-START_PRICE"
            } else {
                "-PRICE"
            }
        }
        _ => "-RANKING",
    }
}

/// Map Shopping_MCP condition vocabulary to the app API `conditions`
/// values (ItemCondition ids in the APK).
fn api_condition(s: &str) -> Option<&'static str> {
    match s.to_lowercase().as_str() {
        "new" | "新品" => Some("NEW"),
        "unused" | "未使用" => Some("USED10"),
        "nearly_new" | "目立った傷や汚れなし" => Some("USED20"),
        "used" | "中古" => Some("USED40"),
        "slightly_used" | "やや傷や汚れあり" => Some("USED40"),
        "used_fair" | "傷や汚れあり" => Some("USED60"),
        "junk" | "全体的に状態が悪い" => Some("USED80"),
        _ => None,
    }
}

/// The API returns the seller good-rating as a ratio (0.992); present it
/// the way the app/Web do ("99.2%").
fn format_good_ratio(ratio: f64) -> String {
    format!("{:.1}%", ratio * 100.0)
}

fn seller_is_store(seller_type: &str) -> Option<bool> {
    match seller_type {
        "STORE" => Some(true),
        "AUCTION" => Some(false),
        _ => None,
    }
}

fn push_str(qp: &mut Vec<(String, String)>, key: &str, value: &str) {
    if !value.trim().is_empty() {
        qp.push((key.into(), value.trim().to_string()));
    }
}

fn push_i64(qp: &mut Vec<(String, String)>, key: &str, value: Option<i64>) {
    if let Some(v) = value {
        qp.push((key.into(), v.to_string()));
    }
}

fn push_bool(qp: &mut Vec<(String, String)>, key: &str, value: Option<bool>) {
    if let Some(v) = value {
        qp.push((key.into(), v.to_string()));
    }
}

/// Build the `list/v2/search/items` query pairs from `SearchParams`,
/// following `Search.Request.toQueryMap()` (round-1 report §4.4): the
/// always-sent keys first, then each conditional key only when set.
/// Returns the pairs and the effective keyword string (AND/NOT keywords
/// folded in). Pure, so the full key set is unit-testable offline.
fn build_search_query(params: &SearchParams) -> (Vec<(String, String)>, String) {
    let is_closed = params.status == "closed";
    let limit = if params.limit > 0 { params.limit } else { 20 };
    let page = if params.page > 0 { params.page } else { 1 };

    // The API has no separate AND/NOT keyword fields: fold them into
    // the query string the way the app's search syntax does.
    let mut query = params.query.trim().to_string();
    if !params.keywords_and.trim().is_empty() {
        if !query.is_empty() {
            query.push(' ');
        }
        query.push_str(params.keywords_and.trim());
    }
    for word in params.keywords_not.split_whitespace() {
        if !query.is_empty() {
            query.push(' ');
        }
        query.push('-');
        query.push_str(word);
    }

    let mut qp: Vec<(String, String)> = Vec::new();
    qp.push(("offset".into(), ((page - 1) * limit).to_string()));
    qp.push(("limit".into(), limit.to_string()));
    if !query.is_empty() {
        qp.push(("query".into(), query.clone()));
    }
    push_str(&mut qp, "categoryIds", &params.category_id);
    push_str(&mut qp, "auctionIds", &params.auction_ids);
    push_str(&mut qp, "aucUserIds", &params.seller_id);
    qp.push(("sort".into(), api_sort(&params.sort, is_closed).into()));
    qp.push((
        "status".into(),
        if is_closed { "CLOSED" } else { "OPEN" }.into(),
    ));
    push_str(&mut qp, "priority", &params.priority);
    // Raw `conditions` (comma-joined ids) wins over the single-value
    // `condition` vocabulary mapping.
    if !params.conditions.trim().is_empty() {
        qp.push(("conditions".into(), params.conditions.trim().to_string()));
    } else if let Some(c) = api_condition(&params.condition) {
        qp.push(("conditions".into(), c.into()));
    }
    push_str(&mut qp, "brandIds", &params.brand_ids);
    push_str(&mut qp, "catalogIds", &params.catalog_ids);
    push_str(&mut qp, "imageUrl", &params.image_url);
    push_i64(&mut qp, "minPrice", params.min_price);
    push_i64(&mut qp, "maxPrice", params.max_price);
    push_i64(&mut qp, "minBuyNowPrice", params.min_buy_now_price);
    push_i64(&mut qp, "maxBuyNowPrice", params.max_buy_now_price);
    push_str(&mut qp, "searchType", &params.search_type);
    if params.free_shipping {
        qp.push(("isFreeShipping".into(), "true".into()));
    }
    // Raw `submitType` wins over the `buynow_only` shortcut.
    if !params.submit_type.trim().is_empty() {
        qp.push(("submitType".into(), params.submit_type.trim().to_string()));
    } else if params.buynow_only {
        qp.push(("submitType".into(), "FIXED".into()));
    }
    push_str(&mut qp, "sellerType", &params.seller_type);
    push_str(&mut qp, "prefectureCode", &params.prefecture_code);
    push_str(
        &mut qp,
        "destinationPrefectureCode",
        &params.destination_prefecture_code,
    );
    push_bool(&mut qp, "canPrivacyDelivery", params.can_privacy_delivery);
    push_bool(&mut qp, "isFeatured", params.is_featured);
    push_bool(&mut qp, "isAppraisal", params.is_appraisal);
    push_bool(&mut qp, "isFinishSoon", params.is_finish_soon);
    push_bool(&mut qp, "isConvenienceStore", params.is_convenience_store);
    push_i64(&mut qp, "timeFromFirstStart", params.time_from_first_start);
    push_str(&mut qp, "rewriteQueryType", &params.rewrite_query_type);
    push_bool(&mut qp, "isNewArrival", params.is_new_arrival);
    push_bool(&mut qp, "isAdultOk", params.is_adult_ok);
    push_str(&mut qp, "campaignType", &params.campaign_type);
    push_i64(&mut qp, "campaignStartTime", params.campaign_start_time);
    push_i64(&mut qp, "campaignEndTime", params.campaign_end_time);
    push_str(&mut qp, "shpSpec", &params.shp_spec);
    push_str(&mut qp, "specSizeType", &params.spec_size_type);
    push_str(&mut qp, "specSizeIds", &params.spec_size_ids);
    push_str(&mut qp, "module", &params.module);
    push_str(&mut qp, "exceptAuctionIds", &params.except_auction_ids);
    push_str(&mut qp, "exceptAucUserIds", &params.except_auc_user_ids);
    push_str(&mut qp, "exceptCatalogIds", &params.except_catalog_ids);
    push_bool(&mut qp, "exceptSuspectedFake", params.except_suspected_fake);
    push_str(&mut qp, "src", &params.src);
    qp.push(("isEnableSpeller".into(), "true".into()));
    (qp, query)
}

/// Build the suggest query pairs ([S1-30]): `categoryLimit` / `brandLimit`
/// default to 5 each — the values the provider used to hard-code — and are
/// sent with the caller's values when given.
fn build_suggest_query(
    query: &str,
    limit: i32,
    category_limit: Option<i32>,
    brand_limit: Option<i32>,
) -> Vec<(String, String)> {
    let limit = if limit > 0 { limit } else { 10 };
    vec![
        ("query".into(), query.to_string()),
        ("limit".into(), limit.to_string()),
        (
            "categoryLimit".into(),
            category_limit.unwrap_or(5).to_string(),
        ),
        ("brandLimit".into(), brand_limit.unwrap_or(5).to_string()),
    ]
}

/// Build the hm1 statistics query pairs ([S1-27]).
/// UNVERIFIED: the joiner for several condition ids inside the single
/// `conditions` value is not confirmed (round-1 §8 point 4); comma is
/// used, matching every list parameter of `toQueryMap()`.
fn build_statistics_query(params: &StatisticsParams) -> Vec<(String, String)> {
    let mut qp: Vec<(String, String)> = Vec::new();
    qp.push(("catalogId".into(), params.catalog_id.clone()));
    if !params.conditions.is_empty() {
        qp.push(("conditions".into(), params.conditions.join(",")));
    }
    qp
}

/// Build the yne (auxiliary API) statistics query pairs: same endpoint
/// with `itemState=closed` fixed in the path, plus `shpSpec` /
/// `sellerType`. Their exact interplay with the hm1 variant is part of
/// the same unconfirmed point as the `conditions` joiner.
fn build_statistics_yne_query(params: &StatisticsParams) -> Vec<(String, String)> {
    let mut qp = build_statistics_query(params);
    push_str(&mut qp, "shpSpec", &params.shp_spec);
    push_str(&mut qp, "sellerType", &params.seller_type);
    qp
}

pub struct YahooAuctionsClient {
    api_client: Client,
    web_client: Client,
}

impl YahooAuctionsClient {
    pub fn new() -> Self {
        let mut api_headers = HeaderMap::new();
        api_headers.insert("x-z-os", HeaderValue::from_static("android"));
        api_headers.insert("x-z-app-version", HeaderValue::from_static("8.48.1"));
        api_headers.insert("x-z-os-version", HeaderValue::from_static("34"));
        api_headers.insert("x-z-service", HeaderValue::from_static("auction"));
        Self {
            api_client: Client::builder()
                .user_agent(APP_USER_AGENT)
                .default_headers(api_headers)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .expect("failed to build API HTTP client"),
            web_client: Client::builder()
                .user_agent(WEB_USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .expect("failed to build Web HTTP client"),
        }
    }

    /// GET an app-API path with the mandatory `appid` query attached.
    /// On HTTP errors, returns an error carrying the status and the head
    /// of the body (the API answers `{error:{code,message,name}}` or the
    /// gateway form `{Error:{Message}}`).
    async fn api_get(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        self.api_get_from(API_BASE, path, query).await
    }

    /// Same as `api_get` but against the auxiliary (yne) root base.
    async fn api_get_root(&self, path: &str, query: &[(String, String)]) -> Result<Value> {
        self.api_get_from(API_ROOT_BASE, path, query).await
    }

    async fn api_get_from(
        &self,
        base: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Value> {
        let url = format!("{base}{path}");
        let mut qp: Vec<(String, String)> = query.to_vec();
        qp.push(("appid".into(), APPID.into()));
        let resp = self.api_client.get(&url).query(&qp).send().await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            let head: String = text.chars().take(300).collect();
            anyhow::bail!("yahoo app API HTTP {status} for {path}: {head}");
        }
        let value: Value =
            serde_json::from_str(&text).with_context(|| format!("invalid JSON from {path}"))?;
        if let Some(err) = value.get("error") {
            let msg = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown API error");
            let code = err.get("code").and_then(|v| v.as_str()).unwrap_or("");
            anyhow::bail!("yahoo app API error for {path}: {code} {msg}");
        }
        if let Some(err) = value.get("Error") {
            let msg = err
                .get("Message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown gateway error");
            anyhow::bail!("yahoo app API gateway error for {path}: {msg}");
        }
        Ok(value)
    }

    pub async fn search(&self, params: SearchParams) -> Result<SearchResult> {
        let is_closed = params.status == "closed";
        let limit = if params.limit > 0 { params.limit } else { 20 };
        let page = if params.page > 0 { params.page } else { 1 };
        let (qp, _query) = build_search_query(&params);

        let resp = self.api_get("list/v2/search/items", &qp).await?;
        let (items, total) = parse_search_response(&resp);
        let count = items.len().min(limit as usize);
        Ok(SearchResult {
            provider: "yahoo_auctions".into(),
            ok: true,
            query: params.query.clone(),
            page,
            limit,
            sort: params.sort.clone(),
            status: if is_closed { "closed" } else { "open" }.into(),
            items: items.into_iter().take(limit as usize).collect(),
            count,
            total_available: total,
            search_mode: "appApi".into(),
        })
    }

    pub async fn get_item(&self, auction_id: &str) -> Result<ItemResult> {
        let path = format!("detail/v2/items/{auction_id}");
        let qp = vec![("collectHistory".into(), "false".into())];
        let resp = self.api_get(&path, &qp).await?;
        if resp["auctionId"].is_null() {
            return Ok(ItemResult {
                provider: "yahoo_auctions".into(),
                ok: false,
                item: None,
                error: Some("no_item_data".into()),
            });
        }
        Ok(ItemResult {
            provider: "yahoo_auctions".into(),
            ok: true,
            item: Some(normalize_detail_item(&resp)),
            error: None,
        })
    }

    pub async fn suggest_keywords(&self, query: &str, limit: i32) -> Result<SuggestResult> {
        self.suggest_keywords_with_limits(query, limit, None, None)
            .await
    }

    /// [S1-30] Suggest with caller-chosen `categoryLimit` / `brandLimit`
    /// (`None` keeps the previous fixed value of 5 for that limit).
    pub async fn suggest_keywords_with_limits(
        &self,
        query: &str,
        limit: i32,
        category_limit: Option<i32>,
        brand_limit: Option<i32>,
    ) -> Result<SuggestResult> {
        let qp = build_suggest_query(query, limit, category_limit, brand_limit);
        let resp = self.api_get("search/v2/suggest/queries", &qp).await?;
        let suggestions = parse_suggestions(&resp);
        let count = suggestions.len();
        Ok(SuggestResult {
            provider: "yahoo_auctions".into(),
            ok: true,
            query: query.to_string(),
            suggestions,
            count,
        })
    }

    /// [S1-28] Seller profile (`GET user/v2/users/{aucUserId}/profile`).
    /// Anonymous HTTP 200 verified 2026-11-01 (round-2 report).
    pub async fn get_seller_profile(&self, seller_id: &str) -> Result<SellerProfile> {
        let path = format!("user/v2/users/{seller_id}/profile");
        let resp = self.api_get(&path, &[]).await?;
        Ok(parse_profile_response(&resp, seller_id))
    }

    /// [S1-28] Store profile (`GET user/v1/users/{aucUserId}/store-profile`).
    /// Non-store sellers may legitimately error here; the caller decides.
    pub async fn get_store_profile(&self, seller_id: &str) -> Result<StoreProfileResult> {
        let path = format!("user/v1/users/{seller_id}/store-profile");
        let resp = self.api_get(&path, &[]).await?;
        Ok(parse_store_profile_response(&resp, seller_id))
    }

    /// [S1-27] Closed-price statistics, hm1 variant
    /// (`GET search/v1/search/items/statistics`).
    pub async fn get_statistics(&self, params: StatisticsParams) -> Result<ItemStatisticsResult> {
        let qp = build_statistics_query(&params);
        let resp = self
            .api_get("search/v1/search/items/statistics", &qp)
            .await?;
        Ok(parse_statistics_response(&resp, &params.catalog_id))
    }

    /// [S1-27] Closed-price statistics, yne variant
    /// (`GET app/search/v1/search/items/statistics?itemState=closed`
    /// against the root base). How this differs from the hm1 variant is
    /// unconfirmed (round-1 §8 point 4) — live verification pending.
    pub async fn get_statistics_closed(
        &self,
        params: StatisticsParams,
    ) -> Result<ItemStatisticsResult> {
        let qp = build_statistics_yne_query(&params);
        let resp = self
            .api_get_root(
                "app/search/v1/search/items/statistics?itemState=closed",
                &qp,
            )
            .await?;
        Ok(parse_statistics_response(&resp, &params.catalog_id))
    }

    /// Seller reviews have no app-API equivalent (only a rating summary
    /// exists there), so this stays on the Web rating page as before.
    pub async fn get_reviews(
        &self,
        seller_id: &str,
        page: i32,
        limit: i32,
    ) -> Result<ReviewsResult> {
        let page = if page > 0 { page } else { 1 };
        let limit = if limit > 0 { limit } else { 25 };

        let url = format!("{WEB_BASE}/jp/show/rating");
        let resp = self
            .web_client
            .get(&url)
            .query(&[("auc_user_id", seller_id), ("apg", &page.to_string())])
            .send()
            .await?
            .error_for_status()?;
        let html = resp.text().await?;

        let mut reviews: Vec<Review> = Vec::new();
        for caps in REVIEW_BLOCK_RE.captures_iter(&html) {
            let rating = normalize_rating(&caps[1]);
            let role = if caps[2].contains("出品者") {
                "seller"
            } else {
                "buyer"
            };
            let reviewer_name = caps[3].trim().to_string();
            let reviewer_score: i64 = caps[4].parse().unwrap_or(0);
            let auction_id = caps[5].to_string();
            let item_title = caps[6].trim().to_string();
            let message = caps[7].replace("&nbsp;", " ").trim().to_string();
            let date = caps
                .get(8)
                .map(|m| m.as_str().to_string())
                .unwrap_or_default();

            reviews.push(Review {
                rating: rating.to_string(),
                role: role.to_string(),
                message,
                reviewer_name,
                reviewer_score,
                item_title,
                auction_id,
                date,
            });

            if reviews.len() >= limit as usize {
                break;
            }
        }

        let count = reviews.len();
        // [S1-28] Attach the rating aggregate from the profile API (the
        // other providers' review results carry `ratings_summary` too).
        // Best-effort: a profile failure must not fail the reviews.
        let ratings_summary = self
            .get_seller_profile(seller_id)
            .await
            .ok()
            .and_then(|p| p.ratings_summary);
        Ok(ReviewsResult {
            provider: "yahoo_auctions".into(),
            ok: true,
            seller_id: seller_id.to_string(),
            reviews,
            count,
            page,
            ratings_summary,
        })
    }
}

impl Default for YahooAuctionsClient {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a `RatingsSummary` from a `rating` object as it appears in the
/// profile response and in the detail payload's seller. `good` / `bad`
/// are only present in the detail form.
fn ratings_summary_from_rating(rating: &Value) -> Option<RatingsSummary> {
    if rating.is_null() {
        return None;
    }
    let total = rating["total"].as_i64()?;
    let good_ratio = rating["goodRatio"].as_f64();
    Some(RatingsSummary {
        total,
        good: rating["good"].as_i64(),
        bad: rating["bad"].as_i64(),
        good_ratio,
        rating: good_ratio.map(format_good_ratio),
        rating_status: rating["ratingStatus"].as_str().map(String::from),
        rating_url: rating["ratingUrl"].as_str().map(String::from),
    })
}

fn parse_profile_response(resp: &Value, seller_id: &str) -> SellerProfile {
    SellerProfile {
        provider: "yahoo_auctions".into(),
        ok: true,
        seller_id: resp["aucUserId"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| seller_id.to_string()),
        display_name: resp["displayName"].as_str().map(String::from),
        icon_url: resp["iconUrl"].as_str().map(String::from),
        is_store: resp["isStore"].as_bool(),
        can_display_store_profile: resp["canDisplayStoreProfile"].as_bool(),
        store_url: resp["storeUrl"].as_str().map(String::from),
        is_ekyc_verified: resp["isEkycVerified"].as_bool(),
        is_speed_shipping: resp["isSpeedShipping"].as_bool(),
        myself: resp["myself"].as_bool(),
        following: resp["relation"]["following"].as_bool(),
        ratings_summary: ratings_summary_from_rating(&resp["rating"]),
    }
}

fn parse_store_profile_response(resp: &Value, seller_id: &str) -> StoreProfileResult {
    let banners = resp["banners"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|b| StoreBanner {
                    image_url: b["imageUrl"].as_str().map(String::from),
                    link_url: b["linkUrl"].as_str().map(String::from),
                })
                .collect()
        })
        .unwrap_or_default();
    StoreProfileResult {
        provider: "yahoo_auctions".into(),
        ok: true,
        seller_id: seller_id.to_string(),
        announcement: resp["announcement"].as_str().map(String::from),
        banners,
    }
}

fn parse_statistics_response(resp: &Value, catalog_id: &str) -> ItemStatisticsResult {
    let days = resp["statistics"]["days"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|d| {
                    // Day carries year/month/day flat (serializer
                    // descriptor); fall back to a nested `targetDate`
                    // object, which also exists in the VO.
                    let flat = (d["year"].as_i64(), d["month"].as_i64(), d["day"].as_i64());
                    let (year, month, day) = match flat {
                        (Some(y), Some(m), Some(dd)) => (y, m, dd),
                        _ => (
                            d["targetDate"]["year"].as_i64()?,
                            d["targetDate"]["month"].as_i64()?,
                            d["targetDate"]["day"].as_i64()?,
                        ),
                    };
                    Some(StatisticsDay {
                        year,
                        month,
                        day,
                        date: format!("{year:04}-{month:02}-{day:02}"),
                        count: d["count"].as_i64().unwrap_or(0),
                        avg_price: Price::yen(d["avgPrice"].as_i64().unwrap_or(0)),
                        max_price: Price::yen(d["maxPrice"].as_i64().unwrap_or(0)),
                        min_price: Price::yen(d["minPrice"].as_i64().unwrap_or(0)),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let last_closed_item = if resp["lastClosedItem"].is_object() {
        let l = &resp["lastClosedItem"];
        Some(LastClosedItem {
            end_time: l["endTime"].as_str().map(String::from),
            item_condition: l["itemCondition"].as_str().map(String::from),
            price: l["price"].as_i64().map(Price::yen),
        })
    } else {
        None
    };
    ItemStatisticsResult {
        provider: "yahoo_auctions".into(),
        ok: true,
        catalog_id: catalog_id.to_string(),
        total_results_available: resp["totalResultsAvailable"].as_i64(),
        last_closed_item,
        days,
    }
}

fn parse_payment(raw: &Value) -> Option<PaymentInfo> {
    if !raw.is_object() {
        return None;
    }
    let strings = |v: &Value| -> Option<Vec<String>> {
        v.as_array().map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
    };
    Some(PaymentInfo {
        allow_cash_registration: raw["allowCashRegistration"].as_bool(),
        allow_credit_card: raw["allowCreditCard"].as_bool(),
        allow_installment: raw["allowInstallment"].as_bool(),
        allow_paypay_balance: raw["allowPayPayBalance"].as_bool(),
        allow_paypay_deferred: raw["allowPayPayDeferred"].as_bool(),
        is_easy_payment_available: raw["isEasyPaymentAvailable"].as_bool(),
        banks: strings(&raw["banks"]),
        others: strings(&raw["others"]),
    })
}

fn parse_bid_info(raw: &Value) -> Option<BidInfo> {
    if !raw.is_object() {
        return None;
    }
    Some(BidInfo {
        is_bidder: raw["isBidder"].as_bool(),
        is_deleted_winner: raw["isDeletedWinner"].as_bool(),
        is_highest_bidder: raw["isHighestBidder"].as_bool(),
        is_next_winner: raw["isNextWinner"].as_bool(),
        is_winner: raw["isWinner"].as_bool(),
        next_winner_limit_time: raw["nextWinnerLimitTime"].as_str().map(String::from),
        last_bid_price: raw["lastBidInfo"]["price"].as_i64().map(Price::yen),
        next_bid_price: raw["nextBidInfo"]["price"].as_i64().map(Price::yen),
    })
}

fn normalize_rating(alt: &str) -> &'static str {
    match alt {
        "非常に良い" => "very_good",
        "良い" => "good",
        "どちらでもない" => "neutral",
        "悪い" => "bad",
        "非常に悪い" => "very_bad",
        _ => "unknown",
    }
}

fn parse_search_response(resp: &Value) -> (Vec<Item>, Option<i64>) {
    let total = resp["totalResultsAvailable"].as_i64();
    let items = resp["items"]
        .as_array()
        .map(|arr| arr.iter().filter_map(normalize_search_item).collect())
        .unwrap_or_default();
    (items, total)
}

fn normalize_search_item(raw: &Value) -> Option<Item> {
    let aid = raw["auctionId"].as_str().unwrap_or_default().to_string();
    if aid.is_empty() {
        return None;
    }
    let title = raw["title"].as_str().unwrap_or_default().to_string();

    let price = raw["price"].as_i64().map(Price::yen);
    let buy_now = raw["buyNowPrice"]
        .as_i64()
        .filter(|&v| v > 0)
        .map(Price::yen);
    let bid_count = raw["bidCount"].as_i64();
    let watchers = raw["watchCount"].as_i64();
    let end_time = raw["endTime"].as_str().map(String::from);
    let start_time = raw["startTime"].as_str().map(String::from);
    let image = raw["imageUrl"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    // The app API returns categoryPath as bare numeric IDs with no names,
    // so category / category_path are intentionally left unset here
    // (the detail API does provide names).
    let condition = raw["itemCondition"].as_str().map(String::from);
    let free_shipping = raw["isFreeShipping"].as_bool();
    // [S1-29] Search-side fields that used to be dropped.
    let buy_now_price_no_tax = raw["buyNowPriceNoTax"]
        .as_i64()
        .filter(|&v| v > 0)
        .map(Price::yen);
    let is_fixed_price = raw["isFixedPrice"].as_bool();
    let is_fleamarket_item = raw["isFleamarketItem"].as_bool();
    let is_sold = raw["isSold"].as_bool();
    let quantity = raw["quantity"].as_i64();
    let shp_category = if raw["shpCategory"].is_object() {
        Some(ShpCategory {
            product_id: raw["shpCategory"]["productId"].as_i64(),
            genre_id: raw["shpCategory"]["genreId"].as_i64(),
        })
    } else {
        None
    };

    let seller = raw["seller"].as_object().map(|s| Seller {
        user_id: s
            .get("aucUserId")
            .and_then(|v| v.as_str())
            .map(String::from),
        display_name: s
            .get("displayName")
            .and_then(|v| v.as_str())
            .map(String::from),
        rating: s
            .get("rating")
            .and_then(|r| r.get("goodRatio"))
            .and_then(|v| v.as_f64())
            .map(format_good_ratio),
        is_store: s
            .get("type")
            .and_then(|v| v.as_str())
            .and_then(seller_is_store),
        location: None,
    });

    Some(Item {
        url: format!("{WEB_BASE}/jp/auction/{aid}"),
        product_id: aid.clone(),
        auction_id: aid,
        title,
        source: "yahoo_auctions".into(),
        price,
        buy_now_price: buy_now,
        bid_count,
        watchers,
        end_time,
        start_time,
        image,
        condition,
        free_shipping,
        seller,
        buy_now_price_no_tax,
        is_fixed_price,
        is_fleamarket_item,
        is_sold,
        quantity,
        shp_category,
        ..Default::default()
    })
}

fn normalize_detail_item(raw: &Value) -> Item {
    let aid = raw["auctionId"].as_str().unwrap_or_default().to_string();
    let title = raw["title"].as_str().unwrap_or_default().to_string();
    let price = raw["price"].as_i64().map(Price::yen);
    let buy_now = raw["buyNowPrice"]
        .as_i64()
        .filter(|&v| v > 0)
        .map(Price::yen);
    let bid_count = raw["bidCount"].as_i64();
    // The app API names this `watchCount` (the old Web payload used
    // `watchListNum`).
    let watchers = raw["watchCount"].as_i64();
    let end_time = raw["endTime"].as_str().map(String::from);
    let start_time = raw["startTime"].as_str().map(String::from);
    let status = raw["status"].as_str().map(String::from);
    let condition = raw["condition"].as_str().map(String::from);

    // Prefer the plain-text description over the HTML one.
    let description = raw["plainDescription"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| raw["description"].as_str().map(String::from));

    let (image, images) = if let Some(imgs) = raw["images"].as_array() {
        let all: Vec<String> = imgs
            .iter()
            .filter_map(|im| im["url"].as_str().map(String::from))
            .collect();
        let first = all.first().cloned();
        (first, if all.is_empty() { None } else { Some(all) })
    } else {
        (None, None)
    };

    let shipping = &raw["shippingInfo"];
    let seller = raw["seller"].as_object().map(|s| Seller {
        user_id: s
            .get("aucUserId")
            .and_then(|v| v.as_str())
            .map(String::from),
        display_name: s
            .get("displayName")
            .and_then(|v| v.as_str())
            .map(String::from),
        rating: s
            .get("rating")
            .and_then(|r| r.get("goodRatio"))
            .and_then(|v| v.as_f64())
            .map(format_good_ratio),
        is_store: None,
        // The shipping location lives on shippingInfo in the app API;
        // the old Web detail exposed it as the seller location too.
        location: shipping["location"].as_str().map(String::from),
    });

    let category_path = raw["categoryList"].as_array().map(|arr| {
        arr.iter()
            .filter_map(|c| c["name"].as_str().map(String::from))
            .collect::<Vec<_>>()
    });
    let category = category_path.as_ref().and_then(|p| p.last().cloned());

    let brand = raw["brandList"]
        .as_array()
        .and_then(|arr| arr.last())
        .and_then(|b| b["name"].as_str())
        .map(String::from);

    let free_shipping = shipping["isFreeShipping"].as_bool();

    Item {
        url: format!("{WEB_BASE}/jp/auction/{aid}"),
        product_id: aid.clone(),
        auction_id: aid,
        title,
        source: "yahoo_auctions".into(),
        price,
        buy_now_price: buy_now,
        bid_count,
        watchers,
        end_time,
        start_time,
        status,
        condition,
        description,
        image,
        images,
        seller,
        category,
        category_path,
        brand,
        free_shipping,
        // No shipping-method list exists in the app detail payload.
        shipping_method: None,
        // [S1-29] Detail-side fields that used to be dropped.
        init_price: raw["initPrice"].as_i64().map(Price::yen),
        price_no_tax: raw["priceNoTax"].as_i64().map(Price::yen),
        buy_now_price_no_tax: raw["buyNowPriceNoTax"]
            .as_i64()
            .filter(|&v| v > 0)
            .map(Price::yen),
        qa_count: raw["qaCount"].as_i64(),
        payment: parse_payment(&raw["payment"]),
        bid_info: parse_bid_info(&raw["bidInfo"]),
        quantity: raw["quantity"].as_i64(),
        update_time: raw["updateTime"].as_str().map(String::from),
        is_fixed_price: raw["isFixedPrice"].as_bool(),
        is_fleamarket_item: raw["isListingFleamarket"].as_bool(),
        is_sold: None,
        shp_category: None,
    }
}

fn parse_suggestions(resp: &Value) -> Vec<Suggestion> {
    let Some(queries) = resp["queries"].as_array() else {
        return Vec::new();
    };
    queries
        .iter()
        .filter_map(|q| {
            let keyword = q["keyword"].as_str()?.to_string();
            let category = q.get("category");
            let category_id = category.and_then(|c| c["id"].as_i64());
            let category_path = category.and_then(|c| {
                c["namePath"].as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
            });
            let brand = q.get("brand").and_then(|b| {
                b.get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| {
                        b["namePath"]
                            .as_array()
                            .and_then(|arr| arr.last().and_then(|v| v.as_str()).map(String::from))
                    })
            });
            Some(Suggestion {
                keyword,
                category_id,
                category_path,
                brand,
            })
        })
        .collect()
}

impl Default for Item {
    fn default() -> Self {
        Self {
            product_id: String::new(),
            auction_id: String::new(),
            title: String::new(),
            url: String::new(),
            source: "yahoo_auctions".into(),
            price: None,
            buy_now_price: None,
            bid_count: None,
            watchers: None,
            end_time: None,
            start_time: None,
            image: None,
            images: None,
            category: None,
            category_path: None,
            condition: None,
            free_shipping: None,
            seller: None,
            status: None,
            description: None,
            brand: None,
            shipping_method: None,
            init_price: None,
            price_no_tax: None,
            buy_now_price_no_tax: None,
            qa_count: None,
            payment: None,
            bid_info: None,
            quantity: None,
            update_time: None,
            is_fixed_price: None,
            is_fleamarket_item: None,
            is_sold: None,
            shp_category: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Trimmed copy of the verified live sample
    // (~/workspace/yauction-analysis/live/search-open-appid.json, 2026-11-01).
    fn sample_search_item() -> Value {
        serde_json::json!({
            "auctionId": "m1250380714",
            "title": "美品 iPhone 17 Pro 1TB",
            "imageUrl": "https://example.com/img.jpg",
            "price": 173005,
            "buyNowPrice": null,
            "isFixedPrice": false,
            "startTime": "2026-10-30T23:59:06+09:00",
            "endTime": "2026-11-01T00:23:42+09:00",
            "itemCondition": "USED40",
            "bidCount": 113,
            "watchCount": 36,
            "isFreeShipping": false,
            "categoryPath": [0, 23632, 23960, 2084317598, 2084317599],
            "seller": {"aucUserId": "BZHRJg8RuSfMn2oHCtUiWnmfAwjPw", "type": "AUCTION", "rating": {"goodRatio": 0.992}}
        })
    }

    #[test]
    fn test_api_sort() {
        assert_eq!(api_sort("", false), "-RANKING");
        assert_eq!(api_sort("relevance", false), "-RANKING");
        assert_eq!(api_sort("default", true), "-RANKING");
        assert_eq!(api_sort("price_asc", false), "+PRICE");
        assert_eq!(api_sort("price_desc", false), "-PRICE");
        assert_eq!(api_sort("bids", false), "-BID_COUNT");
        assert_eq!(api_sort("bids", true), "-BID_COUNT");
        assert_eq!(api_sort("end_time", false), "+END_TIME");
        assert_eq!(api_sort("ending_soonest", true), "+END_TIME");
        assert_eq!(api_sort("newest", false), "-FIRST_START_TIME");
        assert_eq!(api_sort("newest", true), "-END_TIME");
        assert_eq!(api_sort("unknown_sort", false), "-RANKING");
    }

    #[test]
    fn test_api_condition() {
        assert_eq!(api_condition("new"), Some("NEW"));
        assert_eq!(api_condition("新品"), Some("NEW"));
        assert_eq!(api_condition("unused"), Some("USED10"));
        assert_eq!(api_condition("nearly_new"), Some("USED20"));
        assert_eq!(api_condition("used"), Some("USED40"));
        assert_eq!(api_condition("slightly_used"), Some("USED40"));
        assert_eq!(api_condition("used_fair"), Some("USED60"));
        assert_eq!(api_condition("junk"), Some("USED80"));
        assert_eq!(api_condition(""), None);
        assert_eq!(api_condition("unknown"), None);
    }

    #[test]
    fn test_format_good_ratio() {
        assert_eq!(format_good_ratio(0.992), "99.2%");
        assert_eq!(format_good_ratio(1.0), "100.0%");
        assert_eq!(format_good_ratio(0.99), "99.0%");
        assert_eq!(format_good_ratio(0.946), "94.6%");
    }

    #[test]
    fn test_seller_is_store() {
        assert_eq!(seller_is_store("STORE"), Some(true));
        assert_eq!(seller_is_store("AUCTION"), Some(false));
        // Seen live (AUC_STORE / FLEAMARKET): not guessed at.
        assert_eq!(seller_is_store("AUC_STORE"), None);
        assert_eq!(seller_is_store("FLEAMARKET"), None);
    }

    #[test]
    fn test_normalize_search_item() {
        let item = normalize_search_item(&sample_search_item()).unwrap();
        assert_eq!(item.auction_id, "m1250380714");
        assert_eq!(item.product_id, "m1250380714");
        assert!(item.url.ends_with("/jp/auction/m1250380714"));
        assert_eq!(item.price.as_ref().unwrap().value, 173005);
        assert_eq!(item.price.as_ref().unwrap().currency, "JPY");
        assert!(
            item.buy_now_price.is_none(),
            "null buyNowPrice must stay None"
        );
        assert_eq!(item.bid_count, Some(113));
        assert_eq!(item.watchers, Some(36));
        assert_eq!(item.image.as_deref(), Some("https://example.com/img.jpg"));
        assert_eq!(item.condition.as_deref(), Some("USED40"));
        assert_eq!(item.free_shipping, Some(false));
        // categoryPath is bare numeric IDs in the app API: no names.
        assert!(item.category.is_none());
        assert!(item.category_path.is_none());
        let seller = item.seller.as_ref().unwrap();
        assert_eq!(
            seller.user_id.as_deref(),
            Some("BZHRJg8RuSfMn2oHCtUiWnmfAwjPw")
        );
        assert_eq!(seller.rating.as_deref(), Some("99.2%"));
        assert_eq!(seller.is_store, Some(false));
    }

    #[test]
    fn test_parse_search_response() {
        let resp = serde_json::json!({
            "totalResultsAvailable": 370056,
            "items": [sample_search_item(), {"auctionId": "x1", "title": "t", "price": 10, "buyNowPrice": 500, "seller": {"aucUserId": "s", "type": "AUC_STORE", "rating": {"goodRatio": 0.99}}}]
        });
        let (items, total) = parse_search_response(&resp);
        assert_eq!(total, Some(370056));
        assert_eq!(items.len(), 2);
        assert_eq!(items[1].buy_now_price.as_ref().unwrap().value, 500);
        assert_eq!(items[1].seller.as_ref().unwrap().is_store, None);
        assert_eq!(
            items[1].seller.as_ref().unwrap().rating.as_deref(),
            Some("99.0%")
        );
    }

    #[test]
    fn test_normalize_detail_item() {
        // Trimmed copy of the verified live sample
        // (~/workspace/yauction-analysis/live/detail-appid.json, 2026-11-01).
        let raw = serde_json::json!({
            "auctionId": "m1250380714",
            "title": "美品 iPhone 17 Pro 1TB",
            "description": "<DIV>HTML説明</DIV>",
            "plainDescription": "プレーン説明",
            "images": [{"url": "https://example.com/1.jpg", "comment": null}, {"url": "https://example.com/2.jpg", "comment": null}],
            "price": 173005,
            "buyNowPrice": null,
            "status": "OPEN",
            "startTime": "2026-10-30T23:59:06+09:00",
            "endTime": "2026-11-01T00:23:42+09:00",
            "bidCount": 113,
            "watchCount": 36,
            "condition": "USED40",
            "seller": {
                "aucUserId": "BZHRJg8RuSfMn2oHCtUiWnmfAwjPw",
                "displayName": "rab********",
                "rating": {"total": 10539, "good": 10627, "bad": 88, "goodRatio": 0.992, "ratingStatus": "RATED"}
            },
            "categoryList": [{"id": 0, "name": "すべて"}, {"id": 23632, "name": "家電、AV、カメラ"}, {"id": 2084317599, "name": "iPhone"}],
            "brandList": [{"id": 100011, "name": "Apple"}, {"id": 205733, "name": "iPhone 17 Pro"}],
            "shippingInfo": {"isFreeShipping": false, "chargeForShipping": "BUYER", "location": "広島県"}
        });
        let item = normalize_detail_item(&raw);
        assert_eq!(item.auction_id, "m1250380714");
        assert_eq!(item.price.as_ref().unwrap().value, 173005);
        assert!(item.buy_now_price.is_none());
        assert_eq!(item.bid_count, Some(113));
        // app API key is watchCount, not the old Web watchListNum.
        assert_eq!(item.watchers, Some(36));
        assert_eq!(item.status.as_deref(), Some("OPEN"));
        assert_eq!(item.condition.as_deref(), Some("USED40"));
        // plainDescription wins over the HTML description.
        assert_eq!(item.description.as_deref(), Some("プレーン説明"));
        assert_eq!(item.image.as_deref(), Some("https://example.com/1.jpg"));
        assert_eq!(item.images.as_ref().unwrap().len(), 2);
        assert_eq!(item.category.as_deref(), Some("iPhone"));
        assert_eq!(
            item.category_path.as_ref().unwrap(),
            &vec![
                "すべて".to_string(),
                "家電、AV、カメラ".to_string(),
                "iPhone".to_string()
            ]
        );
        assert_eq!(item.brand.as_deref(), Some("iPhone 17 Pro"));
        assert_eq!(item.free_shipping, Some(false));
        assert!(item.shipping_method.is_none());
        let seller = item.seller.as_ref().unwrap();
        assert_eq!(
            seller.user_id.as_deref(),
            Some("BZHRJg8RuSfMn2oHCtUiWnmfAwjPw")
        );
        assert_eq!(seller.display_name.as_deref(), Some("rab********"));
        assert_eq!(seller.rating.as_deref(), Some("99.2%"));
        assert_eq!(seller.location.as_deref(), Some("広島県"));
    }

    #[test]
    fn test_normalize_detail_item_html_description_fallback() {
        let raw = serde_json::json!({
            "auctionId": "a1",
            "title": "t",
            "description": "<DIV>HTMLのみ</DIV>",
            "plainDescription": "",
            "price": 100
        });
        let item = normalize_detail_item(&raw);
        assert_eq!(item.description.as_deref(), Some("<DIV>HTMLのみ</DIV>"));
        assert!(item.images.is_none());
        assert!(item.seller.is_none());
    }

    #[test]
    fn test_parse_suggestions() {
        // Verified live sample shape
        // (~/workspace/yauction-analysis/live/suggest-appid.json, 2026-11-01).
        let resp = serde_json::json!({
            "queries": [
                {"keyword": "iphone", "category": {"id": 2084005061, "namePath": ["オークション", "家電、AV、カメラ", "携帯電話、スマートフォン", "アクセサリー"]}},
                {"keyword": "iphone"},
                {"keyword": "iphone17"}
            ]
        });
        let suggestions = parse_suggestions(&resp);
        assert_eq!(suggestions.len(), 3);
        assert_eq!(suggestions[0].keyword, "iphone");
        assert_eq!(suggestions[0].category_id, Some(2084005061));
        assert_eq!(suggestions[0].category_path.as_ref().unwrap().len(), 4);
        assert!(suggestions[1].category_id.is_none());
        assert!(suggestions[1].category_path.is_none());
        assert_eq!(suggestions[2].keyword, "iphone17");
        assert!(parse_suggestions(&serde_json::json!({})).is_empty());
    }

    fn qp_map(qp: &[(String, String)]) -> std::collections::HashMap<&str, &str> {
        qp.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
    }

    #[test]
    fn test_api_sort_new_values() {
        // [S1-26] Literal API values pass through when valid for the status.
        assert_eq!(api_sort("+TOTAL_PRICE", false), "+TOTAL_PRICE");
        assert_eq!(api_sort("-TOTAL_PRICE", false), "-TOTAL_PRICE");
        assert_eq!(api_sort("-WATCH_COUNT", false), "-WATCH_COUNT");
        assert_eq!(api_sort("-FEATURED", false), "-FEATURED");
        assert_eq!(api_sort("+START_PRICE", true), "+START_PRICE");
        assert_eq!(api_sort("-START_PRICE", true), "-START_PRICE");
        assert_eq!(api_sort("+BID_COUNT", false), "+BID_COUNT");
        assert_eq!(api_sort("+BID_COUNT", true), "+BID_COUNT");
        // Vocabulary forms.
        assert_eq!(api_sort("total_price_asc", false), "+TOTAL_PRICE");
        assert_eq!(api_sort("total_price_desc", false), "-TOTAL_PRICE");
        assert_eq!(api_sort("watch_count", false), "-WATCH_COUNT");
        assert_eq!(api_sort("most_watched", false), "-WATCH_COUNT");
        assert_eq!(api_sort("featured", false), "-FEATURED");
        assert_eq!(api_sort("start_price_asc", true), "+START_PRICE");
        assert_eq!(api_sort("start_price_desc", true), "-START_PRICE");
        assert_eq!(api_sort("bids_asc", false), "+BID_COUNT");
        assert_eq!(api_sort("end_time_desc", false), "-END_TIME");
        // A literal from the other status's set is never sent as-is.
        assert_eq!(api_sort("+TOTAL_PRICE", true), "-RANKING");
        assert_eq!(api_sort("+START_PRICE", false), "-RANKING");
        assert_eq!(api_sort("-FEATURED", true), "-RANKING");
    }

    #[test]
    fn test_build_search_query_full_params() {
        // [S1-25] Every §4.4 key the provider now supports must reach the
        // wire map with the app's key name and joining.
        let params = SearchParams {
            query: "iphone".into(),
            limit: 20,
            page: 2,
            sort: "total_price_asc".into(),
            status: "open".into(),
            min_price: Some(1000),
            max_price: Some(50000),
            category_id: "2084317599".into(),
            seller_id: "seller1".into(),
            condition: "used".into(),
            brand_ids: "205733,100011".into(),
            catalog_ids: "926964cf77".into(),
            min_buy_now_price: Some(2000),
            max_buy_now_price: Some(60000),
            seller_type: "AUC_STORE,AUCTION".into(),
            prefecture_code: "TOKYO,OSAKA".into(),
            destination_prefecture_code: "HOKKAIDO".into(),
            is_featured: Some(true),
            is_appraisal: Some(false),
            is_finish_soon: Some(true),
            is_convenience_store: Some(false),
            is_new_arrival: Some(true),
            except_suspected_fake: Some(true),
            can_privacy_delivery: Some(true),
            is_adult_ok: Some(false),
            auction_ids: "m1,m2".into(),
            except_auction_ids: "x1".into(),
            except_auc_user_ids: "bad1".into(),
            except_catalog_ids: "c9".into(),
            shp_spec: "specA specB".into(),
            spec_size_type: "MENS_SHOES".into(),
            spec_size_ids: "101,102".into(),
            priority: "PRIORITY".into(),
            image_url: "https://example.com/i.jpg".into(),
            search_type: "ALL".into(),
            time_from_first_start: Some(1700000000),
            rewrite_query_type: "BRAND".into(),
            campaign_type: "SALE".into(),
            campaign_start_time: Some(1700000000),
            campaign_end_time: Some(1700086400),
            module: "BRAND,CATEGORY".into(),
            src: "SEARCH".into(),
            ..Default::default()
        };
        let (qp, query) = build_search_query(&params);
        assert_eq!(query, "iphone");
        let m = qp_map(&qp);
        assert_eq!(m["offset"], "20");
        assert_eq!(m["limit"], "20");
        assert_eq!(m["query"], "iphone");
        assert_eq!(m["sort"], "+TOTAL_PRICE");
        assert_eq!(m["status"], "OPEN");
        assert_eq!(m["isEnableSpeller"], "true");
        assert_eq!(m["categoryIds"], "2084317599");
        assert_eq!(m["aucUserIds"], "seller1");
        assert_eq!(m["conditions"], "USED40");
        assert_eq!(m["minPrice"], "1000");
        assert_eq!(m["maxPrice"], "50000");
        assert_eq!(m["brandIds"], "205733,100011");
        assert_eq!(m["catalogIds"], "926964cf77");
        assert_eq!(m["minBuyNowPrice"], "2000");
        assert_eq!(m["maxBuyNowPrice"], "60000");
        assert_eq!(m["sellerType"], "AUC_STORE,AUCTION");
        assert_eq!(m["prefectureCode"], "TOKYO,OSAKA");
        assert_eq!(m["destinationPrefectureCode"], "HOKKAIDO");
        assert_eq!(m["isFeatured"], "true");
        assert_eq!(m["isAppraisal"], "false");
        assert_eq!(m["isFinishSoon"], "true");
        assert_eq!(m["isConvenienceStore"], "false");
        assert_eq!(m["isNewArrival"], "true");
        assert_eq!(m["exceptSuspectedFake"], "true");
        assert_eq!(m["canPrivacyDelivery"], "true");
        assert_eq!(m["isAdultOk"], "false");
        assert_eq!(m["auctionIds"], "m1,m2");
        assert_eq!(m["exceptAuctionIds"], "x1");
        assert_eq!(m["exceptAucUserIds"], "bad1");
        assert_eq!(m["exceptCatalogIds"], "c9");
        assert_eq!(m["shpSpec"], "specA specB");
        assert_eq!(m["specSizeType"], "MENS_SHOES");
        assert_eq!(m["specSizeIds"], "101,102");
        assert_eq!(m["priority"], "PRIORITY");
        assert_eq!(m["imageUrl"], "https://example.com/i.jpg");
        assert_eq!(m["searchType"], "ALL");
        assert_eq!(m["timeFromFirstStart"], "1700000000");
        assert_eq!(m["rewriteQueryType"], "BRAND");
        assert_eq!(m["campaignType"], "SALE");
        assert_eq!(m["campaignStartTime"], "1700000000");
        assert_eq!(m["campaignEndTime"], "1700086400");
        assert_eq!(m["module"], "BRAND,CATEGORY");
        assert_eq!(m["src"], "SEARCH");
    }

    #[test]
    fn test_build_search_query_defaults_and_precedence() {
        // Defaults: none of the [S1-25] keys are sent.
        let (qp, _) = build_search_query(&SearchParams {
            query: "x".into(),
            ..Default::default()
        });
        let m = qp_map(&qp);
        for key in [
            "brandIds",
            "catalogIds",
            "minBuyNowPrice",
            "maxBuyNowPrice",
            "sellerType",
            "prefectureCode",
            "destinationPrefectureCode",
            "isFeatured",
            "isAppraisal",
            "isFinishSoon",
            "isConvenienceStore",
            "isNewArrival",
            "exceptSuspectedFake",
            "canPrivacyDelivery",
            "auctionIds",
            "exceptAuctionIds",
            "shpSpec",
            "specSizeIds",
            "submitType",
            "conditions",
        ] {
            assert!(!m.contains_key(key), "default must not send {key}");
        }
        // buynow_only shortcut still maps to submitType=FIXED.
        let (qp, _) = build_search_query(&SearchParams {
            buynow_only: true,
            free_shipping: true,
            ..Default::default()
        });
        let m = qp_map(&qp);
        assert_eq!(m["submitType"], "FIXED");
        assert_eq!(m["isFreeShipping"], "true");
        // Raw submit_type / conditions take precedence.
        let (qp, _) = build_search_query(&SearchParams {
            buynow_only: true,
            submit_type: "AUCTION".into(),
            condition: "used".into(),
            conditions: "NEW,USED10".into(),
            ..Default::default()
        });
        let m = qp_map(&qp);
        assert_eq!(m["submitType"], "AUCTION");
        assert_eq!(m["conditions"], "NEW,USED10");
    }

    #[test]
    fn test_build_statistics_queries() {
        // [S1-27]
        let params = StatisticsParams {
            catalog_id: "926964cf77".into(),
            conditions: vec!["USED40".into(), "USED60".into()],
            shp_spec: "specA".into(),
            seller_type: "AUCTION".into(),
        };
        let q = build_statistics_query(&params);
        let m = qp_map(&q);
        assert_eq!(m["catalogId"], "926964cf77");
        // Joiner is comma pending live confirmation (see notes-ya.md).
        assert_eq!(m["conditions"], "USED40,USED60");
        assert!(!m.contains_key("shpSpec"), "hm1 variant has no shpSpec");
        let q = build_statistics_yne_query(&params);
        let m = qp_map(&q);
        assert_eq!(m["shpSpec"], "specA");
        assert_eq!(m["sellerType"], "AUCTION");
        // No conditions -> key omitted entirely.
        let empty = StatisticsParams {
            catalog_id: "c1".into(),
            ..Default::default()
        };
        let q = build_statistics_query(&empty);
        let m = qp_map(&q);
        assert!(!m.contains_key("conditions"));
    }

    #[test]
    fn test_parse_statistics_response() {
        // [S1-27] Shape from ItemStatistics.Response (round-1 §4.3):
        // days[{avgPrice,count,maxPrice,minPrice,year/month/day}],
        // lastClosedItem, totalResultsAvailable.
        let resp = serde_json::json!({
            "totalResultsAvailable": 42,
            "lastClosedItem": {"endTime": "2026-10-31T21:00:00+09:00", "itemCondition": "USED40", "price": 150000},
            "statistics": {"days": [
                {"year": 2026, "month": 10, "day": 30, "count": 3, "avgPrice": 140000, "maxPrice": 155000, "minPrice": 120000},
                {"year": 2026, "month": 10, "day": 31, "count": 1, "avgPrice": 150000, "maxPrice": 150000, "minPrice": 150000}
            ]}
        });
        let r = parse_statistics_response(&resp, "926964cf77");
        assert!(r.ok);
        assert_eq!(r.catalog_id, "926964cf77");
        assert_eq!(r.total_results_available, Some(42));
        let last = r.last_closed_item.as_ref().unwrap();
        assert_eq!(last.price.as_ref().unwrap().value, 150000);
        assert_eq!(last.item_condition.as_deref(), Some("USED40"));
        assert_eq!(r.days.len(), 2);
        assert_eq!(r.days[0].date, "2026-10-30");
        assert_eq!(r.days[0].count, 3);
        assert_eq!(r.days[0].avg_price.value, 140000);
        assert_eq!(r.days[0].max_price.value, 155000);
        assert_eq!(r.days[0].min_price.value, 120000);
        assert_eq!(r.days[1].date, "2026-10-31");
        // Nested targetDate fallback.
        let resp2 = serde_json::json!({
            "totalResultsAvailable": 1,
            "statistics": {"days": [
                {"targetDate": {"year": 2026, "month": 9, "day": 5}, "count": 2, "avgPrice": 100, "maxPrice": 110, "minPrice": 90}
            ]}
        });
        let r2 = parse_statistics_response(&resp2, "c1");
        assert_eq!(r2.days[0].date, "2026-09-05");
        assert!(r2.last_closed_item.is_none());
    }

    #[test]
    fn test_parse_profile_response() {
        // [S1-28] Trimmed copy of the verified live sample
        // (~/workspace/yauction-analysis/live/profile-appid.json, 2026-11-01).
        let resp = serde_json::json!({
            "aucUserId": "BZHRJg8RuSfMn2oHCtUiWnmfAwjPw",
            "displayName": "rab********",
            "iconUrl": "https://s.yimg.jp/images/account/sp/img/display_name/user/128/00.png",
            "isStore": false,
            "canDisplayStoreProfile": false,
            "storeUrl": null,
            "isEkycVerified": true,
            "myself": false,
            "isSpeedShipping": false,
            "rating": {"total": 10539, "goodRatio": 0.992, "ratingStatus": "RATED", "ratingUrl": "https://auctions.yahoo.co.jp/jp/show/rating?auc_user_id=BZHRJg8RuSfMn2oHCtUiWnmfAwjPw"},
            "relation": {"following": false, "alertId": null}
        });
        let p = parse_profile_response(&resp, "BZHRJg8RuSfMn2oHCtUiWnmfAwjPw");
        assert!(p.ok);
        assert_eq!(p.display_name.as_deref(), Some("rab********"));
        assert_eq!(p.is_store, Some(false));
        assert_eq!(p.is_ekyc_verified, Some(true));
        assert_eq!(p.following, Some(false));
        assert!(p.store_url.is_none());
        let s = p.ratings_summary.as_ref().unwrap();
        assert_eq!(s.total, 10539);
        // Profile Rating has no good/bad split.
        assert!(s.good.is_none());
        assert!(s.bad.is_none());
        assert_eq!(s.good_ratio, Some(0.992));
        assert_eq!(s.rating.as_deref(), Some("99.2%"));
        assert_eq!(s.rating_status.as_deref(), Some("RATED"));
    }

    #[test]
    fn test_ratings_summary_from_detail_rating() {
        // Detail seller rating carries the good/bad split (live sample).
        let rating = serde_json::json!({"total": 10539, "good": 10627, "bad": 88, "goodRatio": 0.992, "ratingStatus": "RATED"});
        let s = ratings_summary_from_rating(&rating).unwrap();
        assert_eq!(s.good, Some(10627));
        assert_eq!(s.bad, Some(88));
        assert!(ratings_summary_from_rating(&serde_json::json!(null)).is_none());
        assert!(ratings_summary_from_rating(&serde_json::json!({})).is_none());
    }

    #[test]
    fn test_parse_store_profile_response() {
        // [S1-28] Shape from StoreProfile.Response {banners, announcement}.
        let resp = serde_json::json!({
            "announcement": "年末年始の発送について",
            "banners": [
                {"imageUrl": "https://example.com/b1.png", "linkUrl": "https://example.com/shop"},
                {"imageUrl": "https://example.com/b2.png", "linkUrl": null}
            ]
        });
        let r = parse_store_profile_response(&resp, "store1");
        assert!(r.ok);
        assert_eq!(r.seller_id, "store1");
        assert_eq!(r.announcement.as_deref(), Some("年末年始の発送について"));
        assert_eq!(r.banners.len(), 2);
        assert_eq!(
            r.banners[0].image_url.as_deref(),
            Some("https://example.com/b1.png")
        );
        assert!(r.banners[1].link_url.is_none());
        let empty = parse_store_profile_response(&serde_json::json!({}), "s");
        assert!(empty.banners.is_empty());
        assert!(empty.announcement.is_none());
    }

    #[test]
    fn test_reviews_result_ratings_summary_serialization() {
        // [S1-28] The JSON key must be `ratings_summary`, like the other
        // providers, and omitted while None.
        let with = ReviewsResult {
            provider: "yahoo_auctions".into(),
            ok: true,
            seller_id: "s".into(),
            reviews: vec![],
            count: 0,
            page: 1,
            ratings_summary: Some(RatingsSummary {
                total: 10,
                good: Some(9),
                bad: Some(1),
                good_ratio: Some(0.9),
                rating: Some("90.0%".into()),
                rating_status: None,
                rating_url: None,
            }),
        };
        let v = serde_json::to_value(&with).unwrap();
        assert_eq!(v["ratings_summary"]["total"], 10);
        assert_eq!(v["ratings_summary"]["good"], 9);
        let without = ReviewsResult {
            ratings_summary: None,
            ..with
        };
        let v = serde_json::to_value(&without).unwrap();
        assert!(v.get("ratings_summary").is_none());
    }

    #[test]
    fn test_normalize_search_item_new_fields() {
        // [S1-29] Live search item shape (search-open-appid.json).
        let mut raw = sample_search_item();
        raw["buyNowPrice"] = serde_json::json!(200000);
        raw["buyNowPriceNoTax"] = serde_json::json!(200000);
        raw["isFixedPrice"] = serde_json::json!(true);
        raw["isFleamarketItem"] = serde_json::json!(true);
        raw["isSold"] = serde_json::json!(false);
        raw["quantity"] = serde_json::json!(3);
        raw["shpCategory"] = serde_json::json!({"productId": 31765, "genreId": 38340});
        let item = normalize_search_item(&raw).unwrap();
        assert_eq!(item.buy_now_price_no_tax.as_ref().unwrap().value, 200000);
        assert_eq!(item.is_fixed_price, Some(true));
        assert_eq!(item.is_fleamarket_item, Some(true));
        assert_eq!(item.is_sold, Some(false));
        assert_eq!(item.quantity, Some(3));
        let shp = item.shp_category.as_ref().unwrap();
        assert_eq!(shp.product_id, Some(31765));
        assert_eq!(shp.genre_id, Some(38340));
        // Absent shpCategory stays None.
        let plain = normalize_search_item(&sample_search_item()).unwrap();
        assert!(plain.shp_category.is_none());
        assert!(plain.buy_now_price_no_tax.is_none());
    }

    #[test]
    fn test_normalize_detail_item_new_fields() {
        // [S1-29] Field shapes from the verified live detail sample
        // (detail-appid.json): payment / bidInfo objects included.
        let raw = serde_json::json!({
            "auctionId": "m1250380714",
            "title": "t",
            "price": 173005,
            "priceNoTax": 173005,
            "initPrice": 1000,
            "buyNowPrice": 250000,
            "buyNowPriceNoTax": 250000,
            "qaCount": 1,
            "quantity": 2,
            "updateTime": "2026-10-31T12:34:56+09:00",
            "isFixedPrice": false,
            "payment": {
                "allowCashRegistration": false,
                "allowCreditCard": true,
                "allowInstallment": false,
                "allowPayPayBalance": true,
                "allowPayPayDeferred": true,
                "banks": [],
                "isEasyPaymentAvailable": true,
                "others": []
            },
            "bidInfo": {
                "isBidder": false,
                "isDeletedWinner": false,
                "isHighestBidder": true,
                "isNextWinner": false,
                "isWinner": false,
                "lastBidInfo": {"isPartial": false, "price": 173005, "quantity": 1},
                "nextBidInfo": {"limitQuantity": 1, "price": 173500},
                "nextWinnerLimitTime": null
            }
        });
        let item = normalize_detail_item(&raw);
        assert_eq!(item.init_price.as_ref().unwrap().value, 1000);
        assert_eq!(item.price_no_tax.as_ref().unwrap().value, 173005);
        assert_eq!(item.buy_now_price_no_tax.as_ref().unwrap().value, 250000);
        assert_eq!(item.qa_count, Some(1));
        assert_eq!(item.quantity, Some(2));
        assert_eq!(
            item.update_time.as_deref(),
            Some("2026-10-31T12:34:56+09:00")
        );
        assert_eq!(item.is_fixed_price, Some(false));
        let pay = item.payment.as_ref().unwrap();
        assert_eq!(pay.allow_credit_card, Some(true));
        assert_eq!(pay.allow_cash_registration, Some(false));
        assert_eq!(pay.allow_paypay_balance, Some(true));
        assert_eq!(pay.is_easy_payment_available, Some(true));
        assert_eq!(pay.banks.as_ref().unwrap().len(), 0);
        let bid = item.bid_info.as_ref().unwrap();
        assert_eq!(bid.is_highest_bidder, Some(true));
        assert_eq!(bid.is_winner, Some(false));
        assert_eq!(bid.last_bid_price.as_ref().unwrap().value, 173005);
        assert_eq!(bid.next_bid_price.as_ref().unwrap().value, 173500);
        assert!(bid.next_winner_limit_time.is_none());
    }

    #[test]
    fn test_build_suggest_query_limits() {
        // [S1-30] Defaults reproduce the old fixed 5/5; explicit values win.
        let q = build_suggest_query("iphone", 10, None, None);
        let m = qp_map(&q);
        assert_eq!(m["categoryLimit"], "5");
        assert_eq!(m["brandLimit"], "5");
        assert_eq!(m["limit"], "10");
        let q = build_suggest_query("iphone", 10, Some(3), Some(7));
        let m = qp_map(&q);
        assert_eq!(m["categoryLimit"], "3");
        assert_eq!(m["brandLimit"], "7");
        let q = build_suggest_query("iphone", 0, Some(0), None);
        let m = qp_map(&q);
        assert_eq!(m["limit"], "10", "non-positive limit falls back to 10");
        assert_eq!(m["categoryLimit"], "0");
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_seller_profile() {
        // [S1-28] Seller id from the verified live sample.
        let client = YahooAuctionsClient::new();
        let profile = client
            .get_seller_profile("BZHRJg8RuSfMn2oHCtUiWnmfAwjPw")
            .await
            .unwrap();
        assert!(profile.ok);
        assert!(profile.display_name.is_some());
        let summary = profile.ratings_summary.expect("profile must carry rating");
        assert!(summary.total > 0);
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_store_profile() {
        // [S1-28] Anonymous acceptance of store-profile was unconfirmed.
        // The sample seller may not be a store; a structured API error
        // is also a valid answer (it proves anonymous reachability).
        let client = YahooAuctionsClient::new();
        let result = client
            .get_store_profile("BZHRJg8RuSfMn2oHCtUiWnmfAwjPw")
            .await
            .unwrap();
        println!("store-profile result: {result:?}");
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_statistics_closed() {
        // [S1-27] yne variant of the statistics API.
        let client = YahooAuctionsClient::new();
        let result = client
            .get_statistics_closed(StatisticsParams {
                catalog_id: "926964cf77".into(),
                conditions: vec!["USED40".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        println!("statistics_closed result: {result:?}");
        assert_eq!(result.catalog_id, "926964cf77");
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_statistics_hm1() {
        // [S1-27] catalogId from the verified live detail sample.
        // Also settles the `conditions` joiner question if run with
        // conditions set (see notes-ya.md live checklist).
        let client = YahooAuctionsClient::new();
        let result = client
            .get_statistics(StatisticsParams {
                catalog_id: "926964cf77".into(),
                conditions: vec!["USED40".into()],
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        assert_eq!(result.catalog_id, "926964cf77");
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_new_filters() {
        // [S1-25] New filter keys must not break the search call.
        let client = YahooAuctionsClient::new();
        let result = client
            .search(SearchParams {
                query: "iPhone".into(),
                limit: 5,
                page: 1,
                status: "open".into(),
                prefecture_code: "TOKYO".into(),
                is_new_arrival: Some(true),
                sort: "total_price_asc".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_suggest_with_limits() {
        // [S1-30]
        let client = YahooAuctionsClient::new();
        let result = client
            .suggest_keywords_with_limits("iphone", 10, Some(2), Some(2))
            .await
            .unwrap();
        assert!(result.ok);
        assert!(!result.suggestions.is_empty());
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_closed() {
        let client = YahooAuctionsClient::new();
        let result = client
            .search(SearchParams {
                query: "カメラ".into(),
                limit: 5,
                page: 1,
                status: "closed".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok, "closed search must succeed");
        assert_eq!(result.provider, "yahoo_auctions");
        assert_eq!(result.status, "closed");
        assert_eq!(result.search_mode, "appApi");
        assert!(
            !result.items.is_empty(),
            "closed search returned 0 items — API may be broken"
        );
        assert_eq!(result.count, result.items.len());
        for item in &result.items {
            assert!(!item.auction_id.is_empty(), "auction_id empty");
            assert!(!item.title.is_empty(), "title empty");
            assert!(
                item.url.contains(&item.auction_id),
                "url must contain auction_id"
            );
            if let Some(price) = &item.price {
                assert_eq!(price.currency, "JPY");
                assert!(price.value >= 0);
            }
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_open() {
        let client = YahooAuctionsClient::new();
        let result = client
            .search(SearchParams {
                query: "iPhone".into(),
                limit: 5,
                page: 1,
                status: "open".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok, "open search must succeed");
        assert_eq!(result.provider, "yahoo_auctions");
        assert_eq!(result.status, "open");
        assert!(
            !result.items.is_empty(),
            "open search returned 0 items — API may be broken"
        );
        assert_eq!(
            result.search_mode, "appApi",
            "open search must use the app API"
        );
        assert!(
            result.total_available.unwrap_or(0) > 0,
            "total_available must be present from the API"
        );
        assert!(
            result.items.iter().any(|i| i.image.is_some()),
            "API items must carry images"
        );
        assert!(
            result.items.iter().any(|i| i.seller.is_some()),
            "API items must carry sellers"
        );
        for item in &result.items {
            assert!(!item.auction_id.is_empty(), "auction_id empty");
            assert!(!item.title.is_empty(), "title empty");
            assert!(
                item.url.contains("auctions.yahoo.co.jp"),
                "bad url: {}",
                item.url
            );
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_open_multiword() {
        let client = YahooAuctionsClient::new();
        let result = client
            .search(SearchParams {
                query: "NVIDIA GPU".into(),
                limit: 3,
                page: 1,
                status: "open".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        // Multi-word search should work; 0 items only if genuinely no results
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_get_item_from_open() {
        let client = YahooAuctionsClient::new();
        let search = client
            .search(SearchParams {
                query: "カメラ".into(),
                limit: 5,
                page: 1,
                status: "open".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(search.ok);
        assert!(!search.items.is_empty(), "need items to test get_item");
        let mut found_detail = false;
        for candidate in &search.items {
            if let Ok(detail) = client.get_item(&candidate.auction_id).await {
                if detail.ok {
                    assert_eq!(detail.provider, "yahoo_auctions");
                    let item = detail.item.expect("item must be present");
                    assert_eq!(item.auction_id, candidate.auction_id);
                    assert!(!item.title.is_empty());
                    assert!(item.url.contains(&item.auction_id));
                    if let Some(price) = &item.price {
                        assert_eq!(price.currency, "JPY");
                    }
                    found_detail = true;
                    break;
                }
            }
        }
        assert!(
            found_detail,
            "get_item failed for ALL {} search results",
            search.items.len()
        );
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_with_sort() {
        let client = YahooAuctionsClient::new();
        let result = client
            .search(SearchParams {
                query: "レンズ".into(),
                limit: 5,
                page: 1,
                status: "closed".into(),
                sort: "price_asc".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        assert_eq!(result.sort, "price_asc");
        assert!(!result.items.is_empty(), "sorted search returned 0 items");
        // price_asc on the API must actually come back ascending.
        let prices: Vec<i64> = result
            .items
            .iter()
            .filter_map(|i| i.price.as_ref().map(|p| p.value))
            .collect();
        assert!(
            prices.windows(2).all(|w| w[0] <= w[1]),
            "closed price_asc not ascending: {prices:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_suggest_keywords() {
        let client = YahooAuctionsClient::new();
        let result = client.suggest_keywords("iphone", 10).await.unwrap();
        assert!(result.ok);
        assert_eq!(result.provider, "yahoo_auctions");
        assert!(
            !result.suggestions.is_empty(),
            "suggest returned 0 suggestions — API may be broken"
        );
        assert_eq!(result.count, result.suggestions.len());
        for s in &result.suggestions {
            assert!(!s.keyword.is_empty(), "suggestion keyword empty");
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_get_reviews() {
        let client = YahooAuctionsClient::new();
        // First search for an item to get a seller_id
        let search = client
            .search(SearchParams {
                query: "カメラ".into(),
                limit: 5,
                page: 1,
                status: "open".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(search.ok);
        assert!(!search.items.is_empty(), "need items to find a seller");

        // Get item detail to extract seller_id (hash-form aucUserId; the
        // Web rating page accepts it, verified 2026-11-01).
        let mut seller_id = String::new();
        for candidate in &search.items {
            if let Ok(detail) = client.get_item(&candidate.auction_id).await {
                if detail.ok {
                    if let Some(item) = &detail.item {
                        if let Some(seller) = &item.seller {
                            if let Some(uid) = &seller.user_id {
                                if !uid.is_empty() {
                                    seller_id = uid.clone();
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(
            !seller_id.is_empty(),
            "could not find a seller_id from search results"
        );

        let reviews = client.get_reviews(&seller_id, 1, 25).await.unwrap();
        assert!(reviews.ok);
        assert_eq!(reviews.provider, "yahoo_auctions");
        assert_eq!(reviews.seller_id, seller_id);
        assert_eq!(reviews.page, 1);
        // Seller may have reviews; if they do, validate structure
        for review in &reviews.reviews {
            assert!(
                ["very_good", "good", "neutral", "bad", "very_bad", "unknown"]
                    .contains(&review.rating.as_str()),
                "unexpected rating: {}",
                review.rating
            );
            assert!(
                review.role == "seller" || review.role == "buyer",
                "unexpected role: {}",
                review.role
            );
            assert!(!review.reviewer_name.is_empty(), "reviewer_name empty");
            assert!(!review.auction_id.is_empty(), "auction_id empty");
        }
    }
}
