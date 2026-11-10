//! Yahoo! Shopping Japan (jp.co.yahoo.android.yshopping)
//! provider.
//!
//! Reproduces the current app's (16.12.0) native API traffic on
//! `shopping-appliproxy.yahooapis.jp`. Guests need no
//! authorization: the app sends only its built-in `appid`
//! constant. Search is `POST /searchList/v4/items`, detail is
//! `GET /item-detail/app/v2` addressed by the search result's
//! `itemId` (the `{storeId}_{itemCode}` id the app passes as its
//! `itemId` Bundle key).
//!
//! The current API has no review-list endpoint (search results
//! carry rating aggregates only). Reviews are served the way the
//! app itself shows them: `ItemReviewDeepLinkRouter` opens
//! `https://shopping.yahoo.co.jp/review/item/list` in
//! `WebViewActivity`, and that page's server-rendered data block
//! carries the first 20 reviews, so `get_reviews` reads them from
//! there. Further pages and the page's sort orders go through
//! the continuation endpoint the page's own SPA posts to
//! (`/hreviewapi/item-review/get`), carrying the `token` the
//! page itself issued in its data block.
//! The suggest endpoint's parameters are not established, so it
//! is not provided here.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const BASE: &str = "https://shopping-appliproxy.yahooapis.jp";
const APPID: &str = "dj00aiZpPWtHdjlEQk16eVA1ZCZzPWNvbnN1bWVyc2VjcmV0Jng9OGM-";
const APP_VERSION: &str = "16.12.0";
const USER_AGENT: &str = "okhttp/4.12.0";
const IMAGE_BASE: &str = "https://item-shopping.c.yimg.jp/i/k/";
/// The item review page the app opens in its WebView
/// (`ItemReviewDeepLinkRouter` builds this URL from the store id
/// and page key).
const REVIEW_PAGE_BASE: &str = "https://shopping.yahoo.co.jp/review/item/list";
/// A standard Android WebView user agent, used only for the
/// review page (the API calls keep the okhttp agent above). The
/// mobile variant of the page carries the review data.
const WEBVIEW_USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36";
/// The review page's own continuation endpoint: the page's SPA
/// posts here (with the `token` the page itself issued in its
/// data block) when the reader loads more reviews or changes
/// the sort order.
const REVIEW_API_URL: &str = "https://shopping.yahoo.co.jp/hreviewapi/item-review/get";
/// Sort orders for `yshopping_get_reviews` (the review page's
/// own `sortType` values): おすすめ順 (the page's default),
/// 評価の高い順, いいねの多い順, 新着順, and the pick-up
/// selection.
pub const REVIEW_SORTS: &[&str] = &[
    "lengthDesc",
    "ratingDesc",
    "referenceCountDesc",
    "postedTimeDesc",
    "pickUp",
];
/// Page size the SPA requests from the continuation endpoint.
const REVIEW_API_PAGE_SIZE: i32 = 10;

/// Sort orders for `yshopping_search_products` (the app's
/// `SearchResultV2Options$Order$Sort` enum names, sent verbatim).
pub const ORDERS: &[&str] = &[
    "Recommend",
    "Price",
    "TotalPrice",
    "ReviewCount",
    "ReviewScoreAverage",
    "DiscountRate",
    "CatalogReleaseDate",
    "RegistrationDate",
    "GoodDelivery",
];

const MODULE_LIST: &[&str] = &[
    "GenreCategories",
    "Brand",
    "Municipalities",
    "SiblingGenreCategories",
    "SiblingSpecs",
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
pub struct SellerInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Item {
    /// The `{storeId}_{itemCode}` id; pass it to
    /// `yshopping_get_product`.
    pub product_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub title: String,
    pub url: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Raw image ids from the detail response (the API hands out
    /// ids, not URLs, for detail images).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seller: Option<SellerInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_count: Option<i64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_available: Option<i64>,
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
// Search (POST /searchList/v4/items)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SearchParams {
    pub query: String,
    /// Page size (`results`, app default 48).
    pub limit: i32,
    /// 1-based (`startIndex`).
    pub start_index: i32,
    /// One of [`ORDERS`].
    pub order: String,
    pub asc: bool,
    pub price_from: Option<i64>,
    pub price_to: Option<i64>,
    pub discounted: Option<bool>,
    pub discount_from: Option<i64>,
    pub discount_rate_from: Option<i64>,
    pub category_ids: Vec<String>,
    pub brand_id_list: Vec<String>,
    pub seller_id: String,
    pub seller_rating_from: Option<f64>,
    /// Passed through verbatim (the app's UsedStatus value).
    pub used_status: String,
    pub jan_code: String,
    pub include_sold_out: Option<bool>,
    pub good_delivery: Option<bool>,
    pub asutsuku: Option<bool>,
    pub furusato: Option<bool>,
    pub has_coupon: Option<bool>,
    pub has_premium_price: Option<bool>,
    pub is_verified: Option<bool>,
    /// Prefecture code (`deliveryArea`, app default "13").
    pub delivery_area: String,
    pub shipping_service_list: Vec<String>,
    pub conditional_free_shipping_price_to: Option<i64>,
    pub payment_list: Vec<String>,
    pub kspec_id_list: Vec<String>,
    pub sp_codes: Vec<String>,
    pub municipality_code: String,
    pub seller_event_group_id: String,
    pub campaign_tab_id: String,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 48,
            start_index: 1,
            order: "Recommend".into(),
            asc: false,
            price_from: None,
            price_to: None,
            discounted: None,
            discount_from: None,
            discount_rate_from: None,
            category_ids: Vec::new(),
            brand_id_list: Vec::new(),
            seller_id: String::new(),
            seller_rating_from: None,
            used_status: String::new(),
            jan_code: String::new(),
            include_sold_out: None,
            good_delivery: None,
            asutsuku: None,
            furusato: None,
            has_coupon: None,
            has_premium_price: None,
            is_verified: None,
            delivery_area: "13".into(),
            shipping_service_list: Vec::new(),
            conditional_free_shipping_price_to: None,
            payment_list: Vec::new(),
            kspec_id_list: Vec::new(),
            sp_codes: Vec::new(),
            municipality_code: String::new(),
            seller_event_group_id: String::new(),
            campaign_tab_id: String::new(),
        }
    }
}

/// Build the search request body. The core keys are the ones the
/// app's builder (`SearchResultV2Options.a`) always emits for a
/// guest search; optional filter keys are added when set. Pure;
/// unit-tested.
pub fn build_search_body(params: &SearchParams) -> Value {
    let mut b = Map::new();
    b.insert("referer".into(), json!("shopping_search"));
    b.insert("mallSellerType".into(), json!("All"));
    b.insert("mallDisplayType".into(), json!("Shopping"));
    b.insert("query".into(), json!(params.query));
    b.insert("results".into(), json!(params.limit.max(1)));
    b.insert("startIndex".into(), json!(params.start_index.max(1)));
    b.insert("moduleList".into(), json!(MODULE_LIST));
    b.insert("ranking".into(), json!("shopping_search"));
    b.insert(
        "order".into(),
        json!(if params.order.is_empty() {
            "Recommend"
        } else {
            params.order.as_str()
        }),
    );
    b.insert("asc".into(), json!(params.asc));
    b.insert(
        "deliveryArea".into(),
        json!(if params.delivery_area.is_empty() {
            "13"
        } else {
            params.delivery_area.as_str()
        }),
    );
    b.insert("stampRallyRankCode".into(), json!("2000"));
    if let Some(v) = params.price_from {
        b.insert("priceFrom".into(), json!(v));
    }
    if let Some(v) = params.price_to {
        b.insert("priceTo".into(), json!(v));
    }
    if let Some(v) = params.discounted {
        b.insert("discounted".into(), json!(v));
    }
    if let Some(v) = params.discount_from {
        b.insert("discountFrom".into(), json!(v));
    }
    if let Some(v) = params.discount_rate_from {
        b.insert("discountRateFrom".into(), json!(v));
    }
    if !params.category_ids.is_empty() {
        b.insert("categoryIds".into(), json!(params.category_ids));
    }
    if !params.brand_id_list.is_empty() {
        b.insert("brandIdList".into(), json!(params.brand_id_list));
    }
    if !params.seller_id.is_empty() {
        b.insert("sellerId".into(), json!(params.seller_id));
    }
    if let Some(v) = params.seller_rating_from {
        b.insert("sellerRatingFrom".into(), json!(v.to_string()));
    }
    if !params.used_status.is_empty() {
        b.insert("usedStatus".into(), json!(params.used_status));
    }
    if !params.jan_code.is_empty() {
        b.insert("janCode".into(), json!(params.jan_code));
    }
    if let Some(v) = params.include_sold_out {
        b.insert("includeSoldOut".into(), json!(v));
    }
    if let Some(v) = params.good_delivery {
        b.insert("goodDelivery".into(), json!(v));
    }
    if let Some(v) = params.asutsuku {
        b.insert("asutsuku".into(), json!(v));
    }
    if let Some(v) = params.furusato {
        b.insert("furusato".into(), json!(v));
    }
    if let Some(v) = params.has_coupon {
        b.insert("hasCoupon".into(), json!(v));
    }
    if let Some(v) = params.has_premium_price {
        b.insert("hasPremiumPrice".into(), json!(v));
    }
    if let Some(v) = params.is_verified {
        b.insert("isVerified".into(), json!(v));
    }
    if !params.shipping_service_list.is_empty() {
        b.insert(
            "shippingServiceList".into(),
            json!(params.shipping_service_list),
        );
    }
    if let Some(v) = params.conditional_free_shipping_price_to {
        b.insert("conditionalFreeShippingPriceTo".into(), json!(v));
    }
    if !params.payment_list.is_empty() {
        b.insert("paymentList".into(), json!(params.payment_list));
    }
    if !params.kspec_id_list.is_empty() {
        b.insert("kspecIdList".into(), json!(params.kspec_id_list));
    }
    if !params.sp_codes.is_empty() {
        b.insert("spCodes".into(), json!(params.sp_codes));
    }
    if !params.municipality_code.is_empty() {
        b.insert("municipalityCode".into(), json!(params.municipality_code));
    }
    if !params.seller_event_group_id.is_empty() {
        b.insert(
            "sellerEventGroupId".into(),
            json!(params.seller_event_group_id),
        );
    }
    if !params.campaign_tab_id.is_empty() {
        b.insert("campaignTabId".into(), json!(params.campaign_tab_id));
    }
    Value::Object(b)
}

/// Parse a search response body. Pure; unit-tested against the
/// captured live response.
pub fn parse_search_response(body: &Value) -> Vec<Item> {
    let results = body
        .pointer("/result/searchResult/results")
        .and_then(Value::as_array);
    let mut items = Vec::new();
    if let Some(results) = results {
        for it in results {
            let item_id = it["itemId"].as_str().unwrap_or_default().to_string();
            let image_id = it["imageId"].as_str().unwrap_or_default();
            items.push(Item {
                product_id: item_id.clone(),
                item_id: Some(item_id),
                title: it["name"].as_str().unwrap_or_default().to_string(),
                url: it["url"].as_str().unwrap_or_default().to_string(),
                source: "yahoo_shopping".into(),
                price: it
                    .pointer("/price/displayPrice")
                    .and_then(Value::as_i64)
                    .map(Price::yen),
                availability: match it["inStock"].as_bool() {
                    Some(true) => Some("in_stock".into()),
                    Some(false) => Some("sold_out".into()),
                    None => None,
                },
                image: if image_id.is_empty() {
                    None
                } else {
                    Some(format!("{IMAGE_BASE}{image_id}"))
                },
                seller: Some(SellerInfo {
                    id: it
                        .pointer("/store/id")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    name: it
                        .pointer("/store/name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    url: it
                        .pointer("/store/url")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                }),
                review_score: it.pointer("/review/rating").and_then(Value::as_f64),
                review_count: it.pointer("/review/rateCount").and_then(Value::as_i64),
                ..Default::default()
            });
        }
    }
    items
}

// ---------------------------------------------------------------------------
// Detail (GET /item-detail/app/v2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DetailParams {
    /// `{storeId}_{itemCode}` — the search result's `itemId`.
    pub item_id: String,
    pub prefectures: String,
    pub sku_id: String,
    pub donation_id: String,
}

/// Build the detail query parameters (without appid /
/// android_version, which the client adds). Pure; unit-tested.
pub fn build_detail_query(params: &DetailParams) -> Vec<(String, String)> {
    let mut q = vec![
        ("itemId".to_string(), params.item_id.clone()),
        ("isSoftBankCarrier".to_string(), "false".to_string()),
        ("giftCard".to_string(), "true".to_string()),
        ("isTablet".to_string(), "false".to_string()),
        ("usePtahSsr".to_string(), "true".to_string()),
    ];
    if !params.prefectures.is_empty() {
        q.push(("prefectures".to_string(), params.prefectures.clone()));
    }
    if !params.sku_id.is_empty() {
        q.push(("skuId".to_string(), params.sku_id.clone()));
    }
    if !params.donation_id.is_empty() {
        q.push(("donationId".to_string(), params.donation_id.clone()));
    }
    q
}

/// Parse a detail response body. Pure; unit-tested against the
/// captured live response.
pub fn parse_detail_response(body: &Value) -> ItemResult {
    let item = body.pointer("/result/item");
    let Some(item) = item.filter(|i| i.is_object()) else {
        return ItemResult {
            provider: "yahoo_shopping".into(),
            ok: false,
            item: None,
            error: Some("detail response carried no result.item".into()),
        };
    };
    let ysr_id = item["ysrId"].as_str().unwrap_or_default().to_string();
    let price = item
        .pointer("/price/applicablePrice")
        .and_then(Value::as_i64)
        .or_else(|| item.pointer("/price/price1/price").and_then(Value::as_i64))
        .or_else(|| item.pointer("/price/regularPrice").and_then(Value::as_i64));
    let mut image_ids: Vec<String> = Vec::new();
    if let Some(id) = item.pointer("/images/mainImage/id").and_then(Value::as_str) {
        image_ids.push(id.to_string());
    }
    if let Some(list) = item
        .pointer("/images/detailImageList")
        .and_then(Value::as_array)
    {
        for e in list {
            if let Some(id) = e["id"].as_str() {
                image_ids.push(id.to_string());
            }
        }
    }
    let seller = body
        .pointer("/result/seller")
        .cloned()
        .unwrap_or(Value::Null);
    ItemResult {
        provider: "yahoo_shopping".into(),
        ok: true,
        item: Some(Item {
            product_id: ysr_id.clone(),
            item_id: item["srId"].as_str().map(str::to_string),
            title: item["name"].as_str().unwrap_or_default().to_string(),
            url: item["url"].as_str().unwrap_or_default().to_string(),
            source: "yahoo_shopping".into(),
            price: price.map(Price::yen),
            availability: match item.pointer("/stock/isAvailable").and_then(Value::as_bool) {
                Some(true) => Some("in_stock".into()),
                Some(false) => Some("sold_out".into()),
                None => None,
            },
            description: item["caption"].as_str().map(str::to_string),
            image: if ysr_id.is_empty() {
                None
            } else {
                Some(format!("{IMAGE_BASE}{ysr_id}"))
            },
            image_ids: if image_ids.is_empty() {
                None
            } else {
                Some(image_ids)
            },
            seller: Some(SellerInfo {
                id: seller["sellerId"].as_str().map(str::to_string),
                name: seller["name"].as_str().map(str::to_string),
                url: None,
            }),
            review_score: item
                .pointer("/review/reviewSummary/average")
                .and_then(Value::as_f64),
            review_count: item
                .pointer("/review/reviewSummary/count")
                .and_then(Value::as_i64),
        }),
        error: None,
    }
}

/// Extract the `{storeId}_{itemCode}` id from a product URL or
/// pass an id through.
pub fn extract_item_id(product: &str) -> String {
    let s = product.trim();
    if s.contains("store.shopping.yahoo.co.jp") {
        // https://store.shopping.yahoo.co.jp/<store>/<code>.html
        // addresses the item as "{store}_{code}".
        let tail = s.trim_end_matches(".html").trim_end_matches('/');
        let mut parts = tail.rsplit('/');
        let code = parts.next().unwrap_or_default();
        let store = parts.next().unwrap_or_default();
        if !store.is_empty() && !code.is_empty() {
            return format!("{store}_{code}");
        }
        return code.to_string();
    }
    s.to_string()
}

// ---------------------------------------------------------------------------
// Reviews (the app's WebView review page)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReviewParams {
    /// `{storeId}_{itemCode}` — the search result's `itemId`.
    pub item_id: String,
    /// Cap on returned reviews.
    pub limit: i32,
    /// Zero-based index of the first review to return.
    pub offset: i32,
    /// One of [`REVIEW_SORTS`]; empty means the page's default
    /// (`lengthDesc`, おすすめ順).
    pub sort: String,
    /// Star-rating filter (the continuation endpoint's
    /// `rating` body key). `None` leaves it unset, as the page
    /// does until the reader picks a filter.
    pub rating: Option<i32>,
    /// Aspect filter: an id from the item's
    /// `subReviewSummaries` (the endpoint's `subReviewId` key).
    /// `None` leaves it unset.
    pub sub_review_id: Option<i64>,
}

impl Default for ReviewParams {
    fn default() -> Self {
        Self {
            item_id: String::new(),
            limit: 20,
            offset: 0,
            sort: String::new(),
            rating: None,
            sub_review_id: None,
        }
    }
}

/// The review page's optional review filters, as its
/// continuation endpoint takes them: `rating` is a star
/// rating, `sub_review_id` an aspect id from the item's
/// `subReviewSummaries`. Both stay unset until the reader
/// picks them.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewFilters {
    pub rating: Option<i32>,
    pub sub_review_id: Option<i64>,
}

/// Resolve a requested review sort to the `sortType` value the
/// review page sends. Empty selects the page's default
/// (`lengthDesc`). Returns `None` for unknown values. Pure;
/// unit-tested.
pub fn normalize_review_sort(sort: &str) -> Option<&'static str> {
    match sort.trim() {
        "" | "lengthDesc" => Some("lengthDesc"),
        "ratingDesc" => Some("ratingDesc"),
        "referenceCountDesc" => Some("referenceCountDesc"),
        "postedTimeDesc" => Some("postedTimeDesc"),
        "pickUp" => Some("pickUp"),
        _ => None,
    }
}

/// Build the body the review page's SPA posts to its
/// continuation endpoint when loading more reviews. `token` is
/// the value the page itself issued in its data block. The SPA
/// sends `rating` / `subReviewId` only once the reader picks
/// those filters, so they are included only when set. Pure;
/// unit-tested.
pub fn build_review_api_body(
    store_id: &str,
    page_key: &str,
    sort_type: &str,
    offset: i32,
    limit: i32,
    token: &str,
    filters: ReviewFilters,
) -> Value {
    let mut body = json!({
        "storeId": store_id,
        "pageKey": page_key,
        "sortType": sort_type,
        "offset": offset,
        "limit": limit,
        "token": token,
    });
    if let Some(rating) = filters.rating {
        body["rating"] = json!(rating);
    }
    if let Some(sub_review_id) = filters.sub_review_id {
        body["subReviewId"] = json!(sub_review_id);
    }
    body
}

/// The `itemReview` object of a continuation endpoint response
/// (`{itemReview, error}`), when the call succeeded. Pure;
/// unit-tested.
pub fn parse_review_api_item_review(body: &Value) -> Option<&Value> {
    if !body["error"].is_null() {
        return None;
    }
    body.get("itemReview").filter(|v| v.is_object())
}

/// Build the review page URL from a `{storeId}_{itemCode}` id
/// (split at the first `_`, the same split the app applies to
/// ysrId). Returns `None` when either side is empty. Pure;
/// unit-tested.
pub fn build_review_url(item_id: &str) -> Option<String> {
    let (store_id, page_key) = item_id.split_once('_')?;
    if store_id.is_empty() || page_key.is_empty() {
        return None;
    }
    Some(format!(
        "{REVIEW_PAGE_BASE}?store_id={store_id}&page_key={page_key}"
    ))
}

/// Format an epoch-milliseconds timestamp as a `YYYY-MM-DD` date
/// in JST (UTC+9, no DST). Pure; unit-tested.
fn jst_date(epoch_ms: i64) -> String {
    let days = (epoch_ms + 9 * 3600 * 1000).div_euclid(86_400_000);
    // Civil-from-days algorithm (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Extract the JSON of the page's `<script id="__NEXT_DATA__">`
/// block. The review page is server-side rendered, so the data
/// is present in the static HTML. Pure; unit-tested.
pub fn extract_next_data(html: &str) -> Option<Value> {
    let marker = html.find("id=\"__NEXT_DATA__\"")?;
    let start = html[marker..].find('>')? + marker + 1;
    let end = html[start..].find("</script>")? + start;
    serde_json::from_str(&html[start..end]).ok()
}

fn normalize_review(r: &Value) -> Value {
    let posted_time = r["postedTime"].as_i64().unwrap_or(0);
    let variant: Vec<String> = r["subCodeOptions"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|o| o["value"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let aspect_ratings: Vec<Value> = r["subReviews"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    json!({
                        "name": s["name"].as_str().unwrap_or_default(),
                        "value": s["valueName"].as_str().unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let images: Vec<String> = r["images"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|i| i["url"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "id": r["id"].as_str().unwrap_or_default(),
        "author": r["maskYid"].as_str().unwrap_or_default(),
        "rating": r["rating"].as_i64().unwrap_or(0),
        "title": r["title"].as_str().unwrap_or_default(),
        "body": r["body"].as_str().unwrap_or_default(),
        "posted_time": posted_time,
        "posted_date": jst_date(posted_time),
        "helpful_count": r["referenceCount"].as_i64().unwrap_or(0),
        "edited": r["edited"].as_bool().unwrap_or(false),
        "variant": variant,
        "aspect_ratings": aspect_ratings,
        "images": images,
        "store_reply": r["storeReply"].clone(),
    })
}

/// Assemble the tool's result JSON from an `itemReview` object
/// (from the page's data block or the continuation endpoint)
/// and the normalized reviews selected from it. `next_offset`
/// is the index the next call should start from, derived from
/// how many reviews were consumed. Pure; unit-tested.
fn build_reviews_result(
    item_id: &str,
    url: &str,
    item_review: &Value,
    reviews: Vec<Value>,
    offset: i32,
) -> Value {
    let summary = &item_review["reviewSummary"];
    let filtered_count = item_review["filteredCount"].as_i64();
    let consumed = offset as i64 + reviews.len() as i64;
    let available = filtered_count.or(summary["count"].as_i64()).unwrap_or(0);
    let next_offset = if consumed < available {
        Some(consumed)
    } else {
        item_review["nextOffset"].as_i64()
    };
    // Aspect (sub-review) summaries from the same itemReview
    // object (`itemReview.subReviewSummaries`, NOT under
    // reviewSummary): the ids a caller passes back as
    // sub_review_id, with their names and per-value counts.
    let sub_review_summaries: Vec<Value> = item_review["subReviewSummaries"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    let values: Vec<Value> = s["valueSummaries"]
                        .as_array()
                        .map(|vs| {
                            vs.iter()
                                .map(|v| {
                                    json!({
                                        "id": v["id"],
                                        "name": v["name"],
                                        "count": v["count"],
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({
                        "id": s["id"],
                        "name": s["name"],
                        "values": values,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "provider": "yahoo_shopping",
        "ok": true,
        "item_id": item_id,
        "url": url,
        "subReviewSummaries": sub_review_summaries,
        "total": summary["count"].as_i64(),
        "average_rating": summary["average"].as_f64(),
        "rating_counts": {
            "1": summary["one"].as_i64().unwrap_or(0),
            "2": summary["two"].as_i64().unwrap_or(0),
            "3": summary["three"].as_i64().unwrap_or(0),
            "4": summary["four"].as_i64().unwrap_or(0),
            "5": summary["five"].as_i64().unwrap_or(0),
        },
        "reviews": reviews,
        "count": reviews.len(),
        "offset": offset,
        "filtered_count": filtered_count,
        "next_offset": next_offset,
    })
}

/// Build the suggest query parameters (round 7 of the analysis:
/// `Api.GET_SUGGEST`, statically determined and live-verified).
/// `detailed` is the app's shouldReturnCategoryAndStoreSuggest
/// flag and drives both `cate` and `da`. `src` is the search
/// screen's fixed `shp` (the `pmall_zozo` dictionary context is
/// out of scope). appid / android_version are added by the caller
/// the same way as the other endpoints.
pub fn build_suggest_query(keyword: &str, limit: i32, detailed: bool) -> Vec<(String, String)> {
    let flag = if detailed { "1" } else { "0" };
    vec![
        ("p".to_string(), keyword.to_string()),
        ("src".to_string(), "shp".to_string()),
        ("cate".to_string(), flag.to_string()),
        ("da".to_string(), flag.to_string()),
        ("format".to_string(), "json".to_string()),
        ("n".to_string(), limit.clamp(1, 20).to_string()),
    ]
}

/// Parse a suggest response (`{Result: {keyword, category,
/// direct, ...}}`) the way the app's SuggestInfoMapper does:
/// keywords as strings, categories with their ids, directs with
/// the store id taken from the URL's last path segment. Pure;
/// unit-tested against the live-captured shape.
pub fn parse_suggest_response(body: &Value, keyword: &str) -> Value {
    let result = &body["Result"];
    let keywords: Vec<Value> = result["keyword"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|k| k.as_str().map(|s| json!(s)))
                .collect()
        })
        .unwrap_or_default();
    let categories: Vec<Value> = result["category"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| {
                    json!({
                        "keyword": c["suggest"],
                        "category_id": c["cid"],
                        "category": c["cat"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let directs: Vec<Value> = result["direct"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|d| {
                    let url = d["url"].as_str().unwrap_or_default();
                    let store_id = url
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .unwrap_or_default();
                    json!({
                        "title": d["title"],
                        "url": d["url"],
                        "image": d["image"],
                        "store_id": store_id,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "provider": "yahoo_shopping",
        "ok": true,
        "query": keyword,
        "keywords": keywords,
        "categories": categories,
        "directs": directs,
    })
}

/// Parse a review page's HTML into the tool's result JSON. Pure;
/// unit-tested against a captured live page.
pub fn parse_reviews_page(html: &str, item_id: &str, limit: i32) -> Value {
    let url = build_review_url(item_id).unwrap_or_default();
    let fail = |error: &str| {
        json!({
            "provider": "yahoo_shopping",
            "ok": false,
            "item_id": item_id,
            "url": url,
            "error": error,
        })
    };
    let Some(data) = extract_next_data(html) else {
        return fail("review page carried no __NEXT_DATA__ block");
    };
    let item_review = data.pointer("/props/pageProps/reviewEntity/itemReview");
    let Some(item_review) = item_review.filter(|v| v.is_object()) else {
        return fail("review page carried no reviewEntity.itemReview");
    };
    let reviews: Vec<Value> = item_review["reviews"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .take(limit.clamp(1, 100) as usize)
                .map(normalize_review)
                .collect()
        })
        .unwrap_or_default();
    build_reviews_result(item_id, &url, item_review, reviews, 0)
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct YahooShoppingClient {
    client: Client,
    /// Client for the review page and its continuation
    /// endpoint. Unlike the API client it keeps cookies: the
    /// page's SPA runs in a browser that carries the cookies
    /// the page load set into its continuation calls.
    web_client: Client,
}

impl YahooShoppingClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            web_client: Client::builder()
                .user_agent(WEBVIEW_USER_AGENT)
                .cookie_store(true)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    pub async fn search_products(&self, params: SearchParams) -> Result<SearchResult> {
        let body = build_search_body(&params);
        let resp = self
            .client
            .post(format!("{BASE}/searchList/v4/items"))
            .query(&[("appid", APPID), ("android_version", APP_VERSION)])
            .header("X-Referer", "shopping_search")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("yahoo shopping search returned HTTP {status}"));
        }
        let parsed: Value = resp.json().await?;
        let items = parse_search_response(&parsed);
        Ok(SearchResult {
            provider: "yahoo_shopping".into(),
            ok: true,
            query: params.query.clone(),
            page: params.start_index,
            limit: params.limit,
            sort: params.order.clone(),
            count: items.len(),
            items,
            total_available: None,
        })
    }

    /// Keyword suggestions (`GET_SUGGEST` on the suggest host;
    /// statically determined and live-verified, analysis round 7).
    /// Anonymous: the app attaches `Authorization: Bearer ` with an
    /// empty token for guests, and so does this client.
    pub async fn suggest_keywords(
        &self,
        keyword: &str,
        limit: i32,
        detailed: bool,
    ) -> Result<Value> {
        let mut query = vec![
            ("appid".to_string(), APPID.to_string()),
            ("android_version".to_string(), APP_VERSION.to_string()),
        ];
        query.extend(build_suggest_query(keyword, limit, detailed));
        let resp = self
            .client
            .get("https://suggest-shop.yahooapis.jp/Shopping/Suggest/V3/suggester")
            .query(&query)
            .header("Authorization", "Bearer ")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("yahoo shopping suggest returned HTTP {status}"));
        }
        let parsed: Value = resp.json().await?;
        Ok(parse_suggest_response(&parsed, keyword))
    }

    pub async fn get_product(&self, params: DetailParams) -> Result<ItemResult> {
        let mut query = vec![
            ("appid".to_string(), APPID.to_string()),
            ("android_version".to_string(), APP_VERSION.to_string()),
        ];
        query.extend(build_detail_query(&params));
        let resp = self
            .client
            .get(format!("{BASE}/item-detail/app/v2"))
            .query(&query)
            .header("X-Referer", "item")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("yahoo shopping detail returned HTTP {status}"));
        }
        let parsed: Value = resp.json().await?;
        Ok(parse_detail_response(&parsed))
    }

    /// Fetch the review page the app opens in its WebView and
    /// return its URL and parsed `__NEXT_DATA__` block.
    async fn fetch_review_page_data(&self, item_id: &str) -> Result<(String, Value)> {
        let url = build_review_url(item_id)
            .ok_or_else(|| anyhow!("invalid item id for reviews: {item_id}"))?;
        let resp = self.web_client.get(&url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("yahoo shopping review page returned HTTP {status}"));
        }
        let html = resp.text().await?;
        let data = extract_next_data(&html)
            .ok_or_else(|| anyhow!("review page carried no __NEXT_DATA__ block"))?;
        Ok((url, data))
    }

    /// One call to the review page's continuation endpoint,
    /// made the way the page's SPA makes it: the SPA's body keys
    /// and its page size of 10, carrying the page-issued
    /// `token`, as a same-origin browser POST. Returns the raw
    /// response body (`{itemReview, error}`).
    async fn fetch_reviews_api(
        &self,
        store_id: &str,
        page_key: &str,
        sort_type: &str,
        offset: i32,
        token: &str,
        filters: ReviewFilters,
    ) -> Result<Value> {
        let body = build_review_api_body(
            store_id,
            page_key,
            sort_type,
            offset,
            REVIEW_API_PAGE_SIZE,
            token,
            filters,
        );
        // The SPA's call is a same-origin browser POST: it
        // carries the page load's cookies (kept by web_client),
        // the page as Referer, and the site as Origin.
        let referer = build_review_url(&format!("{store_id}_{page_key}"))
            .unwrap_or_else(|| REVIEW_PAGE_BASE.to_string());
        let resp = self
            .web_client
            .post(REVIEW_API_URL)
            .header("Referer", referer)
            .header("Origin", "https://shopping.yahoo.co.jp")
            .header("Accept", "application/json, text/plain, */*")
            .json(&body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("yahoo shopping review api returned HTTP {status}"));
        }
        Ok(resp.json().await?)
    }

    /// Fetch item reviews from the review page the app opens in
    /// its WebView (the native API has no review-list endpoint).
    /// The first 20 reviews ride along with the page itself;
    /// anything beyond them, any sort order other than the
    /// page's default, and any rating / aspect filter goes
    /// through the page's own continuation endpoint with the
    /// token the page issued.
    pub async fn get_reviews(&self, params: ReviewParams) -> Result<Value> {
        let item_id = extract_item_id(&params.item_id);
        let Some(sort_type) = normalize_review_sort(&params.sort) else {
            return Ok(json!({
                "provider": "yahoo_shopping",
                "ok": false,
                "item_id": item_id,
                "error": format!(
                    "unknown review sort: {} (expected one of {})",
                    params.sort,
                    REVIEW_SORTS.join(", ")
                ),
            }));
        };
        let offset = params.offset.max(0);
        let limit = params.limit.clamp(1, 100);
        let (url, data) = self.fetch_review_page_data(&item_id).await?;
        let fail = |error: &str| {
            json!({
                "provider": "yahoo_shopping",
                "ok": false,
                "item_id": item_id,
                "url": url,
                "error": error,
            })
        };
        let item_review = data.pointer("/props/pageProps/reviewEntity/itemReview");
        let Some(item_review) = item_review.filter(|v| v.is_object()) else {
            return Ok(fail("review page carried no reviewEntity.itemReview"));
        };
        // Requests the page's own payload covers are served
        // from it directly, as the page itself renders them.
        // The payload is unfiltered, so any filter goes through
        // the continuation endpoint instead.
        if sort_type == "lengthDesc"
            && offset + limit <= 20
            && params.rating.is_none()
            && params.sub_review_id.is_none()
        {
            let reviews: Vec<Value> = item_review["reviews"]
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .skip(offset as usize)
                        .take(limit as usize)
                        .map(normalize_review)
                        .collect()
                })
                .unwrap_or_default();
            return Ok(build_reviews_result(
                &item_id,
                &url,
                item_review,
                reviews,
                offset,
            ));
        }
        let token = data
            .pointer("/props/pageProps/token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("review page carried no token"))?;
        let (store_id, page_key) = item_id
            .split_once('_')
            .ok_or_else(|| anyhow!("invalid item id for reviews: {item_id}"))?;
        let mut collected: Vec<Value> = Vec::new();
        let mut last: Option<Value> = None;
        let mut cur = offset;
        while (collected.len() as i32) < limit {
            let body = self
                .fetch_reviews_api(
                    store_id,
                    page_key,
                    sort_type,
                    cur,
                    token,
                    ReviewFilters {
                        rating: params.rating,
                        sub_review_id: params.sub_review_id,
                    },
                )
                .await?;
            let Some(ir) = parse_review_api_item_review(&body) else {
                return Ok(fail("review api returned an error or no itemReview"));
            };
            let ir = ir.clone();
            let page: Vec<Value> = ir["reviews"]
                .as_array()
                .map(|arr| arr.iter().map(normalize_review).collect())
                .unwrap_or_default();
            let next = ir["nextOffset"].as_i64();
            let filtered = ir["filteredCount"].as_i64().unwrap_or(0);
            last = Some(ir);
            if page.is_empty() {
                break;
            }
            collected.extend(page);
            match next {
                Some(n) if n > cur as i64 && n < filtered => cur = n as i32,
                _ => break,
            }
        }
        collected.truncate(limit as usize);
        let Some(last) = last else {
            return Ok(fail("review api returned no data"));
        };
        Ok(build_reviews_result(
            &item_id, &url, &last, collected, offset,
        ))
    }
}

impl Default for YahooShoppingClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "search" => include_str!("../tests/fixtures/search.json"),
            "detail" => include_str!("../tests/fixtures/detail.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn search_body_core_keys() {
        let body = build_search_body(&SearchParams {
            query: "コーヒー豆".into(),
            order: "Price".into(),
            asc: true,
            price_from: Some(1000),
            price_to: Some(5000),
            ..Default::default()
        });
        assert_eq!(body["referer"], json!("shopping_search"));
        assert_eq!(body["mallSellerType"], json!("All"));
        assert_eq!(body["mallDisplayType"], json!("Shopping"));
        assert_eq!(body["query"], json!("コーヒー豆"));
        assert_eq!(body["results"], json!(48));
        assert_eq!(body["startIndex"], json!(1));
        assert_eq!(body["order"], json!("Price"));
        assert_eq!(body["asc"], json!(true));
        assert_eq!(body["priceFrom"], json!(1000));
        assert_eq!(body["priceTo"], json!(5000));
        assert_eq!(body["deliveryArea"], json!("13"));
        assert_eq!(body["stampRallyRankCode"], json!("2000"));
        assert_eq!(
            body["moduleList"],
            json!([
                "GenreCategories",
                "Brand",
                "Municipalities",
                "SiblingGenreCategories",
                "SiblingSpecs"
            ])
        );
        assert!(body.get("janCode").is_none());
    }

    #[test]
    fn parse_search_fixture() {
        let items = parse_search_response(&fixture("search"));
        assert_eq!(items.len(), 48);
        let first = &items[0];
        assert_eq!(first.product_id, "dripcoffee_house-500-4");
        assert_eq!(first.price.as_ref().unwrap().value, 3980);
        assert_eq!(
            first.image.as_deref(),
            Some("https://item-shopping.c.yimg.jp/i/k/dripcoffee_house-500-4")
        );
        assert_eq!(
            first.seller.as_ref().unwrap().name.as_deref(),
            Some("DripCoffeeFactory")
        );
        assert_eq!(first.review_count, Some(584));
        assert_eq!(first.review_score, Some(4.5));
    }

    #[test]
    fn detail_query_shape() {
        let q = build_detail_query(&DetailParams {
            item_id: "dripcoffee_house-500-4".into(),
            sku_id: "sku1".into(),
            ..Default::default()
        });
        let map: std::collections::HashMap<_, _> = q.into_iter().collect();
        assert_eq!(map["itemId"], "dripcoffee_house-500-4");
        assert_eq!(map["isSoftBankCarrier"], "false");
        assert_eq!(map["giftCard"], "true");
        assert_eq!(map["isTablet"], "false");
        assert_eq!(map["usePtahSsr"], "true");
        assert_eq!(map["skuId"], "sku1");
        assert!(!map.contains_key("prefectures"));
    }

    #[test]
    fn parse_detail_fixture() {
        let result = parse_detail_response(&fixture("detail"));
        assert!(result.ok, "{result:?}");
        let item = result.item.unwrap();
        assert_eq!(item.product_id, "dripcoffee_house-500-4");
        assert_eq!(item.item_id.as_deref(), Some("house-500-4"));
        assert_eq!(item.price.as_ref().unwrap().value, 3980);
        assert_eq!(item.review_count, Some(584));
        assert_eq!(item.review_score, Some(4.5));
        assert_eq!(
            item.seller.as_ref().unwrap().name.as_deref(),
            Some("DripCoffeeFactory")
        );
        assert!(item.image_ids.as_ref().unwrap().len() >= 3);
    }

    #[test]
    fn extract_item_id_from_url() {
        assert_eq!(
            extract_item_id("https://store.shopping.yahoo.co.jp/dripcoffee/house-500-4.html"),
            "dripcoffee_house-500-4"
        );
        assert_eq!(
            extract_item_id("dripcoffee_house-500-4"),
            "dripcoffee_house-500-4"
        );
    }

    #[test]
    fn review_url_shape() {
        assert_eq!(
            build_review_url("dripcoffee_rrich-500-4").as_deref(),
            Some(
                "https://shopping.yahoo.co.jp/review/item/list?store_id=dripcoffee&page_key=rrich-500-4"
            )
        );
        // The page key itself may contain further underscores and
        // hyphens; only the first `_` separates the store.
        assert_eq!(
            build_review_url("store_a_b-c").as_deref(),
            Some("https://shopping.yahoo.co.jp/review/item/list?store_id=store&page_key=a_b-c")
        );
        assert_eq!(build_review_url("noseparator"), None);
        assert_eq!(build_review_url("_code"), None);
        assert_eq!(build_review_url("store_"), None);
    }

    #[test]
    fn jst_date_shape() {
        assert_eq!(jst_date(1_763_360_207_730), "2025-11-17");
        assert_eq!(jst_date(1_718_369_771_054), "2024-06-14");
        assert_eq!(jst_date(0), "1970-01-01");
    }

    #[test]
    fn parse_reviews_fixture() {
        let html = include_str!("../tests/fixtures/reviews.html");
        let out = parse_reviews_page(html, "dripcoffee_rrich-500-4", 20);
        assert_eq!(out["ok"], json!(true), "{out}");
        assert_eq!(out["total"], json!(7394));
        assert_eq!(out["average_rating"], json!(4.5));
        assert_eq!(out["rating_counts"]["5"], json!(4653));
        assert_eq!(out["rating_counts"]["1"], json!(58));
        assert_eq!(out["next_offset"], json!(20));
        assert_eq!(out["count"], json!(20));
        // subReviewSummaries ride along (ids are the sub_review_id
        // filter values): 29=香り, 137=苦味, 138=酸味 on this item.
        let subs = out["subReviewSummaries"].as_array().unwrap();
        assert_eq!(subs.len(), 3);
        assert_eq!(subs[0]["id"], json!(29));
        assert_eq!(subs[0]["name"], json!("香り"));
        assert_eq!(subs[0]["values"][3]["count"], json!(2673));
        let reviews = out["reviews"].as_array().unwrap();
        let first = &reviews[0];
        assert_eq!(first["author"], json!("meg********"));
        assert_eq!(first["rating"], json!(5));
        assert_eq!(first["title"], json!("少し苦みのあるブラジル系で美味しい"));
        assert!(first["body"].as_str().unwrap().contains("酸味が低い"));
        assert_eq!(first["posted_date"], json!("2025-11-17"));
        assert_eq!(first["helpful_count"], json!(27));
        assert_eq!(first["edited"], json!(true));
        assert_eq!(first["variant"][1], json!("【4571621610115】豆のまま"));
        assert_eq!(first["aspect_ratings"][0]["name"], json!("香り"));
        assert_eq!(first["aspect_ratings"][0]["value"], json!("非常に良い"));
        assert!(first["images"][0]
            .as_str()
            .unwrap()
            .starts_with("https://review-shopping.c.yimg.jp/"));
    }

    #[test]
    fn parse_reviews_limit_and_missing_data() {
        let html = include_str!("../tests/fixtures/reviews.html");
        let out = parse_reviews_page(html, "dripcoffee_rrich-500-4", 3);
        assert_eq!(out["count"], json!(3));
        assert_eq!(out["reviews"].as_array().unwrap().len(), 3);

        let out = parse_reviews_page("<html><body>no data</body></html>", "a_b", 20);
        assert_eq!(out["ok"], json!(false));
        assert!(out["error"].as_str().unwrap().contains("__NEXT_DATA__"));
    }

    #[test]
    fn review_sort_normalization() {
        assert_eq!(normalize_review_sort(""), Some("lengthDesc"));
        assert_eq!(normalize_review_sort("lengthDesc"), Some("lengthDesc"));
        assert_eq!(normalize_review_sort("ratingDesc"), Some("ratingDesc"));
        assert_eq!(
            normalize_review_sort("referenceCountDesc"),
            Some("referenceCountDesc")
        );
        assert_eq!(
            normalize_review_sort("postedTimeDesc"),
            Some("postedTimeDesc")
        );
        assert_eq!(normalize_review_sort("pickUp"), Some("pickUp"));
        assert_eq!(normalize_review_sort("-latest"), None);
        assert_eq!(normalize_review_sort("newest"), None);
    }

    #[test]
    fn review_api_body_shape() {
        // The exact body the page's SPA posts: its own keys, in
        // its own page size, with the page-issued token. The
        // fields the SPA leaves unset (rating, subReviewId,
        // isHtml) are absent, not null.
        let body = build_review_api_body(
            "dripcoffee",
            "rrich-500-4",
            "lengthDesc",
            20,
            10,
            "TOKEN",
            ReviewFilters::default(),
        );
        assert_eq!(body["storeId"], json!("dripcoffee"));
        assert_eq!(body["pageKey"], json!("rrich-500-4"));
        assert_eq!(body["sortType"], json!("lengthDesc"));
        assert_eq!(body["offset"], json!(20));
        assert_eq!(body["limit"], json!(10));
        assert_eq!(body["token"], json!("TOKEN"));
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 6, "{keys:?}");
    }

    #[test]
    fn review_api_body_with_filters() {
        // Once the reader picks the page's filters, the SPA
        // sends them as `rating` / `subReviewId`; both ride
        // along with the base keys.
        let body = build_review_api_body(
            "dripcoffee",
            "rrich-500-4",
            "postedTimeDesc",
            0,
            10,
            "TOKEN",
            ReviewFilters {
                rating: Some(5),
                sub_review_id: Some(29),
            },
        );
        assert_eq!(body["rating"], json!(5));
        assert_eq!(body["subReviewId"], json!(29));
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 8, "{keys:?}");
    }

    #[test]
    fn parse_review_api_page2_fixture() {
        // The continuation endpoint's second page as it came
        // back live (offset 20, the page's default sort): ten
        // reviews continuing the SSR page's twenty, the next
        // window at 30, and the full filtered count.
        let raw = include_str!("../tests/fixtures/reviews-api-page2.json");
        let body: Value = serde_json::from_str(raw).unwrap();
        let ir = parse_review_api_item_review(&body).expect("itemReview");
        assert_eq!(ir["nextOffset"], json!(30));
        assert_eq!(ir["filteredCount"], json!(7394));
        let reviews = ir["reviews"].as_array().unwrap();
        assert_eq!(reviews.len(), 10);
        let first = normalize_review(&reviews[0]);
        assert!(!first["id"].as_str().unwrap().is_empty());
        assert!(!first["author"].as_str().unwrap().is_empty());
        assert!(!first["body"].as_str().unwrap().is_empty());
        assert!(first["rating"].as_i64().unwrap() >= 1);
    }

    #[test]
    fn suggest_query_and_parse() {
        // Round-7 form: cate/da follow the detailed flag together,
        // n is the limit, src is the search screen's fixed shp.
        let q = build_suggest_query("コーヒー", 20, true);
        let map: std::collections::HashMap<_, _> = q.iter().cloned().collect();
        assert_eq!(map["p"], "コーヒー");
        assert_eq!(map["src"], "shp");
        assert_eq!(map["cate"], "1");
        assert_eq!(map["da"], "1");
        assert_eq!(map["format"], "json");
        assert_eq!(map["n"], "20");
        let q2 = build_suggest_query("x", 5, false);
        let map2: std::collections::HashMap<_, _> = q2.iter().cloned().collect();
        assert_eq!(map2["cate"], "0");
        assert_eq!(map2["da"], "0");

        // Live-captured shape (2026-11-08, keyword ニトリ): the
        // direct entry's store id is the URL's last segment.
        let body = json!({
            "Result": {
                "query": "",
                "keyword": ["ニトリ カラーボックス", "ニトリ 収納"],
                "category": [
                    {"suggest": "ニトリ", "cid": 1234, "cat": "家具", "score": null}
                ],
                "direct": [{
                    "url": "https://store.shopping.yahoo.co.jp/nitori-net/",
                    "type": "store",
                    "title": "ニトリ Yahoo!店",
                    "image": "https://shopping.c.yimg.jp/lib/nitori-net/nitori-net_1",
                    "icon": "",
                    "start": "20221024",
                    "end": "20301231"
                }],
                "ranking": []
            }
        });
        let out = parse_suggest_response(&body, "ニトリ");
        assert_eq!(
            out["keywords"],
            json!(["ニトリ カラーボックス", "ニトリ 収納"])
        );
        assert_eq!(out["categories"][0]["category_id"], json!(1234));
        assert_eq!(out["categories"][0]["keyword"], json!("ニトリ"));
        assert_eq!(out["directs"][0]["store_id"], json!("nitori-net"));
        assert_eq!(out["directs"][0]["title"], json!("ニトリ Yahoo!店"));
    }

    #[test]
    fn parse_review_api_response_shapes() {
        // Error responses and malformed bodies yield None.
        assert!(parse_review_api_item_review(&json!({"error": {"code": 500}})).is_none());
        assert!(
            parse_review_api_item_review(&json!({"itemReview": null, "error": null})).is_none()
        );
        assert!(parse_review_api_item_review(
            &json!({"itemReview": {"reviews": []}, "error": null})
        )
        .is_some());
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_reviews_pagination() {
        let client = YahooShoppingClient::new();
        let item_id = "dripcoffee_rrich-500-4";
        // Send 1: the SSR page, for the page-issued token and
        // the first page of reviews (default sort).
        let (_url, data) = client.fetch_review_page_data(item_id).await.unwrap();
        let token = data
            .pointer("/props/pageProps/token")
            .and_then(Value::as_str)
            .expect("page token")
            .to_string();
        let ir0 = data
            .pointer("/props/pageProps/reviewEntity/itemReview")
            .expect("itemReview");
        let page1_ids: Vec<String> = ir0["reviews"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(page1_ids.len(), 20);
        // Send 2: the continuation endpoint at offset 20. This
        // is the second page; it must not repeat page 1 (a new
        // review posted in between can shift the window by a
        // little, so a small overlap is tolerated).
        let body = client
            .fetch_reviews_api(
                "dripcoffee",
                "rrich-500-4",
                "lengthDesc",
                20,
                &token,
                ReviewFilters::default(),
            )
            .await
            .unwrap();
        if let Ok(path) = std::env::var("ZERO_SEARCH_DUMP_REVIEW_API") {
            std::fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
        }
        let ir = parse_review_api_item_review(&body).expect("api itemReview");
        let page2 = ir["reviews"].as_array().unwrap();
        assert_eq!(page2.len(), 10, "{body}");
        assert_eq!(ir["nextOffset"], json!(30));
        let overlap = page2
            .iter()
            .filter(|r| page1_ids.contains(&r["id"].as_str().unwrap().to_string()))
            .count();
        assert!(overlap <= 2, "page 2 overlaps page 1 by {overlap}");
        // Send 3: newest-first must come back time-ordered,
        // which the default order is not.
        let body = client
            .fetch_reviews_api(
                "dripcoffee",
                "rrich-500-4",
                "postedTimeDesc",
                0,
                &token,
                ReviewFilters::default(),
            )
            .await
            .unwrap();
        let ir = parse_review_api_item_review(&body).expect("api itemReview");
        let times: Vec<i64> = ir["reviews"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["postedTime"].as_i64().unwrap())
            .collect();
        assert_eq!(times.len(), 10);
        assert!(
            times.windows(2).all(|w| w[0] >= w[1]),
            "postedTimeDesc not ordered: {times:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_reviews() {
        let client = YahooShoppingClient::new();
        let out = client
            .get_reviews(ReviewParams {
                item_id: "dripcoffee_rrich-500-4".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
        assert!(out["total"].as_i64().unwrap() > 0);
        assert!(!out["reviews"].as_array().unwrap().is_empty());
        assert!(!out["reviews"][0]["body"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search() {
        let client = YahooShoppingClient::new();
        let result = client
            .search_products(SearchParams {
                query: "コーヒー豆".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        assert!(!result.items.is_empty());
        assert!(result.items[0].price.is_some());
    }
}
