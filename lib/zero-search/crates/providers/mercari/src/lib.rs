mod dpop;
mod proto;

use anyhow::Result;
use regex::Regex;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;

use dpop::DpopSigner;

const API_BASE: &str = "https://api.mercari.jp";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

const SEARCH_ENDPOINT: &str = "/v2/entities:search";
const ITEM_GET_ENDPOINT: &str = "/items/get";
const ITEMS_GET_ENDPOINT: &str = "/items/get_items";
const AUTOCOMPLETE_ENDPOINT: &str = "/services/bff/search/v1/autocomplete:search";
const BRANDS_DATASET_ENDPOINT: &str = "/master/v2/datasets/item_brands";
const CATEGORIES_DATASET_ENDPOINT: &str = "/master/v2/datasets/item_categories";
const REVIEWS_ENDPOINT: &str = "/reviews/history";

static SORT_ENUMS: &[&str] = &[
    "SORT_DEFAULT",
    "SORT_CREATED_TIME",
    "SORT_NUM_LIKES",
    "SORT_SCORE",
    "SORT_PRICE",
    "SORT_SIMILARITY",
    "SORT_PRICE_WITHOUT_TIME_FILTER",
];
static ORDER_ENUMS: &[&str] = &["ORDER_DESC", "ORDER_ASC"];
static STATUS_ENUMS: &[&str] = &[
    "STATUS_UNSPECIFIED",
    "STATUS_ON_SALE",
    "STATUS_TRADING",
    "STATUS_SOLD_OUT",
];
#[allow(dead_code)]
static ITEM_TYPE_ENUMS: &[&str] = &["ITEM_TYPE_MERCARI", "ITEM_TYPE_BEYOND"];

fn enum_at<'a>(table: &'a [&'a str], idx: usize) -> &'a str {
    table.get(idx).unwrap_or(&table[0])
}

fn search_sort(s: &str) -> (usize, usize) {
    match s.to_lowercase().as_str() {
        "" | "default" | "recommended" => (0, 0),
        "score" | "relevance" => (3, 0),
        "newest" | "created_desc" => (1, 0),
        "oldest" | "created_asc" => (1, 1),
        "liked" | "likes" | "num_likes" => (2, 0),
        "lowest_price" | "price_asc" => (4, 1),
        "highest_price" | "price_desc" => (4, 0),
        "similarity" => (5, 0),
        // [S1-24] SORT_PRICE_WITHOUT_TIME_FILTER (index 6). Bare name
        // follows the enum default order (ORDER_DESC, index 0); explicit
        // asc/desc spellings select the order.
        "price_without_time_filter" | "sort_price_without_time_filter" => (6, 0),
        "price_asc_without_time_filter" | "lowest_price_without_time_filter" => (6, 1),
        "price_desc_without_time_filter" | "highest_price_without_time_filter" => (6, 0),
        _ => (0, 0),
    }
}

/// Every input spelling accepted by [`search_sort`], for tool docs / wiring.
pub const SEARCH_SORT_SPELLINGS: &[&str] = &[
    "default",
    "recommended",
    "score",
    "relevance",
    "newest",
    "created_desc",
    "oldest",
    "created_asc",
    "liked",
    "likes",
    "num_likes",
    "lowest_price",
    "price_asc",
    "highest_price",
    "price_desc",
    "similarity",
    "price_without_time_filter",
    "sort_price_without_time_filter",
    "price_asc_without_time_filter",
    "lowest_price_without_time_filter",
    "price_desc_without_time_filter",
    "highest_price_without_time_filter",
];

fn search_status_value(s: &str) -> Option<usize> {
    match s.to_lowercase().as_str() {
        "" | "default" | "all" => None,
        "on_sale" | "onsale" | "selling" => Some(1),
        "trading" => Some(2),
        "sold_out" | "sold" => Some(3),
        _ => None,
    }
}

fn item_status_label(s: &str) -> &'static str {
    match s {
        "ITEM_STATUS_ON_SALE" => "on_sale",
        "ITEM_STATUS_TRADING" => "trading",
        "ITEM_STATUS_SOLD_OUT" => "sold_out",
        "ITEM_STATUS_STOP" => "stop",
        "ITEM_STATUS_CANCEL" => "cancel",
        "ITEM_STATUS_ADMIN_CANCEL" => "admin_cancel",
        _ => "unspecified",
    }
}

fn item_type_label(s: &str) -> &'static str {
    match s {
        "ITEM_TYPE_MERCARI" => "mercari",
        "ITEM_TYPE_BEYOND" => "beyond",
        _ => "",
    }
}

const MERCARI_AFID: &str = "6694470352";

fn item_url(id: &str) -> String {
    if id.is_empty() {
        String::new()
    } else {
        format!("https://jp.mercari.com/item/{id}?afid={MERCARI_AFID}")
    }
}

fn to_int(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

fn split_values(v: &str) -> Vec<String> {
    v.split(&[',', ';', ' '][..])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn int_list(v: &str) -> Vec<i64> {
    split_values(v)
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect()
}

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    pub limit: i32,
    pub page_token: String,
    pub sort: String,
    pub status: String,
    pub exclude_keyword: String,
    pub category_ids: String,
    pub brand_ids: String,
    pub size_ids: String,
    pub seller_ids: String,
    pub price_min: Option<i64>,
    pub price_max: Option<i64>,
    pub item_condition_ids: String,
    pub shipping_payer_ids: String,
    pub color_ids: String,
    pub item_types: String,
    pub exclude_auction: bool,
    // [S1-21] Previously hard-coded searchCondition fields, now caller-set.
    /// Sent verbatim as `hasCoupon`.
    pub has_coupon: bool,
    /// Raw JSON attribute filter objects, sent verbatim as `attributes`.
    /// The upstream attribute-object shape is not pinned by any fixture in
    /// this crate, so values pass through untouched instead of being
    /// remodelled (and possibly mangled) here.
    pub attributes: Vec<Value>,
    /// Sent as `skuIds` (numeric id array, like the neighbouring
    /// categoryId / brandId / colorId arrays).
    pub sku_ids: Vec<i64>,
    /// Sent as `shopIds` (numeric id array).
    pub shop_ids: Vec<i64>,
    /// Sent as `excludeShippingMethodIds` (numeric id array).
    pub exclude_shipping_method_ids: Vec<i64>,
    /// Sent as `shippingFromArea` (numeric area / prefecture id array).
    pub shipping_from_area: Vec<i64>,
    /// Sent as `shippingMethod` (numeric shipping-method id array).
    pub shipping_method: Vec<i64>,
}

/// Build the exact wire JSON for `entities:search` from typed params.
/// Pure (no I/O) so the [S1-21] fields can be asserted offline.
pub fn build_search_request(params: &SearchParams, limit: i32, session_id: &str) -> Value {
    let (sort_idx, order_idx) = search_sort(&params.sort);

    let mut status_list = vec![];
    if let Some(sv) = search_status_value(&params.status) {
        status_list.push(enum_at(STATUS_ENUMS, sv));
    }

    let mut item_type_list = vec![];
    for t in split_values(&params.item_types) {
        match t.to_lowercase().as_str() {
            "mercari" | "item" => item_type_list.push("ITEM_TYPE_MERCARI"),
            "beyond" | "shops" | "shop" => item_type_list.push("ITEM_TYPE_BEYOND"),
            _ => {}
        }
    }

    let condition = json!({
        "keyword": params.query,
        "excludeKeyword": params.exclude_keyword,
        "sort": enum_at(SORT_ENUMS, sort_idx),
        "order": enum_at(ORDER_ENUMS, order_idx),
        "status": status_list,
        "sizeId": split_values(&params.size_ids),
        "categoryId": int_list(&params.category_ids),
        "brandId": int_list(&params.brand_ids),
        "sellerId": split_values(&params.seller_ids),
        "priceMin": params.price_min.unwrap_or(0),
        "priceMax": params.price_max.unwrap_or(0),
        "itemConditionId": int_list(&params.item_condition_ids),
        "shippingPayerId": int_list(&params.shipping_payer_ids),
        "colorId": int_list(&params.color_ids),
        "hasCoupon": params.has_coupon,
        "attributes": params.attributes,
        "itemTypes": item_type_list,
        "skuIds": params.sku_ids,
        "shopIds": params.shop_ids,
        "excludeShippingMethodIds": params.exclude_shipping_method_ids,
        "shippingFromArea": params.shipping_from_area,
        "shippingMethod": params.shipping_method,
    });

    json!({
        "userId": "",
        "config": {"responseToggles": []},
        "pageSize": limit,
        "pageToken": params.page_token,
        "searchSessionId": session_id,
        "source": "BaseSerp",
        "searchCondition": condition,
        "serviceFrom": "suruga",
        "withItemBrand": true,
        "withItemSizes": true,
        "withItemPromotions": true,
        "withAuction": true,
        "useDynamicAttribute": true,
    })
}

/// Query params for `items/get_items` [S1-23]. The `offset` argument is
/// retained for signature compatibility but is NEVER sent: live probing
/// (2026-11-08) proved the endpoint ignores both `offset` and `page`
/// (page 2 returned byte-identical data), so no continuation parameter
/// is fabricated here. The endpoint's real continuation mechanism is
/// unidentified; its `meta` carries only `has_next` / `total_item_count`.
pub fn build_seller_items_params(
    seller_id: &str,
    status: &str,
    limit: i32,
    offset: i32,
) -> Vec<(String, String)> {
    let _ = offset;
    vec![
        ("seller_id".to_string(), seller_id.to_string()),
        ("status".to_string(), status.to_string()),
        ("limit".to_string(), limit.to_string()),
        ("with_auction".to_string(), "true".to_string()),
        ("with_total_item_count".to_string(), "true".to_string()),
    ]
}

/// Extract whatever continuation state the seller-items `meta` actually
/// carries. This tolerantly surfaces both spellings (`nextPageToken` /
/// `next_page_token`, `has_next` / `hasNext`, total counts) when present.
/// `next_offset` is always null: offset continuation was disproven live
/// (see [`build_seller_items_params`]), so no usable offset is claimed.
pub fn seller_items_pagination(
    meta: Option<&Value>,
    offset: i32,
    returned: usize,
    limit: i32,
) -> Value {
    let _ = (returned, limit);
    let mut out = json!({
        "offset": offset,
        "next_offset": Value::Null,
    });
    if let Some(meta) = meta.filter(|m| m.is_object()) {
        if let Some(tok) = meta
            .get("nextPageToken")
            .or_else(|| meta.get("next_page_token"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            out["next_page_token"] = json!(tok);
        }
        if let Some(has_next) = meta
            .get("has_next")
            .or_else(|| meta.get("hasNext"))
            .and_then(|v| v.as_bool())
        {
            out["has_next"] = json!(has_next);
        }
        for key in [
            "totalItemCount",
            "total_item_count",
            "numFound",
            "num_found",
            "total",
        ] {
            if let Some(n) = meta.get(key).and_then(to_int) {
                out["total_item_count"] = json!(n);
                break;
            }
        }
    }
    out
}

/// Query params for `reviews/history` [S1-22]. Always the original
/// three params, for every page value: live probing (2026-11-08) proved
/// the endpoint ignores both `offset` and `page` (page 2 returned
/// byte-identical data), so no continuation parameter is fabricated
/// here. `meta` exposes `has_next` and a `requested` value whose role
/// as a cursor is undetermined. [`MercariClient::get_reviews`] rejects
/// page > 1 explicitly instead of silently returning page 1 again.
pub fn build_reviews_params(seller_id: &str, page: i32, limit: i32) -> Vec<(String, String)> {
    let _ = page;
    vec![
        ("user_id".to_string(), seller_id.to_string()),
        ("fame".to_string(), "good,normal,bad".to_string()),
        ("limit".to_string(), limit.to_string()),
    ]
}

/// Normalize a `reviews/history` response body. Pure, for fixture tests.
pub fn normalize_reviews_response(body: &Value, seller_id: &str, page: i32, _limit: i32) -> Value {
    let meta = &body["meta"];
    let reviews: Vec<Value> = body["data"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|r| {
                    if !r.is_object() {
                        return None;
                    }
                    Some(json!({
                        "rating": r["fame"].as_str().unwrap_or("unknown"),
                        "message": r["message"].as_str().unwrap_or(""),
                        "reviewer_name": r["user"]["name"].as_str().unwrap_or(""),
                        "reviewer_id": r["user"]["id"].as_i64()
                            .map(|id| id.to_string())
                            .or_else(|| r["user"]["id"].as_str().map(|s| s.to_string()))
                            .unwrap_or_default(),
                        "role": r["subject"].as_str().unwrap_or(""),
                        "date": to_int(&r["created"]),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let ratings = &meta["ratings"];
    let good = ratings["good"].as_i64().unwrap_or(0);
    let normal = ratings["normal"].as_i64().unwrap_or(0);
    let bad = ratings["bad"].as_i64().unwrap_or(0);
    let count = reviews.len();

    json!({
        "provider": "mercari",
        "ok": true,
        "seller_id": seller_id,
        "page": page.max(1),
        "ratings_summary": {
            "good": good,
            "normal": normal,
            "bad": bad,
            "total": meta["num_ratings"].as_i64().unwrap_or(good + normal + bad),
            "star_rating": meta["star_rating_score"].as_i64().unwrap_or(0),
        },
        "reviews": reviews,
        "has_next": meta["has_next"].as_bool().unwrap_or(false),
        "count": count,
    })
}

pub struct MercariClient {
    client: Client,
    dpop: Arc<Mutex<DpopSigner>>,
}

impl MercariClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .expect("failed to build HTTP client"),
            dpop: Arc::new(Mutex::new(DpopSigner::new())),
        }
    }

    pub async fn search_products(&self, params: SearchParams) -> Result<Value> {
        let limit = params.limit.clamp(1, 120);
        let session_id = uuid::Uuid::new_v4().to_string();
        let request = build_search_request(&params, limit, &session_id);

        let body = self.post_json(SEARCH_ENDPOINT, &request).await?;
        let items = normalize_search_response(&body);
        let items = filter_exclude_auction(items, params.exclude_auction);

        Ok(json!({
            "provider": "mercari",
            "ok": true,
            "query": params.query,
            "limit": limit,
            "sort": params.sort,
            "exclude_auction": params.exclude_auction,
            "search_session_id": session_id,
            "items": items,
            "count": items.len(),
            "next_page_token": body["meta"]["nextPageToken"].as_str().unwrap_or(""),
            "num_found": to_int(&body["meta"]["numFound"]),
        }))
    }

    pub async fn get_product(&self, product: &str) -> Result<Value> {
        let item_id = extract_item_id(product);
        if item_id.is_empty() {
            anyhow::bail!("Could not find a Mercari item id in: {product}");
        }

        let url = format!("{API_BASE}{ITEM_GET_ENDPOINT}");
        let params = [
            ("id", item_id.as_str()),
            ("include_item_attributes", "true"),
            ("include_product_page_component", "true"),
            ("include_non_ui_item_attributes", "true"),
            ("include_auction", "true"),
        ];

        let dpop_proof = {
            let full_url = reqwest::Url::parse_with_params(&url, &params)?;
            let signer = self.dpop.lock().await;
            signer.proof("GET", full_url.as_ref())
        };

        let resp = self
            .client
            .get(&url)
            .query(&params)
            .header("DPoP", dpop_proof)
            .header("X-Platform", "web")
            .header("Accept-Language", "ja-JP")
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?;

        let envelope: Value = resp.json().await?;
        let data = if envelope["data"].is_object() {
            &envelope["data"]
        } else {
            &envelope
        };
        let item = normalize_item_detail(data);

        Ok(json!({
            "provider": "mercari",
            "ok": true,
            "item": item,
        }))
    }

    pub async fn search_seller_products(
        &self,
        seller_id: &str,
        status: &str,
        limit: i32,
    ) -> Result<Value> {
        self.search_seller_products_with_offset(seller_id, status, limit, 0)
            .await
    }

    /// [S1-23] Seller item list. `offset == 0` sends exactly the
    /// parameters the original method sent. `offset > 0` is rejected
    /// explicitly: live probing (2026-11-08) proved `items/get_items`
    /// ignores both `offset` and `page` (page 2 is byte-identical to
    /// page 1), and its `meta` carries no continuation token (only
    /// `has_next` / `total_item_count`), so the real continuation
    /// mechanism is unidentified. The result still surfaces whatever
    /// continuation fields `meta` actually contains
    /// (`next_page_token` / `has_next`, extracted tolerantly by
    /// [`seller_items_pagination`]); `next_offset` is always null.
    pub async fn search_seller_products_with_offset(
        &self,
        seller_id: &str,
        status: &str,
        limit: i32,
        offset: i32,
    ) -> Result<Value> {
        let offset = offset.max(0);
        if offset > 0 {
            anyhow::bail!(
                "mercari seller items offset {offset} is unavailable: items/get_items \
                 ignores offset/page parameters (live-verified); only the first page \
                 can be fetched"
            );
        }
        let url = format!("{API_BASE}{ITEMS_GET_ENDPOINT}");
        let limit = limit.clamp(1, 100);
        let params = build_seller_items_params(seller_id, status, limit, offset);

        let dpop_proof = {
            let full_url = reqwest::Url::parse_with_params(&url, &params)?;
            let signer = self.dpop.lock().await;
            signer.proof("GET", full_url.as_ref())
        };

        let resp = self
            .client
            .get(&url)
            .query(&params)
            .header("DPoP", dpop_proof)
            .header("X-Platform", "web")
            .header("Accept-Language", "ja-JP")
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?;

        let data: Value = resp.json().await?;
        let items: Vec<Value> = data["data"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| {
                        if !v.is_object() {
                            return None;
                        }
                        Some(normalize_item_summary(v))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let count = items.len();
        Ok(json!({
            "provider": "mercari",
            "ok": true,
            "seller_id": seller_id,
            "status": status,
            "items": items,
            "count": count,
            "offset": offset,
            "limit": limit,
            "next_offset": Value::Null,
            "pagination": seller_items_pagination(data.get("meta"), offset, count, limit),
            "meta": data.get("meta"),
        }))
    }

    pub async fn suggest_keywords(&self, keyword: &str) -> Result<Value> {
        let request = json!({
            "keyword": keyword,
            "requestId": uuid::Uuid::new_v4().to_string(),
            "searchSessionId": uuid::Uuid::new_v4().to_string(),
        });
        let body = self.post_json(AUTOCOMPLETE_ENDPOINT, &request).await?;
        let suggestions: Vec<Value> = body["suggestions"]
            .as_array()
            .map(|arr| arr.iter().map(normalize_autocomplete).collect())
            .unwrap_or_default();

        Ok(json!({
            "provider": "mercari",
            "ok": true,
            "keyword": keyword,
            "suggestions": suggestions,
        }))
    }

    pub async fn search_brands(&self, query: &str, limit: i32) -> Result<Value> {
        let data = self.get_json(BRANDS_DATASET_ENDPOINT).await?;
        let rows = data["itemBrands"].as_array();
        let limit = limit.clamp(1, 100) as usize;
        let needle = query.to_lowercase();

        let brands: Vec<Value> = rows
            .map(|arr| {
                arr.iter()
                    .filter(|row| {
                        if needle.is_empty() {
                            return true;
                        }
                        let haystack = format!(
                            "{} {} {} {}",
                            row["name"].as_str().unwrap_or(""),
                            row["subname"].as_str().unwrap_or(""),
                            row["initial"].as_str().unwrap_or(""),
                            row["jaPronunciation"].as_str().unwrap_or(""),
                        )
                        .to_lowercase();
                        haystack.contains(&needle)
                    })
                    .take(limit)
                    .map(|row| {
                        json!({
                            "id": to_int(&row["id"]),
                            "name": row["name"],
                            "official_name": row["subname"],
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(json!({
            "provider": "mercari",
            "ok": true,
            "query": query,
            "brands": brands,
        }))
    }

    pub async fn search_categories(&self, query: &str, limit: i32) -> Result<Value> {
        let data = self.get_json(CATEGORIES_DATASET_ENDPOINT).await?;
        let rows = data["itemCategories"].as_array();
        let limit = limit.clamp(1, 100) as usize;
        let needle = query.to_lowercase();

        let categories: Vec<Value> = rows
            .map(|arr| {
                arr.iter()
                    .filter(|row| {
                        if needle.is_empty() {
                            return true;
                        }
                        let name = row["name"].as_str().unwrap_or("").to_lowercase();
                        name.contains(&needle)
                    })
                    .take(limit)
                    .map(|row| {
                        json!({
                            "id": to_int(&row["id"]),
                            "name": row["name"],
                            "is_leaf": !row["hasChild"].as_bool().unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(json!({
            "provider": "mercari",
            "ok": true,
            "query": query,
            "categories": categories,
        }))
    }

    pub async fn get_reviews(&self, seller_id: &str, page: i32, limit: i32) -> Result<Value> {
        let limit = limit.clamp(1, 100);
        let page = page.max(1);
        if page > 1 {
            // [S1-22] The endpoint ignores `offset` and `page` alike
            // (live-verified 2026-11-08: page 2 is byte-identical to
            // page 1). Fail explicitly rather than return page 1 again
            // labelled as page 2. The real continuation mechanism is
            // unidentified (meta carries has_next and an undetermined
            // `requested` value only).
            anyhow::bail!(
                "mercari reviews page {page} is unavailable: reviews/history ignores \
                 offset/page parameters (live-verified); only page 1 can be fetched"
            );
        }
        let url = format!("{API_BASE}{REVIEWS_ENDPOINT}");
        let params = build_reviews_params(seller_id, page, limit);
        let params_ref: Vec<(&str, &str)> = params
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let dpop_proof = {
            let full_url = reqwest::Url::parse_with_params(&url, &params_ref)?;
            let signer = self.dpop.lock().await;
            signer.proof("GET", full_url.as_ref())
        };

        let resp = self
            .client
            .get(&url)
            .query(&params_ref)
            .header("DPoP", dpop_proof)
            .header("X-Platform", "web")
            .header("Accept-Language", "ja-JP")
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?;

        let body: Value = resp.json().await?;
        Ok(normalize_reviews_response(&body, seller_id, page, limit))
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<Value> {
        let url = format!("{API_BASE}{path}");
        let dpop_proof = {
            let signer = self.dpop.lock().await;
            signer.proof("POST", &url)
        };

        let resp = self
            .client
            .post(&url)
            .header("DPoP", dpop_proof)
            .header("X-Platform", "web")
            .header("Accept-Language", "ja-JP")
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(body)
            .send()
            .await?
            .error_for_status()?;

        Ok(resp.json().await?)
    }

    async fn get_json(&self, path: &str) -> Result<Value> {
        let url = format!("{API_BASE}{path}");
        let dpop_proof = {
            let signer = self.dpop.lock().await;
            signer.proof("GET", &url)
        };

        let resp = self
            .client
            .get(&url)
            .header("DPoP", dpop_proof)
            .header("X-Platform", "web")
            .header("Accept-Language", "ja-JP")
            .header("Accept", "application/json")
            .send()
            .await?
            .error_for_status()?;

        Ok(resp.json().await?)
    }
}

impl Default for MercariClient {
    fn default() -> Self {
        Self::new()
    }
}

pub fn extract_item_id(value: &str) -> String {
    let text = value.trim();
    // m + digits (classic format)
    let re = Regex::new(r"\bm\d{6,}\b").unwrap();
    if let Some(m) = re.find(text) {
        return m.as_str().to_string();
    }
    // bare digits → prepend m
    let digits_re = Regex::new(r"^\d{6,}$").unwrap();
    if digits_re.is_match(text) {
        return format!("m{text}");
    }
    // URL with /item/{hash} path
    if text.contains("mercari.") && text.contains("/item/") {
        if let Some(seg) = text.rsplit("/item/").next() {
            let id = seg.split(&['?', '#', '/'][..]).next().unwrap_or("");
            if id.len() >= 6 {
                return id.to_string();
            }
        }
    }
    // alphanumeric hash ID (new format, 20+ chars)
    let hash_re = Regex::new(r"^[A-Za-z0-9]{20,}$").unwrap();
    if hash_re.is_match(text) {
        return text.to_string();
    }
    String::new()
}

/// Normalize auction data from either upstream shape into one object.
///
/// Search results carry `auction` (camelCase, string values, only populated
/// when the search request sets `withAuction: true`); item detail and seller
/// item lists carry `auction_info` (snake_case). Detail uses
/// `total_bids` + `expected_end_time` (epoch seconds), seller lists use
/// `total_bid` + `bid_deadline` (ISO 8601) — both deadline fields describe
/// the same instant. Returns `None` for non-auction items.
fn normalize_auction(raw: &Value) -> Option<Value> {
    let obj = raw.as_object()?;
    let get = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| obj.get(*k))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let id = get(&["id"]).as_str().unwrap_or_default().to_string();
    Some(json!({
        "id": if id.is_empty() { Value::Null } else { json!(id) },
        "highest_bid": to_int(&get(&["highestBid", "highest_bid"])),
        "initial_price": to_int(&get(&["initialPrice", "initial_price"])),
        "total_bids": to_int(&get(&["totalBid", "total_bids", "total_bid"])),
        "bid_deadline": get(&["bidDeadline", "bid_deadline"]),
        "expected_end_time": to_int(&get(&["expected_end_time"])),
        "start_time": to_int(&get(&["start_time"])),
        "state": get(&["state"]),
        "auction_type": get(&["auction_type"]),
    }))
}

fn filter_exclude_auction(items: Vec<Value>, exclude_auction: bool) -> Vec<Value> {
    if !exclude_auction {
        return items;
    }
    items
        .into_iter()
        .filter(|item| item["is_auction"].as_bool() != Some(true))
        .collect()
}

fn normalize_search_response(data: &Value) -> Vec<Value> {
    data["items"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    if !item.is_object() {
                        return None;
                    }
                    Some(normalize_search_item(item))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn normalize_search_item(item: &Value) -> Value {
    let id = item["id"].as_str().unwrap_or_default();
    let status = item["status"].as_str().unwrap_or_default();
    let item_type = item["itemType"].as_str().unwrap_or_default();
    let photos: Vec<&str> = item["photos"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let thumbnails: Vec<&str> = item["thumbnails"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let image = photos
        .first()
        .or(thumbnails.first())
        .copied()
        .unwrap_or_default();
    let auction = normalize_auction(&item["auction"]);

    let mut out = json!({
        "product_id": id,
        "item_id": id,
        "url": item_url(id),
        "title": item.get("title").or(item.get("name")),
        "name": item["name"],
        "price": to_int(&item["price"]),
        "is_auction": auction.is_some(),
        "auction": auction,
        "status": item_status_label(status),
        "seller_id": item["sellerId"],
        "created": to_int(&item["created"]),
        "updated": to_int(&item["updated"]),
        "thumbnails": thumbnails,
        "photos": photos,
        "image": image,
        "item_type": item_type_label(item_type),
        "item_condition_id": to_int(&item["itemConditionId"]),
        "shipping_payer_id": to_int(&item["shippingPayerId"]),
        "category_id": to_int(&item["categoryId"]),
        "source": "mercari",
    });

    if let Some(brand) = item.get("itemBrand").filter(|v| v.is_object()) {
        out["brand"] = json!({
            "id": to_int(&brand["id"]),
            "name": brand["name"],
            "sub_name": brand["subName"],
        });
    }

    out
}

fn normalize_item_detail(data: &Value) -> Value {
    let id = data["id"].as_str().unwrap_or_default();
    let photos: Vec<&str> = data["photos"]
        .as_array()
        .or(data["photo_urls"].as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let thumbnails: Vec<&str> = data["thumbnails"]
        .as_array()
        .or(data["thumbnail_urls"].as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let image = photos
        .first()
        .or(thumbnails.first())
        .copied()
        .unwrap_or_default();
    let auction = normalize_auction(&data["auction_info"]);

    json!({
        "product_id": id,
        "item_id": id,
        "url": item_url(id),
        "title": data["name"],
        "description": data["description"],
        "price": data["price"],
        "is_auction": auction.is_some(),
        "auction": auction,
        "status": data["status"],
        "category": data.get("item_category").or(data.get("category")),
        "condition": data.get("item_condition").or(data.get("condition")),
        "brand": data.get("item_brand").or(data.get("brand")),
        "photos": photos,
        "thumbnails": thumbnails,
        "image": image,
        "like_count": data.get("num_likes").or(data.get("like_count")),
        "comment_count": data.get("num_comments").or(data.get("comment_count")),
        "seller": data["seller"],
        "shipping": {
            "from_area": data["shipping_from_area"],
            "method": data["shipping_method"],
            "payer": data["shipping_payer"],
            "duration": data["shipping_duration"],
        },
        "created": data["created"],
        "updated": data["updated"],
        "source": "mercari",
    })
}

fn normalize_item_summary(data: &Value) -> Value {
    let id = data["id"]
        .as_str()
        .or(data["item_id"].as_str())
        .unwrap_or_default();
    let photos: Vec<&str> = data["photo_urls"]
        .as_array()
        .or(data["thumbnail_urls"].as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();

    let auction = normalize_auction(&data["auction_info"]);

    json!({
        "product_id": id,
        "item_id": id,
        "url": item_url(id),
        "title": data.get("name").or(data.get("title")),
        "price": data["price"],
        "is_auction": auction.is_some(),
        "auction": auction,
        "status": data["status"],
        "image": photos.first().copied().unwrap_or(""),
        "seller_id": data["seller_id"],
    })
}

fn normalize_autocomplete(s: &Value) -> Value {
    if let Some(kw) = s.get("keyword").filter(|v| v.is_object()) {
        return json!({
            "type": "keyword",
            "title": kw["title"],
            "subtitle": kw["subtitle"],
        });
    }
    if let Some(seller) = s.get("seller").filter(|v| v.is_object()) {
        return json!({
            "type": "seller",
            "user_id": seller["id"],
            "display_name": seller["name"],
        });
    }
    json!({"type": "unknown"})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_item_id() {
        assert_eq!(extract_item_id("m1234567890"), "m1234567890");
        assert_eq!(extract_item_id("1234567890"), "m1234567890");
        assert_eq!(
            extract_item_id("https://jp.mercari.com/item/m1234567890"),
            "m1234567890"
        );
        // new hash format
        assert_eq!(
            extract_item_id("2JTNgESdSinuH7PX7iJk2w"),
            "2JTNgESdSinuH7PX7iJk2w"
        );
        assert_eq!(
            extract_item_id("https://jp.mercari.com/item/2JTNgESdSinuH7PX7iJk2w"),
            "2JTNgESdSinuH7PX7iJk2w"
        );
        assert_eq!(extract_item_id("short"), "");
    }

    #[test]
    fn test_search_sort() {
        assert_eq!(search_sort(""), (0, 0));
        assert_eq!(search_sort("newest"), (1, 0));
        assert_eq!(search_sort("price_asc"), (4, 1));
        assert_eq!(search_sort("price_desc"), (4, 0));
    }

    // [S1-24]
    #[test]
    fn test_search_sort_price_without_time_filter() {
        assert_eq!(search_sort("price_without_time_filter"), (6, 0));
        assert_eq!(search_sort("SORT_PRICE_WITHOUT_TIME_FILTER"), (6, 0));
        assert_eq!(search_sort("price_asc_without_time_filter"), (6, 1));
        assert_eq!(search_sort("lowest_price_without_time_filter"), (6, 1));
        assert_eq!(search_sort("price_desc_without_time_filter"), (6, 0));
        assert_eq!(search_sort("highest_price_without_time_filter"), (6, 0));
        let req = build_search_request(
            &SearchParams {
                query: "x".into(),
                sort: "price_asc_without_time_filter".into(),
                ..Default::default()
            },
            20,
            "sess",
        );
        assert_eq!(
            req["searchCondition"]["sort"],
            "SORT_PRICE_WITHOUT_TIME_FILTER"
        );
        assert_eq!(req["searchCondition"]["order"], "ORDER_ASC");
        for s in [
            "price_without_time_filter",
            "sort_price_without_time_filter",
            "price_asc_without_time_filter",
            "lowest_price_without_time_filter",
            "price_desc_without_time_filter",
            "highest_price_without_time_filter",
        ] {
            assert!(
                SEARCH_SORT_SPELLINGS.contains(&s),
                "spelling list must contain {s}"
            );
            // Every published spelling must actually map to index 6.
            assert_eq!(search_sort(s).0, 6, "spelling {s}");
        }
    }

    // [S1-21]
    #[test]
    fn test_build_search_request_extras_reach_wire() {
        let params = SearchParams {
            query: "バッグ".into(),
            has_coupon: true,
            attributes: vec![json!({"attributeId": 1, "valueIds": [2, 3]})],
            sku_ids: vec![111, 222],
            shop_ids: vec![333],
            exclude_shipping_method_ids: vec![4, 5],
            shipping_from_area: vec![1, 13],
            shipping_method: vec![7],
            ..Default::default()
        };
        let req = build_search_request(&params, 20, "sess-1");
        let c = &req["searchCondition"];
        assert_eq!(c["hasCoupon"], true);
        assert_eq!(c["attributes"][0]["attributeId"], 1);
        assert_eq!(c["attributes"][0]["valueIds"], json!([2, 3]));
        assert_eq!(c["skuIds"], json!([111, 222]));
        assert_eq!(c["shopIds"], json!([333]));
        assert_eq!(c["excludeShippingMethodIds"], json!([4, 5]));
        assert_eq!(c["shippingFromArea"], json!([1, 13]));
        assert_eq!(c["shippingMethod"], json!([7]));
        assert_eq!(req["searchSessionId"], "sess-1");
    }

    // [S1-21] Defaults must preserve the previous fixed wire values.
    #[test]
    fn test_build_search_request_defaults_preserve_old_fixed_values() {
        let req = build_search_request(
            &SearchParams {
                query: "x".into(),
                ..Default::default()
            },
            20,
            "sess",
        );
        let c = &req["searchCondition"];
        assert_eq!(c["hasCoupon"], false);
        assert_eq!(c["attributes"], json!([]));
        assert_eq!(c["skuIds"], json!([]));
        assert_eq!(c["shopIds"], json!([]));
        assert_eq!(c["excludeShippingMethodIds"], json!([]));
        assert_eq!(c["shippingFromArea"], json!([]));
        assert_eq!(c["shippingMethod"], json!([]));
    }

    // [S1-22] Live probing disproved offset/page continuation, so the
    // params builder must never emit a continuation parameter.
    #[test]
    fn test_reviews_params_have_no_continuation() {
        for page in [1, 2, 3] {
            let p = build_reviews_params("42", page, 20);
            assert_eq!(p.len(), 3, "params must stay the original three: {p:?}");
            assert!(!p.iter().any(|(k, _)| k == "offset" || k == "page"));
        }
    }

    // [S1-22]
    #[test]
    fn test_normalize_reviews_response_fixture() {
        let body = json!({
            "data": [
                {"fame": "good", "message": "ありがとうございました", "subject": "seller",
                 "created": 1700000000_i64, "user": {"id": 99, "name": "buyer-a"}},
                {"fame": "bad", "message": "", "subject": "seller",
                 "created": "1700000100", "user": {"id": "100", "name": "buyer-b"}}
            ],
            "meta": {
                "has_next": true,
                "num_ratings": 12,
                "star_rating_score": 5,
                "ratings": {"good": 10, "normal": 1, "bad": 1}
            }
        });
        let out = normalize_reviews_response(&body, "42", 2, 5);
        assert_eq!(out["page"], 2);
        assert_eq!(out["has_next"], true);
        assert_eq!(out["count"], 2);
        assert_eq!(out["ratings_summary"]["total"], 12);
        assert_eq!(out["reviews"][0]["reviewer_id"], "99");
        assert_eq!(out["reviews"][1]["reviewer_id"], "100");
        assert_eq!(out["reviews"][1]["date"], 1700000100_i64);
    }

    // [S1-23] Offset continuation was disproven live; the params
    // builder never emits it and next_offset stays null.
    #[test]
    fn test_seller_items_pagination() {
        for off in [0, 30] {
            let p = build_seller_items_params("42", "on_sale", 30, off);
            assert!(
                !p.iter().any(|(k, _)| k == "offset" || k == "page"),
                "no continuation parameter may be sent: {p:?}"
            );
        }
        // Tolerant meta extraction, both spellings.
        let meta = json!({"nextPageToken": "tok-abc", "has_next": true, "totalItemCount": 87});
        let pg = seller_items_pagination(Some(&meta), 0, 30, 30);
        assert_eq!(pg["next_page_token"], "tok-abc");
        assert_eq!(pg["has_next"], true);
        assert_eq!(pg["total_item_count"], 87);
        assert_eq!(pg["next_offset"], Value::Null);
        let pg_none = seller_items_pagination(None, 0, 3, 30);
        assert!(pg_none.get("next_page_token").is_none());
        assert_eq!(pg_none["next_offset"], Value::Null);
    }

    #[test]
    fn test_extract_item_id_url_variants() {
        assert_eq!(
            extract_item_id("https://jp.mercari.com/item/m1234567890?x=1"),
            "m1234567890"
        );
        assert_eq!(
            extract_item_id("https://mercari.com/jp/items/m9876543210/"),
            "m9876543210"
        );
    }

    #[test]
    fn test_extract_item_id_edge_cases() {
        assert_eq!(extract_item_id(""), "");
        assert_eq!(extract_item_id("abc"), "");
        assert_eq!(extract_item_id("m123"), "");
    }

    #[test]
    fn test_normalize_search_item_auction() {
        // Shape measured live 2026-11-05 (m62825700406): search `auction`
        // is populated only when the request sets withAuction: true.
        let item = json!({
            "id": "m62825700406",
            "name": "Apple iPhone 14 Pro 256GB",
            "price": "30200",
            "status": "ITEM_STATUS_ON_SALE",
            "itemType": "ITEM_TYPE_MERCARI",
            "auction": {
                "id": "",
                "bidDeadline": "2026-11-05T11:11:00Z",
                "highestBid": "30200",
                "initialPrice": "10000",
                "totalBid": "29"
            }
        });
        let out = normalize_search_item(&item);
        assert_eq!(out["is_auction"], true);
        assert_eq!(out["auction"]["highest_bid"], 30200);
        assert_eq!(out["auction"]["initial_price"], 10000);
        assert_eq!(out["auction"]["total_bids"], 29);
        assert_eq!(out["auction"]["bid_deadline"], "2026-11-05T11:11:00Z");
        assert!(
            out["auction"]["id"].is_null(),
            "empty search auction id must become null"
        );
    }

    #[test]
    fn test_normalize_search_item_not_auction() {
        let item = json!({
            "id": "m45533881258",
            "name": "通常出品",
            "price": "40000",
            "status": "ITEM_STATUS_ON_SALE",
            "itemType": "ITEM_TYPE_MERCARI",
            "auction": null
        });
        let out = normalize_search_item(&item);
        assert_eq!(out["is_auction"], false);
        assert!(out["auction"].is_null());
    }

    #[test]
    fn test_normalize_item_detail_auction() {
        // Detail shape measured live 2026-11-05 (m62825700406).
        let data = json!({
            "id": "m62825700406",
            "name": "Apple iPhone 14 Pro 256GB",
            "price": 30200,
            "status": "on_sale",
            "auction_info": {
                "id": "110329327",
                "start_time": 1793784237_i64,
                "expected_end_time": 1793877060_i64,
                "total_bids": 29,
                "initial_price": 10000,
                "highest_bid": 30200,
                "state": "STATE_ONGOING",
                "auction_type": "AUCTION_TYPE_NORMAL"
            }
        });
        let out = normalize_item_detail(&data);
        assert_eq!(out["is_auction"], true);
        assert_eq!(out["auction"]["id"], "110329327");
        assert_eq!(out["auction"]["highest_bid"], 30200);
        assert_eq!(out["auction"]["total_bids"], 29);
        assert_eq!(out["auction"]["expected_end_time"], 1793877060_i64);
        assert_eq!(out["auction"]["state"], "STATE_ONGOING");
    }

    #[test]
    fn test_normalize_item_detail_not_auction() {
        let data =
            json!({"id": "m45533881258", "name": "通常出品", "price": 40000, "status": "on_sale"});
        let out = normalize_item_detail(&data);
        assert_eq!(out["is_auction"], false);
        assert!(out["auction"].is_null());
    }

    #[test]
    fn test_normalize_item_summary_auction_seller_shape() {
        // Seller-list shape measured live 2026-11-05: total_bid (singular)
        // and bid_deadline (ISO), unlike the detail shape.
        let data = json!({
            "id": "m62825700406",
            "name": "Apple iPhone 14 Pro 256GB",
            "price": 30200,
            "status": "on_sale",
            "auction_info": {
                "id": "110329327",
                "highest_bid": 30200,
                "initial_price": 10000,
                "total_bid": 29,
                "bid_deadline": "2026-11-05T11:11:00Z"
            }
        });
        let out = normalize_item_summary(&data);
        assert_eq!(out["is_auction"], true);
        assert_eq!(out["auction"]["total_bids"], 29);
        assert_eq!(out["auction"]["bid_deadline"], "2026-11-05T11:11:00Z");
    }

    #[test]
    fn test_filter_exclude_auction() {
        let items = vec![
            json!({"product_id": "a", "is_auction": true}),
            json!({"product_id": "b", "is_auction": false}),
            json!({"product_id": "c", "is_auction": true}),
        ];
        let kept = filter_exclude_auction(items.clone(), true);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0]["product_id"], "b");
        assert_eq!(filter_exclude_auction(items, false).len(), 3);
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search() {
        let client = MercariClient::new();
        let result = client
            .search_products(SearchParams {
                query: "iPhone".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result["provider"], "mercari");
        assert_eq!(result["ok"], true, "search failed: {}", result);
        let items = result["items"].as_array().unwrap();
        assert!(
            !items.is_empty(),
            "search returned 0 items — API may be broken"
        );
        let count = result["count"].as_i64().unwrap();
        assert_eq!(count as usize, items.len());
        for item in items {
            let pid = item["product_id"].as_str().unwrap();
            assert!(!pid.is_empty());
            let url = item["url"].as_str().unwrap();
            assert!(url.contains("mercari.com"), "bad url: {}", url);
            assert!(url.contains("afid="), "missing afid: {}", url);
            assert!(
                item["price"].as_i64().is_some() || item["price"].is_object(),
                "price missing: {:?}",
                item["price"]
            );
            assert_eq!(item["source"], "mercari");
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_multiword() {
        let client = MercariClient::new();
        let result = client
            .search_products(SearchParams {
                query: "ワイヤレス イヤホン Bluetooth".into(),
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "multi-word search failed: {}", result);
        let items = result["items"].as_array().unwrap();
        assert!(!items.is_empty(), "multi-word search returned 0 items");
    }

    // [S1-24] The index-6 sort (SORT_PRICE_WITHOUT_TIME_FILTER) must be
    // accepted by the live search API via its new spellings.
    #[tokio::test]
    #[ignore = "live test: verify index-6 sort is accepted"]
    async fn test_live_search_sort_price_without_time_filter() {
        let client = MercariClient::new();
        let result = client
            .search_products(SearchParams {
                query: "カメラ".into(),
                limit: 5,
                sort: "price_without_time_filter".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "index-6 sort failed: {}", result);
        assert_eq!(result["sort"], "price_without_time_filter");
        let items = result["items"].as_array().unwrap();
        assert!(!items.is_empty(), "index-6 sort returned 0 items");
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_sort_price() {
        let client = MercariClient::new();
        let result = client
            .search_products(SearchParams {
                query: "カメラ".into(),
                limit: 5,
                sort: "price_asc".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "sorted search failed: {}", result);
        let items = result["items"].as_array().unwrap();
        assert!(!items.is_empty(), "sorted search returned 0 items");
        let prices: Vec<i64> = items.iter().filter_map(|i| i["price"].as_i64()).collect();
        if prices.len() >= 2 {
            for w in prices.windows(2) {
                assert!(w[0] <= w[1], "prices not ascending: {} > {}", w[0], w[1]);
            }
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_auction_fields() {
        let client = MercariClient::new();
        let result = client
            .search_products(SearchParams {
                query: "iPhone 14 Pro".into(),
                limit: 120,
                sort: "newest".into(),
                status: "on_sale".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "search failed: {}", result);
        let items = result["items"].as_array().unwrap();
        assert!(!items.is_empty(), "search returned 0 items");
        for item in items {
            assert!(
                item["is_auction"].is_boolean(),
                "is_auction missing: {:?}",
                item
            );
            if item["is_auction"] == true {
                assert!(
                    item["auction"]["highest_bid"].is_i64(),
                    "auction highest_bid missing: {:?}",
                    item
                );
                assert!(
                    item["auction"]["total_bids"].is_i64(),
                    "auction total_bids missing: {:?}",
                    item
                );
            } else {
                assert!(
                    item["auction"].is_null(),
                    "non-auction item must have null auction: {:?}",
                    item
                );
            }
        }
        let auction_item = items.iter().find(|i| i["is_auction"] == true)
            .expect("no auction item found in 120 newest iPhone 14 Pro results — auction population may have regressed");
        // Cross-check: the same item must also be flagged in detail.
        let pid = auction_item["product_id"].as_str().unwrap();
        let detail = client.get_product(pid).await.unwrap();
        assert_eq!(detail["ok"], true, "get_product failed: {}", detail);
        assert_eq!(
            detail["item"]["is_auction"], true,
            "detail disagrees with search for auction item {}: {}",
            pid, detail["item"]
        );
        assert!(detail["item"]["auction"]["total_bids"].is_i64());
        assert!(detail["item"]["auction"]["expected_end_time"].is_i64());
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_exclude_auction() {
        let client = MercariClient::new();
        let result = client
            .search_products(SearchParams {
                query: "iPhone 14 Pro".into(),
                limit: 40,
                sort: "newest".into(),
                status: "on_sale".into(),
                exclude_auction: true,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(result["ok"], true, "search failed: {}", result);
        assert_eq!(result["exclude_auction"], true);
        let items = result["items"].as_array().unwrap();
        assert!(!items.is_empty(), "exclude_auction search returned 0 items");
        for item in items {
            assert_eq!(
                item["is_auction"], false,
                "auction item leaked through exclude_auction: {:?}",
                item
            );
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_get_product() {
        let client = MercariClient::new();
        let search = client
            .search_products(SearchParams {
                query: "バッグ".into(),
                limit: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(search["ok"], true);
        let items = search["items"].as_array().unwrap();
        assert!(!items.is_empty(), "need items to test get_product");
        // Find an m-format ID
        let m_item = items.iter().find(|i| {
            i["product_id"]
                .as_str()
                .is_some_and(|id| id.starts_with('m'))
        });
        assert!(
            m_item.is_some(),
            "no m-format product_id found in {} items — Mercari may have changed ID format",
            items.len()
        );
        let pid = m_item.unwrap()["product_id"].as_str().unwrap();
        let detail = client.get_product(pid).await.unwrap();
        assert_eq!(detail["provider"], "mercari");
        assert_eq!(
            detail["ok"], true,
            "get_product FAILED for {}: {}",
            pid, detail
        );
        let item = &detail["item"];
        assert!(item.is_object(), "item must be object");
        assert!(!item["product_id"].as_str().unwrap_or("").is_empty());
        assert!(item["url"].as_str().unwrap().contains("mercari.com"));
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_suggest_keywords() {
        let client = MercariClient::new();
        let result = client.suggest_keywords("iPhone").await.unwrap();
        assert_eq!(result["provider"], "mercari");
        assert_eq!(result["ok"], true, "suggest failed: {}", result);
        let suggestions = result["suggestions"]
            .as_array()
            .expect("suggestions must be array");
        assert!(!suggestions.is_empty(), "suggest returned 0 suggestions");
        for s in suggestions {
            assert!(s.is_object(), "suggestion must be object: {:?}", s);
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_empty_query() {
        let client = MercariClient::new();
        let result = client
            .search_products(SearchParams {
                query: "".into(),
                limit: 5,
                ..Default::default()
            })
            .await;
        // Mercari returns 400 for empty query — this must not panic
        match result {
            Ok(v) => assert_eq!(v["provider"], "mercari"),
            Err(e) => assert!(
                e.to_string().contains("400") || e.to_string().contains("Bad Request"),
                "unexpected error: {}",
                e
            ),
        }
    }

    // [S1-22/S1-23 probe] Raw-shape probe: `offset` was ignored by both
    // endpoints live (2026-11-08). Fetch page 1 raw, then the same
    // request plus `page=2`, and print meta + leading ids so the real
    // continuation parameter can be identified from evidence.
    #[tokio::test]
    #[ignore = "live probe: identify the real pagination parameter"]
    async fn test_live_probe_pagination_params() {
        async fn raw_get(client: &MercariClient, endpoint: &str, params: &[(&str, &str)]) -> Value {
            let url = format!("{API_BASE}{endpoint}");
            let dpop_proof = {
                let full_url = reqwest::Url::parse_with_params(&url, params).unwrap();
                let signer = client.dpop.lock().await;
                signer.proof("GET", full_url.as_ref())
            };
            client
                .client
                .get(&url)
                .query(&params)
                .header("DPoP", dpop_proof)
                .header("X-Platform", "web")
                .header("Accept-Language", "ja-JP")
                .header("Accept", "application/json")
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap()
        }
        let client = MercariClient::new();
        let search = client
            .search_products(SearchParams {
                query: "バッグ".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        let seller_id = search["items"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|i| {
                i["seller_id"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| i["seller_id"].as_i64().map(|n| n.to_string()))
            })
            .expect("need a seller_id");
        println!("PROBE seller_id={seller_id}");

        let rev_base: Vec<(&str, &str)> = vec![
            ("user_id", seller_id.as_str()),
            ("fame", "good,normal,bad"),
            ("limit", "5"),
        ];
        let rev1 = raw_get(&client, REVIEWS_ENDPOINT, &rev_base).await;
        let mut rev2_params = rev_base.clone();
        rev2_params.push(("page", "2"));
        let rev2 = raw_get(&client, REVIEWS_ENDPOINT, &rev2_params).await;
        println!("PROBE reviews meta p1: {}", rev1["meta"]);
        println!("PROBE reviews meta p2(page=2): {}", rev2["meta"]);
        println!(
            "PROBE reviews data differs with page=2: {}",
            rev1["data"] != rev2["data"]
        );

        let items_base: Vec<(&str, &str)> = vec![
            ("seller_id", seller_id.as_str()),
            ("status", "on_sale"),
            ("limit", "5"),
            ("with_auction", "true"),
            ("with_total_item_count", "true"),
        ];
        let it1 = raw_get(&client, ITEMS_GET_ENDPOINT, &items_base).await;
        let mut it2_params = items_base.clone();
        it2_params.push(("page", "2"));
        let it2 = raw_get(&client, ITEMS_GET_ENDPOINT, &it2_params).await;
        println!("PROBE items meta p1: {}", it1["meta"]);
        println!("PROBE items meta p2(page=2): {}", it2["meta"]);
        println!(
            "PROBE items data differs with page=2: {}",
            it1["data"] != it2["data"]
        );
    }

    // [S1-22] Live-verified 2026-11-08: offset/page are ignored by the
    // endpoint, so page 2 must now fail explicitly (client-side, no
    // send) instead of returning page 1 again.
    #[tokio::test]
    #[ignore = "live test: verify reviews page2 is rejected explicitly"]
    async fn test_live_get_reviews_page2_differs() {
        let client = MercariClient::new();
        let search = client
            .search_products(SearchParams {
                query: "バッグ".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        let seller_id = search["items"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|i| {
                i["seller_id"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| i["seller_id"].as_i64().map(|n| n.to_string()))
            })
            .expect("need a seller_id");
        let p1 = client.get_reviews(&seller_id, 1, 5).await.unwrap();
        assert_eq!(p1["ok"], true, "page1 failed: {p1}");
        if p1["has_next"] == true {
            let p2 = client.get_reviews(&seller_id, 2, 5).await;
            assert!(
                p2.is_err(),
                "page2 must be rejected explicitly, got: {p2:?}"
            );
        }
    }

    // [S1-23] Live-verified 2026-11-08: offset/page are ignored by the
    // endpoint, so offset > 0 must fail explicitly (client-side).
    #[tokio::test]
    #[ignore = "live test: verify seller items offset is rejected explicitly"]
    async fn test_live_seller_items_offset_differs() {
        let client = MercariClient::new();
        let search = client
            .search_products(SearchParams {
                query: "バッグ".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        let seller_id = search["items"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|i| {
                i["seller_id"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| i["seller_id"].as_i64().map(|n| n.to_string()))
            })
            .expect("need a seller_id");
        let p1 = client
            .search_seller_products_with_offset(&seller_id, "on_sale", 5, 0)
            .await
            .unwrap();
        assert_eq!(p1["ok"], true, "seller items failed: {p1}");
        assert_eq!(p1["next_offset"], Value::Null);
        let p2 = client
            .search_seller_products_with_offset(&seller_id, "on_sale", 5, 5)
            .await;
        assert!(
            p2.is_err(),
            "offset > 0 must be rejected explicitly, got: {p2:?}"
        );
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_get_reviews() {
        let client = MercariClient::new();
        // Search to find a seller_id
        let search = client
            .search_products(SearchParams {
                query: "バッグ".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(search["ok"], true);
        let items = search["items"].as_array().unwrap();
        assert!(!items.is_empty(), "need items to find a seller_id");

        let seller_id = items
            .iter()
            .find_map(|i| {
                let v = &i["seller_id"];
                if let Some(s) = v.as_str() {
                    if !s.is_empty() && s != "0" {
                        return Some(s.to_string());
                    }
                }
                if let Some(n) = v.as_i64() {
                    if n > 0 {
                        return Some(n.to_string());
                    }
                }
                None
            })
            .expect("no seller_id found in search results");

        let result = client.get_reviews(&seller_id, 1, 5).await.unwrap();
        assert_eq!(result["provider"], "mercari");
        assert_eq!(result["ok"], true, "get_reviews failed: {}", result);
        assert!(
            result["ratings_summary"].is_object(),
            "ratings_summary must be object"
        );
        assert!(result["reviews"].is_array(), "reviews must be array");
        let reviews = result["reviews"].as_array().unwrap();
        assert!(result["count"].as_i64().unwrap() as usize == reviews.len());
        for r in reviews {
            assert!(r["rating"].is_string(), "rating must be string: {:?}", r);
            assert!(r["message"].is_string(), "message must be string: {:?}", r);
        }
    }
}
