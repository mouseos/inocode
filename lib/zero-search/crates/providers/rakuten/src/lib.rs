//! Rakuten Ichiba (楽天市場) provider.
//!
//! Reproduces the official Android app's guest BFF traffic
//! (`app-ichiba-gateway.rakuten.co.jp/mobile-bff/`, all POST + JSON).
//! No login, token or device registration is involved: the app
//! itself routes guests to this host with only the fixed
//! `x-clientid: ichiba_android_long` header.
//!
//! Search uses `dynamic/search/v1` (the search-results screen API,
//! the only route that carries sort / filter parameters). Product
//! detail uses `itemscreen/get/v16`, reviews use `review/list/v3`.
//! The detail endpoint answers HTTP 207 (BFF partial success) when
//! member-only features fail; that is a normal response shape and
//! success is judged by the core features carrying data.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

const BASE: &str = "https://app-ichiba-gateway.rakuten.co.jp/mobile-bff/";
const CLIENT_ID: &str = "ichiba_android_long";
const USER_AGENT: &str = "okhttp/4.12.0";

/// Sort values for `rakuten_search_products` (the `allValue` forms
/// the app sends for a normal guest search).
pub const SORTS: &[&str] = &[
    "relevancy",
    "price_low_to_high_all",
    "price_high_to_low_all",
    "newest",
    "review_count_all",
    "review_score_all",
    "price_plus_shippingfee_low_to_high_all",
    "price_plus_shippingfee_high_to_low_all",
];

/// The `highRelevancyValue` sort forms: the app sends these
/// instead of the `allValue` forms when the search is in the
/// high-relevancy state (the same six sorts, without `_all`;
/// `relevancy` and `newest` have no separate form). Order
/// corresponds to [`SORTS`] entries 2, 3, 5, 6, 7, 8.
pub const SORTS_HIGH_RELEVANCY: &[&str] = &[
    "price_low_to_high",
    "price_high_to_low",
    "review_count",
    "review_score",
    "price_plus_shippingfee_low_to_high",
    "price_plus_shippingfee_high_to_low",
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

/// One genre in a search item's genre path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenreRef {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Item {
    /// `"{shop_id}_{item_id}"` — pass the parts to
    /// `rakuten_get_product` / `rakuten_get_reviews`.
    pub product_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shop_id: Option<i64>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seller: Option<SellerInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shipping_price: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stock_quantity: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shipping_methods: Option<Vec<String>>,
    /// Search items: the item's subtitle / catch copy line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// Search items: genre path (id + name), root placeholder
    /// entries (empty name, id 0) removed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub genres: Option<Vec<GenreRef>>,
    /// Search items: the raw `point` object (multipliers etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<Value>,
    /// Search items: the raw `coupons` array (only when non-empty).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coupons: Option<Value>,
    /// Search items: the raw `sale` object (only when present).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sale: Option<Value>,
    /// Search items: delivery estimate text
    /// (`shipping.estimateDeliveryDay`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_estimate: Option<String>,
    /// Detail: the item's catch copy (`tagLine`, HTML line breaks
    /// preserved as served).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag_line: Option<String>,
    /// Detail: every variant, normalized (sku_id, attributes,
    /// prices, quantity). The price / stock fields above mirror
    /// the first variant, as before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variants: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub provider: String,
    pub ok: bool,
    pub query: String,
    pub offset: i32,
    pub sort: String,
    pub items: Vec<Item>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_available: Option<i64>,
    /// Related search keywords from the response's
    /// `related_keywords` module, when returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub related_keywords: Option<Vec<String>>,
    /// The `dynamic_filter` / `facet_conditions` modules' data
    /// (server-distributed filter candidates), keyed by module
    /// id, when returned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub facets: Option<Value>,
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
// Search (dynamic/search/v1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    /// One of [`SORTS`]; empty means the app default (`relevancy`).
    pub sort: String,
    pub price_min: Option<i64>,
    pub price_max: Option<i64>,
    /// Genre id (`category` in the app's parameter map).
    pub category: String,
    pub offset: i32,
    /// Flag filters, comma-joined the way the app sends them
    /// (fixed values: `free_shipping`, `out_of_stock`, `super_deal`,
    /// `subscription`, `group_distribution`; other values are the
    /// server-distributed CommonFilter option values).
    pub filters: String,
    pub gift: String,
    /// `new` or `used`.
    pub condition: String,
    /// Review score lower bound (3.0 / 3.5 / 4.0 / 4.5).
    pub review_min: Option<f64>,
    pub excluded_keyword: String,
    pub brand_tags: String,
    pub shop_id: String,
    pub shop_code: String,
    /// Prefecture id; 48 (overseas) pairs with `country`.
    pub region: Option<i64>,
    pub country: String,
    pub postal_code: String,
    pub tags: String,
    pub tag_groups: String,
    pub event: Option<i64>,
    pub ran: String,
    pub inshop_pure_lexical_search: Option<bool>,
    pub image_search_items: String,
    pub similar_image_search_item: String,
    /// Response modules; empty means the measured default set.
    pub modules: String,
    pub ab: String,
    /// Client-side cap on returned items (page size is the
    /// server's, measured 45).
    pub limit: i32,
}

/// The gateway wraps every response in `{body, status}`; the
/// parsers work on the inner body. Pure; unit-tested.
fn unwrap_body(parsed: Value) -> Value {
    match parsed.get("body") {
        Some(body) if body.is_object() => body.clone(),
        _ => parsed,
    }
}

fn envelope(feature: &str, feature_params: Value) -> Value {
    envelope_multi(&[(feature, feature_params)], json!({}))
}

fn envelope_multi(features: &[(&str, Value)], common_params: Value) -> Value {
    let mut feats = Map::new();
    for (name, params) in features {
        feats.insert(
            name.to_string(),
            json!({ "params": params, "include": [], "exclude": [] }),
        );
    }
    json!({
        "common": { "params": common_params, "include": [], "exclude": [] },
        "features": Value::Object(feats),
    })
}

/// Build the `dynamic/search/v1` request body. Pure; unit-tested.
pub fn build_search_body(params: &SearchParams) -> Value {
    let mut p = Map::new();
    p.insert("config".into(), json!("ichiba_search_android"));
    p.insert("lang".into(), json!("ja"));
    p.insert("v".into(), json!(4));
    p.insert("query".into(), json!(params.query));
    let sort = if params.sort.is_empty() {
        "relevancy"
    } else {
        params.sort.as_str()
    };
    p.insert("sort".into(), json!(sort));
    // Price band: sent only when set. Sending -1 for "unset"
    // makes the live server answer numFound=0 (measured
    // 2026-11-07; round2's static note said the app sends -1,
    // but the server's measured behaviour wins).
    if let Some(v) = params.price_min {
        p.insert("priceMin".into(), json!(v));
    }
    if let Some(v) = params.price_max {
        p.insert("priceMax".into(), json!(v));
    }
    p.insert("offset".into(), json!(params.offset.max(0)));
    let modules = if params.modules.is_empty() {
        "ichiba_search,dynamic_filter,facet_conditions"
    } else {
        params.modules.as_str()
    };
    p.insert("modules".into(), json!(modules));
    if !params.category.is_empty() {
        p.insert("category".into(), json!(params.category));
    }
    if !params.filters.is_empty() {
        p.insert("filters".into(), json!(params.filters));
    }
    if !params.gift.is_empty() {
        p.insert("gift".into(), json!(params.gift));
    }
    if !params.condition.is_empty() {
        p.insert("condition".into(), json!(params.condition));
    }
    if let Some(v) = params.review_min {
        p.insert("review".into(), json!(v));
    }
    if !params.excluded_keyword.is_empty() {
        p.insert("excludedKeyword".into(), json!(params.excluded_keyword));
    }
    if !params.brand_tags.is_empty() {
        p.insert("brandTags".into(), json!(params.brand_tags));
    }
    if !params.shop_id.is_empty() {
        p.insert("shopId".into(), json!(params.shop_id));
    }
    if !params.shop_code.is_empty() {
        p.insert("shopCode".into(), json!(params.shop_code));
    }
    if let Some(v) = params.region {
        p.insert("region".into(), json!(v));
    }
    if !params.country.is_empty() {
        p.insert("country".into(), json!(params.country));
    }
    if !params.postal_code.is_empty() {
        p.insert("postalCode".into(), json!(params.postal_code));
    }
    if !params.tags.is_empty() {
        p.insert("tags".into(), json!(params.tags));
    }
    if !params.tag_groups.is_empty() {
        p.insert("tagGroups".into(), json!(params.tag_groups));
    }
    if let Some(v) = params.event {
        p.insert("event".into(), json!(v));
    }
    if !params.ran.is_empty() {
        p.insert("ran".into(), json!(params.ran));
    }
    if let Some(v) = params.inshop_pure_lexical_search {
        p.insert("inshopPureLexicalSearch".into(), json!(v));
    }
    if !params.image_search_items.is_empty() {
        p.insert("imageSearchItems".into(), json!(params.image_search_items));
    }
    if !params.similar_image_search_item.is_empty() {
        p.insert(
            "similarImageSearchItem".into(),
            json!(params.similar_image_search_item),
        );
    }
    if !params.ab.is_empty() {
        p.insert("ab".into(), json!(params.ab));
    }
    envelope("dynamicSearchInfo", Value::Object(p))
}

fn image_url(img: &Value) -> Option<String> {
    if let Some(s) = img.as_str() {
        return Some(s.to_string());
    }
    if let Some(u) = img.get("url").and_then(Value::as_str) {
        if !u.contains("${") {
            return Some(u.to_string());
        }
    }
    img.get("resizableUrl")
        .and_then(Value::as_str)
        .map(|u| u.replace("${width}x${height}", "400x400"))
}

/// Parse a `dynamic/search/v1` response body into items + total.
/// Pure; unit-tested against the captured live response.
pub fn parse_search_response(body: &Value) -> (Vec<Item>, Option<i64>) {
    let modules = body
        .pointer("/dynamicSearchInfo/data/modules")
        .and_then(Value::as_array);
    let mut items = Vec::new();
    let mut total = None;
    if let Some(modules) = modules {
        for module in modules {
            if module.get("id").and_then(Value::as_str) != Some("ichiba_search") {
                continue;
            }
            let data = &module["data"];
            total = data.pointer("/pagination/numFound").and_then(Value::as_i64);
            if let Some(arr) = data.get("items").and_then(Value::as_array) {
                for it in arr {
                    items.push(parse_search_item(it));
                }
            }
        }
    }
    (items, total)
}

fn parse_search_item(it: &Value) -> Item {
    let shop = &it["shop"];
    let shop_id = shop.get("id").and_then(Value::as_i64);
    let code = it
        .get("code")
        .and_then(|c| {
            c.as_i64()
                .map(|n| n.to_string())
                .or_else(|| c.as_str().map(str::to_string))
        })
        .unwrap_or_default();
    let images: Vec<String> = it
        .get("images")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(image_url).collect())
        .unwrap_or_default();
    let price = it
        .get("price")
        .and_then(|p| p.as_i64().or_else(|| p.as_f64().map(|f| f.round() as i64)));
    Item {
        product_id: match shop_id {
            Some(sid) => format!("{sid}_{code}"),
            None => code.clone(),
        },
        item_id: Some(code),
        shop_id,
        title: it["name"].as_str().unwrap_or_default().to_string(),
        url: it["url"].as_str().unwrap_or_default().to_string(),
        source: "rakuten".into(),
        price: price.map(Price::yen),
        availability: match it["isSoldOut"].as_bool() {
            Some(true) => Some("sold_out".into()),
            Some(false) => Some("in_stock".into()),
            None => None,
        },
        image: images.first().cloned(),
        images: if images.is_empty() {
            None
        } else {
            Some(images)
        },
        seller: Some(SellerInfo {
            id: shop_id.map(|s| s.to_string()),
            name: shop["name"].as_str().map(str::to_string),
            url: shop["url"].as_str().map(str::to_string),
        }),
        review_score: it.pointer("/review/score").and_then(Value::as_f64),
        review_count: it.pointer("/review/numReviews").and_then(Value::as_i64),
        shipping_price: it.pointer("/shipping/price").and_then(Value::as_i64),
        subtitle: it["subtitle"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        genres: parse_genres(it.get("genres")),
        point: it.get("point").filter(|p| p.is_object()).cloned(),
        coupons: it
            .get("coupons")
            .filter(|c| c.as_array().is_some_and(|a| !a.is_empty()))
            .cloned(),
        sale: it.get("sale").filter(|s| !s.is_null()).cloned(),
        delivery_estimate: it
            .pointer("/shipping/estimateDeliveryDay")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        ..Default::default()
    }
}

fn parse_genres(raw: Option<&Value>) -> Option<Vec<GenreRef>> {
    let arr = raw?.as_array()?;
    let genres: Vec<GenreRef> = arr
        .iter()
        .filter_map(|g| {
            let name = g["name"].as_str()?;
            if name.is_empty() {
                return None;
            }
            Some(GenreRef {
                id: g["id"].as_i64().unwrap_or(0),
                name: name.to_string(),
            })
        })
        .collect();
    if genres.is_empty() {
        None
    } else {
        Some(genres)
    }
}

/// Extract the non-item modules of a search response: the
/// `related_keywords` module's keyword list, and the
/// `dynamic_filter` / `facet_conditions` modules' data (the
/// server-distributed filter/facet candidates), when the server
/// returned them. Pure; unit-tested.
pub fn parse_search_extras(body: &Value) -> (Option<Vec<String>>, Option<Value>) {
    let modules = body
        .pointer("/dynamicSearchInfo/data/modules")
        .and_then(Value::as_array);
    let mut related = None;
    let mut facets = Map::new();
    if let Some(modules) = modules {
        for module in modules {
            match module.get("id").and_then(Value::as_str) {
                Some("related_keywords") => {
                    let list: Vec<String> = module["data"]["related"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|k| k.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !list.is_empty() {
                        related = Some(list);
                    }
                }
                Some(id @ ("dynamic_filter" | "facet_conditions")) => {
                    let data = &module["data"];
                    if !data.is_null() {
                        facets.insert(id.to_string(), data.clone());
                    }
                }
                _ => {}
            }
        }
    }
    let facets = if facets.is_empty() {
        None
    } else {
        Some(Value::Object(facets))
    };
    (related, facets)
}

// ---------------------------------------------------------------------------
// Product-aggregated search (product/search/v1)
// ---------------------------------------------------------------------------

/// Parameters for the product-aggregated search
/// (`product/search/v1`, feature `productSearchInfo`). Results
/// are aggregated per product (across shops), so they carry a
/// product id hash and a price range instead of shop/item ids;
/// this route has no sort parameter. Live-verified in round 1
/// of the provider analysis (HTTP 200, count=1881).
#[derive(Debug, Clone)]
pub struct ProductSearchParams {
    pub keyword: String,
    /// 1-based page.
    pub page: i32,
    /// Page size (app default 30).
    pub hits: i32,
    pub genre_id: Option<i64>,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    pub product_code: Option<String>,
    /// A product id hash from a previous product search.
    pub product_id: Option<String>,
    /// Keyword search type (app default 0; sent only when != 0).
    pub keyword_search_type: i32,
    /// Include genre information (app default 0; sent only
    /// when != 0).
    pub genre_information_flag: i32,
}

impl Default for ProductSearchParams {
    fn default() -> Self {
        Self {
            keyword: String::new(),
            page: 1,
            hits: 30,
            genre_id: None,
            min_price: None,
            max_price: None,
            product_code: None,
            product_id: None,
            keyword_search_type: 0,
            genre_information_flag: 0,
        }
    }
}

/// Build the `product/search/v1` request body. The base key set
/// (keyword / page / hits / bffVersion) is the exact body
/// live-verified in round 1; optional keys are added only when
/// set. Pure; unit-tested.
pub fn build_product_search_body(params: &ProductSearchParams) -> Value {
    let mut p = Map::new();
    p.insert("keyword".into(), json!(params.keyword));
    p.insert("page".into(), json!(params.page.max(1)));
    p.insert("hits".into(), json!(params.hits.max(1)));
    if let Some(v) = params.genre_id {
        p.insert("genreId".into(), json!(v));
    }
    if let Some(v) = params.min_price {
        p.insert("minPrice".into(), json!(v));
    }
    if let Some(v) = params.max_price {
        p.insert("maxPrice".into(), json!(v));
    }
    if let Some(v) = &params.product_code {
        p.insert("productCode".into(), json!(v));
    }
    if let Some(v) = &params.product_id {
        p.insert("productId".into(), json!(v));
    }
    if params.keyword_search_type != 0 {
        p.insert(
            "keywordSearchType".into(),
            json!(params.keyword_search_type),
        );
    }
    if params.genre_information_flag != 0 {
        p.insert(
            "genreInformationFlag".into(),
            json!(params.genre_information_flag),
        );
    }
    p.insert("bffVersion".into(), json!("v1"));
    envelope("productSearchInfo", Value::Object(p))
}

/// Parse a `product/search/v1` response body into the result
/// JSON. Pure; unit-tested against the captured live response.
pub fn parse_product_search_response(body: &Value, keyword: &str) -> Value {
    let data = body.pointer("/productSearchInfo/data");
    let Some(data) = data.filter(|d| d.is_object()) else {
        return json!({
            "provider": "rakuten",
            "ok": false,
            "query": keyword,
            "error": "productSearchInfo feature returned no data",
        });
    };
    let products: Vec<Value> = data["products"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|p| {
                    json!({
                        "product_id": p["productId"].as_str(),
                        "product_code": p["productCode"].as_str(),
                        "ran_code": p["ranCode"].as_str(),
                        "name": p["productName"].as_str().unwrap_or_default(),
                        // Prices are served as floats (e.g. 990.0).
                        "min_price": p["minPrice"].as_f64().map(|f| f.round() as i64),
                        "max_price": p["maxPrice"].as_f64().map(|f| f.round() as i64),
                        "image": p["mediumImageUrl"].as_str()
                            .or_else(|| p["smallImageUrl"].as_str()),
                        "url": p["productUrlPC"].as_str()
                            .or_else(|| p["productUrlMobile"].as_str()),
                        "url_mobile": p["productUrlMobile"].as_str(),
                        "review_score": p["reviewAverage"].as_f64(),
                        "review_count": p["reviewCount"].as_i64(),
                        "genre_id": p["genreId"].as_i64(),
                        "genre_name": p["genreName"].as_str(),
                        "brand_name": p["brandName"].as_str(),
                        "maker_name": p["makerName"].as_str(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "provider": "rakuten",
        "ok": true,
        "query": keyword,
        "total": data["count"].as_i64(),
        "page": data["page"].as_i64(),
        "page_count": data["pageCount"].as_i64(),
        "products": products,
        "count": products.len(),
    })
}

// ---------------------------------------------------------------------------
// Product detail (itemscreen/get/v16)
// ---------------------------------------------------------------------------

/// How to address one product. The app resolves the first form
/// that is fully specified, in this order: shop_id + item_id,
/// item_code, shop_id + manage_number, item_url.
#[derive(Debug, Clone, Default)]
pub struct ProductRef {
    pub shop_id: Option<i64>,
    pub item_id: Option<i64>,
    /// `"shopCode:itemId"` form is split into shop + item by the app.
    pub item_code: Option<String>,
    pub manage_number: Option<String>,
    pub item_url: Option<String>,
    pub variant_id: Option<String>,
    pub postal_code: Option<String>,
    pub prefecture_id: Option<i64>,
}

/// Build the `itemscreen/get/v16` request body. Pure; unit-tested.
pub fn build_detail_body(pref_: &ProductRef) -> Result<Value> {
    let mut common = Map::new();
    if let (Some(shop), Some(item)) = (pref_.shop_id, pref_.item_id) {
        common.insert("shopId".into(), json!(shop));
        common.insert("itemId".into(), json!(item));
    } else if let Some(code) = &pref_.item_code {
        common.insert("itemCode".into(), json!(code));
    } else if let (Some(shop), Some(mn)) = (pref_.shop_id, &pref_.manage_number) {
        common.insert("shopId".into(), json!(shop));
        common.insert("manageNumber".into(), json!(mn));
    } else if let Some(url) = &pref_.item_url {
        common.insert("itemUrl".into(), json!(url));
    } else {
        return Err(anyhow!(
            "rakuten_get_product needs shop_id+item_id, item_code, shop_id+manage_number, or item_url"
        ));
    }
    if let Some(v) = &pref_.variant_id {
        common.insert("variantId".into(), json!(v));
    }
    common.insert("locale".into(), json!("ja-JP"));
    common.insert("deviceType".into(), json!("sp"));

    let mut item_info = Map::new();
    if let Some(pc) = &pref_.postal_code {
        item_info.insert("postalCode".into(), json!(pc));
    }
    if let Some(pf) = pref_.prefecture_id {
        item_info.insert("prefectureId".into(), json!(pf));
    }
    let mut shipping_info = Map::new();
    if let Some(pc) = &pref_.postal_code {
        shipping_info.insert("postalCode".into(), json!(pc));
    }
    if let Some(pf) = pref_.prefecture_id {
        shipping_info.insert("prefectureId".into(), json!(pf));
    }
    Ok(envelope_multi(
        &[
            ("itemInfo", Value::Object(item_info)),
            ("shopInfo", json!({ "hits": 20 })),
            ("shippingInfo", Value::Object(shipping_info)),
            ("disabledShopInfo", json!({ "isInflow": false })),
            ("topicsInfo", json!({ "hits": 4 })),
            ("consumptionTaxInfo", json!({})),
        ],
        Value::Object(common),
    ))
}

/// Normalize one entry of the detail response's `variants`
/// array (SKU id, attribute name/value pairs, prices, stock).
fn normalize_variant(v: &Value) -> Value {
    let attributes: Vec<Value> = v["attributes"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|a| {
                    json!({
                        "name": a["name"].as_str().unwrap_or_default(),
                        "values": a["values"],
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "sku_id": v["skuId"].as_str(),
        "attributes": attributes,
        "standard_price": v["standardPrice"].as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| v["standardPrice"].as_i64()),
        "tax_included_price": v["taxIncludedStandardPrice"]
            .as_f64().map(|f| f.round() as i64)
            .or_else(|| v["taxIncludedStandardPrice"].as_i64()),
        "quantity": v["quantity"].as_i64(),
        "article_number": v["articleNumber"]["value"].as_str(),
        "delivery_caption": v["deliveryCaption"].as_str().filter(|s| !s.is_empty()),
        "postage_included": v["shipping"]["postageIncluded"].as_bool(),
        "single_item_shipping": v["shipping"]["singleItemShipping"].as_i64(),
        // State the variant-details call needs (see
        // VariantDetailsParams): subscription eligibility and
        // the delivery-date template ids.
        "is_subscription_eligible": v["isSubscriptionEligible"].as_bool(),
        "normal_delivery_date_id": v["normalDeliveryDateId"].as_i64(),
        "back_order_delivery_date_id": v["backOrderDeliveryDateId"].as_i64(),
    })
}

/// Parse an `itemscreen/get/v16` response body. Success is judged
/// by the core `itemInfo` feature carrying data (the endpoint's
/// HTTP 207 partial-success form fails member-only features only).
/// Pure; unit-tested against the captured live response.
pub fn parse_detail_response(body: &Value) -> ItemResult {
    let data = body.pointer("/itemInfo/data");
    let Some(data) = data.filter(|d| d.is_object()) else {
        return ItemResult {
            provider: "rakuten".into(),
            ok: false,
            item: None,
            error: Some("itemInfo feature returned no data".into()),
        };
    };
    let variants: Vec<Value> = data
        .get("variants")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(normalize_variant).collect())
        .unwrap_or_default();
    let variant = data
        .get("variants")
        .and_then(Value::as_array)
        .and_then(|v| v.first())
        .cloned()
        .unwrap_or(Value::Null);
    let price = variant
        .get("taxIncludedStandardPrice")
        .and_then(|p| p.as_f64().map(|f| f.round() as i64).or_else(|| p.as_i64()))
        .or_else(|| {
            variant.get("standardPrice").and_then(|p| {
                p.as_i64()
                    .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
            })
        });
    let images: Vec<String> = data
        .get("images")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|i| i.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let shop = body
        .pointer("/shopInfo/data")
        .cloned()
        .unwrap_or(Value::Null);
    let shipping_methods: Vec<String> = body
        .pointer("/shippingInfo/data/standard/deliveryInfo")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let item_id = data.get("itemId").and_then(Value::as_i64);
    let description = data["productDescription"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| data["salesDescription"].as_str().filter(|s| !s.is_empty()))
        .map(str::to_string);
    ItemResult {
        provider: "rakuten".into(),
        ok: true,
        item: Some(Item {
            product_id: item_id.map(|i| i.to_string()).unwrap_or_default(),
            item_id: item_id.map(|i| i.to_string()),
            title: data["title"].as_str().unwrap_or_default().to_string(),
            url: data["itemUrl"].as_str().unwrap_or_default().to_string(),
            source: "rakuten".into(),
            price: price.map(Price::yen),
            availability: data["availability"].as_i64().map(|a| {
                if a > 0 {
                    "in_stock".into()
                } else {
                    "sold_out".into()
                }
            }),
            description,
            image: images.first().cloned(),
            images: if images.is_empty() {
                None
            } else {
                Some(images)
            },
            seller: Some(SellerInfo {
                id: shop["shopCode"].as_str().map(str::to_string),
                name: shop["shopName"].as_str().map(str::to_string),
                url: shop["shopURL"].as_str().map(str::to_string),
            }),
            review_score: data.pointer("/review/average").and_then(Value::as_f64),
            review_count: data.pointer("/review/count").and_then(Value::as_i64),
            stock_quantity: variant.get("quantity").and_then(Value::as_i64),
            shipping_methods: if shipping_methods.is_empty() {
                None
            } else {
                Some(shipping_methods)
            },
            tag_line: data["tagLine"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            variants: if variants.is_empty() {
                None
            } else {
                Some(variants)
            },
            ..Default::default()
        }),
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Variant details (variantdetails/get/v5)
// ---------------------------------------------------------------------------

/// Parameters for the per-variant details lookup
/// (`variantdetails/get/v5`), mirroring the app's
/// `VariantDetailParam` / `VariantDetailRequest`. Beyond the
/// identity fields, the app echoes item-screen state (prices,
/// genre, article number, inventory) into the feature params;
/// those are optional here and land exactly where the app
/// puts them.
#[derive(Debug, Clone)]
pub struct VariantDetailsParams {
    pub shop_id: i64,
    pub item_id: i64,
    pub variant_id: Option<String>,
    /// Always sent in the common params, as in the app (the
    /// server requires it: "isAvailableForSale request
    /// parameter is required!", measured live).
    pub is_available_for_sale: bool,
    pub is_social_gift: bool,
    pub purchasable_period: Option<Value>,
    pub prefecture_id: i64,
    pub postal_code: Option<String>,
    pub article_number: Option<String>,
    pub genre_id: Option<i64>,
    pub tax_rate: Option<Value>,
    pub standard_price: Option<i64>,
    pub tax_included_price: Option<i64>,
    pub inventory: Option<i64>,
    /// Required by the shippingInfo / pointsDetailsInfo
    /// features ("isSubscriptionEligible should be present!",
    /// measured live). Carried per-variant in the detail
    /// response.
    pub is_subscription_eligible: bool,
    /// One entry of the `variants` param of
    /// deliveryCaptionsInfo (the app's
    /// `DeliveryCaptionVariant`, built from the detail
    /// response's variant + shipping state: skuId, inventory,
    /// price, shopId, itemId, normal/backOrderDeliveryDateId,
    /// includedPostage, individualShipping, ...). Required by
    /// the server ("Missing required parameter: VARIANTS"),
    /// and sent as a one-element list: the server rejects a
    /// bare object with "variants parameters should be
    /// list!" (measured live 2026-11-08).
    pub delivery_caption_variant: Option<Value>,
}

impl Default for VariantDetailsParams {
    fn default() -> Self {
        Self {
            shop_id: 0,
            item_id: 0,
            variant_id: None,
            is_available_for_sale: true,
            is_social_gift: false,
            purchasable_period: None,
            prefecture_id: 0,
            postal_code: None,
            article_number: None,
            genre_id: None,
            tax_rate: None,
            standard_price: None,
            tax_included_price: None,
            inventory: None,
            is_subscription_eligible: false,
            delivery_caption_variant: None,
        }
    }
}

/// Build the `variantdetails/get/v5` request body with the
/// app's per-feature param projections. Pure; unit-tested.
pub fn build_variant_details_body(params: &VariantDetailsParams) -> Value {
    let mut common = Map::new();
    common.insert("shopId".into(), json!(params.shop_id));
    common.insert("itemId".into(), json!(params.item_id));
    common.insert("isSocialGift".into(), json!(params.is_social_gift));
    if let Some(v) = &params.variant_id {
        common.insert("variantId".into(), json!(v));
    }
    common.insert(
        "isAvailableForSale".into(),
        json!(params.is_available_for_sale),
    );
    if let Some(p) = &params.purchasable_period {
        common.insert("purchasablePeriod".into(), p.clone());
    }
    let mut shipping = Map::new();
    shipping.insert("prefectureId".into(), json!(params.prefecture_id));
    shipping.insert("shopId".into(), json!(params.shop_id));
    shipping.insert("itemId".into(), json!(params.item_id));
    if let Some(pc) = &params.postal_code {
        shipping.insert("postalCode".into(), json!(pc));
    }
    if let Some(v) = &params.variant_id {
        shipping.insert("variantId".into(), json!(v));
    }
    if let Some(p) = params.tax_included_price {
        shipping.insert("taxIncludedPrice".into(), json!(p));
    }
    if let Some(p) = params.standard_price {
        shipping.insert("standardPrice".into(), json!(p));
    }
    if let Some(i) = params.inventory {
        shipping.insert("inventory".into(), json!(i));
    }
    shipping.insert(
        "isSubscriptionEligible".into(),
        json!(params.is_subscription_eligible),
    );
    let mut maker = Map::new();
    if let Some(a) = &params.article_number {
        maker.insert("articleNumber".into(), json!(a));
    }
    let mut points = Map::new();
    if let Some(g) = params.genre_id {
        points.insert("genreId".into(), json!(g));
    }
    if let Some(t) = &params.tax_rate {
        points.insert("taxRate".into(), t.clone());
    }
    if let Some(p) = params.standard_price {
        points.insert("standardPrice".into(), json!(p));
    }
    points.insert(
        "isSubscriptionEligible".into(),
        json!(params.is_subscription_eligible),
    );
    let mut captions = Map::new();
    captions.insert("prefectureId".into(), json!(params.prefecture_id));
    if let Some(pc) = &params.postal_code {
        captions.insert("postalCode".into(), json!(pc));
    }
    if let Some(v) = &params.delivery_caption_variant {
        // The server requires a list ("variants parameters
        // should be list!", measured live 2026-11-08); the
        // selected variant goes in as the single element.
        let mut variant = v.clone();
        // The detail response carries singleItemShipping as
        // a number, but this request wants a boolean
        // ("individualShipping should be boolean!", measured
        // live 2026-11-08); coerce on the way in so values
        // carried over from `get_product` work as-is.
        if let Some(n) = variant.get("individualShipping").and_then(Value::as_i64) {
            variant["individualShipping"] = json!(n != 0);
        }
        // Each entry must carry variantId and price
        // ("variant must include both variantId and
        // price.", measured live 2026-11-08); fill them
        // from the params when the caller's object lacks
        // them.
        if variant.get("variantId").is_none() {
            if let Some(vid) = &params.variant_id {
                variant["variantId"] = json!(vid);
            }
        }
        if variant.get("price").is_none() {
            if let Some(p) = params.standard_price {
                variant["price"] = json!(p);
            }
        }
        captions.insert("variants".into(), json!([variant]));
    }
    let mut installment = Map::new();
    installment.insert("prefectureId".into(), json!(params.prefecture_id));
    if let Some(pc) = &params.postal_code {
        installment.insert("postalCode".into(), json!(pc));
    }
    if let Some(p) = params.standard_price {
        installment.insert("standardPrice".into(), json!(p));
    }
    envelope_multi(
        &[
            ("shippingInfo", Value::Object(shipping)),
            ("pointsDetailsInfo", Value::Object(points)),
            ("deliveryCaptionsInfo", Value::Object(captions)),
            ("installmentPaymentInfo", Value::Object(installment)),
            ("makerContentInfo", Value::Object(maker)),
        ],
        Value::Object(common),
    )
}

/// Parse a `variantdetails/get/v5` response body: each
/// feature's `data` is returned verbatim under a normalized
/// key. `ok` is true when at least one feature carried data.
/// Pure; unit-tested.
pub fn parse_variant_details_response(body: &Value, shop_id: i64, item_id: i64) -> Value {
    let feature = |name: &str| -> Value {
        body.pointer(&format!("/{name}/data"))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let shipping = feature("shippingInfo");
    let points = feature("pointsDetailsInfo");
    let captions = feature("deliveryCaptionsInfo");
    let installment = feature("installmentPaymentInfo");
    let maker = feature("makerContentInfo");
    let ok = [&shipping, &points, &captions, &installment, &maker]
        .iter()
        .any(|v| !v.is_null());
    json!({
        "provider": "rakuten",
        "ok": ok,
        "shop_id": shop_id,
        "item_id": item_id,
        "shipping_info": shipping,
        "points_details": points,
        "delivery_captions": captions,
        "installment_payment": installment,
        "maker_content": maker,
    })
}

// ---------------------------------------------------------------------------
// Reviews (review/list/v3)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ReviewParams {
    pub shop_id: i64,
    pub item_id: i64,
    pub page: i32,
    /// Page size for the review list (server cap 30).
    pub hits: i32,
    /// `-regtime` (newest) / `-evaluation` / `-judgment`.
    pub sort: String,
    pub check_display_condition: bool,
    /// 0 = male, 1 = female.
    pub sex: Option<i64>,
    pub min_evaluation: Option<i64>,
    pub max_evaluation: Option<i64>,
    pub min_age: Option<i64>,
    pub max_age: Option<i64>,
    /// 0 = all reviews, 1 = media-only.
    pub image_flag: i32,
}

impl Default for ReviewParams {
    fn default() -> Self {
        Self {
            shop_id: 0,
            item_id: 0,
            page: 1,
            hits: 10,
            sort: String::new(),
            check_display_condition: true,
            sex: None,
            min_evaluation: None,
            max_evaluation: None,
            min_age: None,
            max_age: None,
            image_flag: 0,
        }
    }
}

/// Build the `review/list/v3` request body. Pure; unit-tested.
pub fn build_reviews_body(params: &ReviewParams) -> Value {
    let mut common = Map::new();
    common.insert("shopId".into(), json!(params.shop_id));
    common.insert("itemId".into(), json!(params.item_id));
    if !params.sort.is_empty() {
        common.insert("sort".into(), json!(params.sort));
    }
    common.insert(
        "checkDisplayCondition".into(),
        json!(params.check_display_condition),
    );
    if let Some(v) = params.sex {
        common.insert("sex".into(), json!(v));
    }
    if let Some(v) = params.min_evaluation {
        common.insert("minEvaluation".into(), json!(v));
    }
    if let Some(v) = params.max_evaluation {
        common.insert("maxEvaluation".into(), json!(v));
    }
    if let Some(v) = params.min_age {
        common.insert("minAge".into(), json!(v));
    }
    if let Some(v) = params.max_age {
        common.insert("maxAge".into(), json!(v));
    }
    envelope_multi(
        &[
            (
                "reviewListInfo",
                json!({
                    "page": params.page.max(1),
                    "hits": params.hits.clamp(1, 30),
                    "imageFlag": params.image_flag,
                }),
            ),
            (
                "mediaReviewsInfo",
                json!({
                    "page": params.page.max(1),
                    "hits": params.hits.clamp(1, 20),
                    "imageFlag": 1,
                }),
            ),
            ("reviewAISummaryInfo", json!({})),
        ],
        Value::Object(common),
    )
}

fn normalize_review(r: &Value) -> Value {
    json!({
        "body": r["body"].as_str().unwrap_or_default(),
        "rating": r["userEvaluation"].as_str().map(str::to_string)
            .or_else(|| r["userEvaluation"].as_i64().map(|n| n.to_string())),
        "nickname": r["nickname"].as_str().unwrap_or_default(),
        "age": r["age"].as_str().unwrap_or_default(),
        "sex": r["sex"].as_str().unwrap_or_default(),
        "buy_flag": r["buyFlag"].as_i64(),
        "reg_time": r["regTime"].as_str().unwrap_or_default(),
        "judgement_ok_count": r["judgementOkCount"].as_i64().unwrap_or(0),
        "judgement_score": r["judgementScore"].as_i64(),
        "evaluation_details": r["evaluationDetails"],
        "usage": r["usage"].as_str().filter(|s| !s.is_empty()),
        "frequency": r["frequency"].as_str().filter(|s| !s.is_empty()),
        "theme": r["theme"].as_str().filter(|s| !s.is_empty()),
        "object": r["object"].as_str().filter(|s| !s.is_empty()),
        "shop_reply_flag": r["shopReplyFlag"].as_i64(),
    })
}

/// Parse a `review/list/v3` response body. Pure; unit-tested.
pub fn parse_reviews_response(body: &Value, shop_id: i64, item_id: i64) -> Value {
    let data = body.pointer("/reviewListInfo/data");
    let Some(data) = data.filter(|d| d.is_object()) else {
        return json!({
            "provider": "rakuten",
            "ok": false,
            "shop_id": shop_id,
            "item_id": item_id,
            "error": "reviewListInfo feature returned no data",
        });
    };
    let reviews: Vec<Value> = data
        .get("reviews")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(normalize_review).collect())
        .unwrap_or_default();
    let media: Vec<Value> = body
        .pointer("/mediaReviewsInfo/data/reviews")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().map(normalize_review).collect())
        .unwrap_or_default();
    let summary = &data["summary"];
    json!({
        "provider": "rakuten",
        "ok": true,
        "shop_id": shop_id,
        "item_id": item_id,
        "total": summary["total"].as_i64(),
        "page": summary["page"].as_i64(),
        "page_count": summary["pageCount"].as_i64(),
        "evaluation_totals": summary["evaluationTotals"],
        "reviews": reviews,
        "count": reviews.len(),
        "media_reviews": media,
        "media_total": body.pointer("/mediaReviewsInfo/data/summary/total").and_then(Value::as_i64),
        // The AI summary is requested on every call; return its
        // data verbatim (null when the feature carried none, an
        // empty object when the server had no summary, as in the
        // captured fixture).
        "ai_summary": body.pointer("/reviewAISummaryInfo/data").cloned().unwrap_or(Value::Null),
    })
}

// ---------------------------------------------------------------------------
// Genre tree (genre/search/v1)
// ---------------------------------------------------------------------------

/// Parameters for the genre tree lookup (`genre/search/v1`,
/// feature `genreSearchInfo`). The app sends the genre id as a
/// string and four include flags. Request shape recovered from
/// the app's `GenreSearchRequest` / `GenreSearchParam`.
#[derive(Debug, Clone)]
pub struct GenreSearchParams {
    pub genre_id: i64,
    pub include_children: bool,
    pub include_brother: bool,
    pub include_tag_group: bool,
    pub include_tag: bool,
}

impl Default for GenreSearchParams {
    fn default() -> Self {
        Self {
            genre_id: 0,
            include_children: true,
            include_brother: false,
            include_tag_group: false,
            include_tag: false,
        }
    }
}

/// Build the `genre/search/v1` request body. Pure; unit-tested.
/// The include flags go on the wire as the strings "0" / "1"
/// (the app's request serializer stringifies the param map):
/// the server rejects both JSON booleans and integers with
/// "Please select the value from [0, 1] for parameter:
/// includeChildFlg" (measured live).
pub fn build_genre_search_body(params: &GenreSearchParams) -> Value {
    envelope(
        "genreSearchInfo",
        json!({
            "genreId": params.genre_id.to_string(),
            "includeChildFlg": if params.include_children { "1" } else { "0" },
            "includeBrotherFlg": if params.include_brother { "1" } else { "0" },
            "includeTagGroupFlg": if params.include_tag_group { "1" } else { "0" },
            "includeTagFlg": if params.include_tag { "1" } else { "0" },
        }),
    )
}

fn normalize_genre_item(g: &Value) -> Value {
    json!({
        "genre_id": g["genreId"].as_i64(),
        "genre_name": g["genreName"].as_str().unwrap_or_default(),
        "genre_level": g["genreLevel"].as_i64(),
        "lowest_flg": g["lowestFlg"].as_i64(),
    })
}

fn normalize_genre_list(raw: Option<&Value>) -> Vec<Value> {
    raw.and_then(Value::as_array)
        .map(|arr| arr.iter().map(normalize_genre_item).collect())
        .unwrap_or_default()
}

/// Parse a `genre/search/v1` response body. Pure; unit-tested.
pub fn parse_genre_search_response(body: &Value) -> Value {
    let data = body.pointer("/genreSearchInfo/data");
    let Some(data) = data.filter(|d| d.is_object()) else {
        return json!({
            "provider": "rakuten",
            "ok": false,
            "error": "genreSearchInfo feature returned no data",
        });
    };
    json!({
        "provider": "rakuten",
        "ok": true,
        "current": data.get("current").map(normalize_genre_item),
        "parents": normalize_genre_list(data.get("parents")),
        "children": normalize_genre_list(data.get("children")),
        "brothers": normalize_genre_list(data.get("brothers")),
        "tag_groups": normalize_genre_list(data.get("tagGroups")),
    })
}

// ---------------------------------------------------------------------------
// Keyword suggestions (SUI autocomplete)
// ---------------------------------------------------------------------------

/// The app's suggest endpoint (ApiEndPoint.SEARCH_SUGGEST_PROD).
const SUGGEST_URL: &str = "https://rdc-api-catalog-gateway-api.rakuten.co.jp/SUI/autocomplete/app";

/// Build the suggest query parameters exactly as the app's
/// `SearchSuggestRequest.createRequest()` does: `v=1.0`,
/// `h=10` (hit count), `q`, `rid` (a per-request random id),
/// `acc=101` (fixed in `SearchSuggestionFragmentViewModel`),
/// `aid` (screen id, default 1). Pure; unit-tested.
pub fn build_suggest_params(keyword: &str, request_id: u64) -> Vec<(String, String)> {
    vec![
        ("v".into(), "1.0".into()),
        ("h".into(), "10".into()),
        ("q".into(), keyword.to_string()),
        ("rid".into(), request_id.to_string()),
        ("acc".into(), "101".into()),
        ("aid".into(), "1".into()),
    ]
}

/// Parse a suggest response (`{question, suggestions}`; each
/// suggestion has `type` / `name` / `data`). Pure; unit-tested.
pub fn parse_suggest_response(body: &Value, keyword: &str) -> Value {
    let suggestions: Vec<Value> = body["suggestions"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|s| {
                    let data = &s["data"];
                    let genre_src = if data.get("keyword").is_some() {
                        &data["keyword"]
                    } else {
                        &data["genre"]
                    };
                    json!({
                        "type": s["type"].as_str().unwrap_or_default(),
                        "name": s["name"].as_str().unwrap_or_default(),
                        "genre_id": genre_src["genre"].as_i64(),
                        "genre_name": genre_src["genreName"].as_str(),
                        "link": data.get("link").filter(|l| !l.is_null()).cloned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "provider": "rakuten",
        "ok": true,
        "query": keyword,
        "suggestions": suggestions,
        "count": suggestions.len(),
    })
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct RakutenClient {
    client: Client,
}

impl RakutenClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    async fn post_bff(&self, path: &str, body: &Value) -> Result<Value> {
        let resp = self
            .client
            .post(format!("{BASE}{path}"))
            .header("x-clientid", CLIENT_ID)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        // itemscreen / review answer 207 (partial success) as a
        // normal shape; treat it as success and let the parsers
        // judge by feature data.
        if !(status.is_success() || status.as_u16() == 207) {
            return Err(anyhow!("rakuten BFF {path} returned HTTP {status}"));
        }
        let parsed: Value = resp.json().await?;
        Ok(unwrap_body(parsed))
    }

    pub async fn search_products(&self, params: SearchParams) -> Result<SearchResult> {
        let body = build_search_body(&params);
        let resp = self.post_bff("dynamic/search/v1", &body).await?;
        let (mut items, total) = parse_search_response(&resp);
        let (related_keywords, facets) = parse_search_extras(&resp);
        let limit = if params.limit > 0 {
            params.limit as usize
        } else {
            items.len()
        };
        items.truncate(limit);
        Ok(SearchResult {
            provider: "rakuten".into(),
            ok: true,
            query: params.query.clone(),
            offset: params.offset,
            sort: if params.sort.is_empty() {
                "relevancy".into()
            } else {
                params.sort.clone()
            },
            count: items.len(),
            items,
            total_available: total,
            related_keywords,
            facets,
        })
    }

    /// Product-aggregated search (`product/search/v1`): one
    /// entry per product across shops, with a price range. The
    /// entries do not carry shop/item ids, so they cannot feed
    /// `get_product` / `get_reviews`.
    pub async fn search_products_aggregated(&self, params: ProductSearchParams) -> Result<Value> {
        let body = build_product_search_body(&params);
        let resp = self.post_bff("product/search/v1", &body).await?;
        Ok(parse_product_search_response(&resp, &params.keyword))
    }

    pub async fn get_product(&self, pref_: ProductRef) -> Result<ItemResult> {
        let body = build_detail_body(&pref_)?;
        let resp = self.post_bff("itemscreen/get/v16", &body).await?;
        Ok(parse_detail_response(&resp))
    }

    /// Per-variant details (`variantdetails/get/v5`): shipping,
    /// points, delivery captions, installment and maker content
    /// for one item (optionally one selected variant).
    pub async fn get_variant_details(&self, params: VariantDetailsParams) -> Result<Value> {
        let body = build_variant_details_body(&params);
        let resp = self.post_bff("variantdetails/get/v5", &body).await?;
        Ok(parse_variant_details_response(
            &resp,
            params.shop_id,
            params.item_id,
        ))
    }

    /// Genre tree lookup (`genre/search/v1`): the genre itself
    /// plus its parents / children / siblings / tag groups,
    /// according to the include flags.
    pub async fn get_genre_tree(&self, params: GenreSearchParams) -> Result<Value> {
        let body = build_genre_search_body(&params);
        let resp = self.post_bff("genre/search/v1", &body).await?;
        Ok(parse_genre_search_response(&resp))
    }

    /// Keyword suggestions from the app's SUI autocomplete
    /// endpoint (GET with the app's fixed parameter set).
    pub async fn suggest_keywords(&self, keyword: &str) -> Result<Value> {
        // The app generates rid as (Math.random() *
        // currentTimeMillis); a time-derived value serves the
        // same purpose.
        let rid = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let resp = self
            .client
            .get(SUGGEST_URL)
            .query(&build_suggest_params(keyword, rid))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow!("rakuten suggest returned HTTP {status}"));
        }
        let parsed: Value = resp.json().await?;
        Ok(parse_suggest_response(&parsed, keyword))
    }

    pub async fn get_reviews(&self, params: ReviewParams) -> Result<Value> {
        let body = build_reviews_body(&params);
        let resp = self.post_bff("review/list/v3", &body).await?;
        Ok(parse_reviews_response(
            &resp,
            params.shop_id,
            params.item_id,
        ))
    }
}

impl Default for RakutenClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let raw = match name {
            "dynamic" => include_str!("../tests/fixtures/r2-dynamic.json"),
            "detail" => include_str!("../tests/fixtures/r2-detail.json"),
            "review" => include_str!("../tests/fixtures/r2-review.json"),
            "product" => include_str!("../tests/fixtures/r1-product.json"),
            _ => unreachable!(),
        };
        let saved: Value = serde_json::from_str(raw).unwrap();
        saved["body"].clone()
    }

    #[test]
    fn unwrap_body_strips_gateway_wrapper() {
        // The fixtures are the raw gateway responses ({status,
        // body}); the client must parse the inner body.
        let raw = include_str!("../tests/fixtures/r2-dynamic.json");
        let saved: Value = serde_json::from_str(raw).unwrap();
        assert!(saved.get("dynamicSearchInfo").is_none());
        let body = unwrap_body(saved);
        let (items, total) = parse_search_response(&body);
        assert!(!items.is_empty());
        assert!(total.unwrap_or(0) > 0);
        // Unwrapped payloads pass through unchanged.
        assert_eq!(unwrap_body(json!({"a": 1})), json!({"a": 1}));
    }

    #[test]
    fn search_body_has_fixed_and_optional_params() {
        let body = build_search_body(&SearchParams {
            query: "コーヒー豆".into(),
            sort: "price_low_to_high_all".into(),
            price_min: Some(1000),
            price_max: Some(3000),
            condition: "new".into(),
            review_min: Some(4.0),
            ..Default::default()
        });
        let p = &body["features"]["dynamicSearchInfo"]["params"];
        assert_eq!(p["config"], json!("ichiba_search_android"));
        assert_eq!(p["lang"], json!("ja"));
        assert_eq!(p["v"], json!(4));
        assert_eq!(p["query"], json!("コーヒー豆"));
        assert_eq!(p["sort"], json!("price_low_to_high_all"));
        assert_eq!(p["priceMin"], json!(1000));
        assert_eq!(p["priceMax"], json!(3000));
        assert_eq!(p["condition"], json!("new"));
        assert_eq!(p["review"], json!(4.0));
        assert_eq!(
            p["modules"],
            json!("ichiba_search,dynamic_filter,facet_conditions")
        );
        assert!(p.get("category").is_none());
        assert_eq!(body["common"]["params"], json!({}));
    }

    #[test]
    fn search_body_omits_unset_price_band() {
        // Sending -1 for an unset band makes the live server
        // answer numFound=0 (measured 2026-11-07), so the keys
        // must be absent when unset.
        let body = build_search_body(&SearchParams {
            query: "x".into(),
            ..Default::default()
        });
        let p = &body["features"]["dynamicSearchInfo"]["params"];
        assert!(p.get("priceMin").is_none());
        assert!(p.get("priceMax").is_none());
        assert_eq!(p["sort"], json!("relevancy"));
        assert_eq!(p["offset"], json!(0));
    }

    #[test]
    fn parse_search_fixture() {
        let (items, total) = parse_search_response(&fixture("dynamic"));
        assert_eq!(total, Some(63486));
        assert_eq!(items.len(), 45);
        let first = &items[0];
        assert_eq!(first.shop_id, Some(355510));
        assert_eq!(first.item_id.as_deref(), Some("10000010"));
        assert_eq!(first.product_id, "355510_10000010");
        assert_eq!(first.price.as_ref().unwrap().value, 1000);
        assert_eq!(first.review_count, Some(11803));
        assert_eq!(first.review_score, Some(4.32));
        assert!(first.url.contains("item.rakuten.co.jp"));
        assert!(first.seller.as_ref().unwrap().name.is_some());
        assert!(first.image.is_some());
    }

    #[test]
    fn parse_search_fixture_extras_and_item_fields() {
        let body = fixture("dynamic");
        let (related, facets) = parse_search_extras(&body);
        let related = related.expect("related_keywords module must be returned");
        assert_eq!(related.len(), 10);
        assert!(related.iter().any(|k| k.contains("コーヒー豆")));
        // The captured response carries no dynamic_filter /
        // facet_conditions module, so facets stay None here.
        assert!(facets.is_none());

        let (items, _) = parse_search_response(&body);
        let first = &items[0];
        assert_eq!(
            first.subtitle.as_deref(),
            Some("【お買い物マラソン限定P2倍+α】")
        );
        let genres = first.genres.as_ref().expect("genres must be extracted");
        assert!(genres
            .iter()
            .any(|g| g.name == "ダイエット・健康" && g.id == 100938));
        assert!(genres.iter().all(|g| !g.name.is_empty()));
        let point = first.point.as_ref().expect("point must be extracted");
        assert_eq!(point["shopMultiplier"], json!(2));
        assert_eq!(
            first.delivery_estimate.as_deref(),
            Some("14時までの注文で当日出荷(対象地域のみ)")
        );
    }

    #[test]
    fn product_search_body_shape() {
        let body = build_product_search_body(&ProductSearchParams {
            keyword: "コーヒー豆".into(),
            page: 1,
            hits: 10,
            ..Default::default()
        });
        // Exactly the key set live-verified in round 1.
        assert_eq!(
            body["features"]["productSearchInfo"]["params"],
            json!({"keyword": "コーヒー豆", "page": 1, "hits": 10, "bffVersion": "v1"})
        );
        let body = build_product_search_body(&ProductSearchParams {
            keyword: "x".into(),
            genre_id: Some(100938),
            min_price: Some(500),
            product_code: Some("PC1".into()),
            genre_information_flag: 1,
            ..Default::default()
        });
        let p = &body["features"]["productSearchInfo"]["params"];
        assert_eq!(p["genreId"], json!(100938));
        assert_eq!(p["minPrice"], json!(500));
        assert_eq!(p["productCode"], json!("PC1"));
        assert_eq!(p["genreInformationFlag"], json!(1));
        assert!(p.get("maxPrice").is_none());
        assert!(p.get("keywordSearchType").is_none());
    }

    #[test]
    fn parse_product_search_fixture() {
        // Real response captured live for this fix (keyword
        // コーヒー豆, page 1, hits 10).
        let out = parse_product_search_response(&fixture("product"), "コーヒー豆");
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["total"], json!(1873));
        assert_eq!(out["count"], json!(10));
        let first = &out["products"][0];
        assert!(first["name"].as_str().unwrap().contains("コーヒー"));
        assert_eq!(first["min_price"], json!(990));
        assert_eq!(first["max_price"], json!(4570));
        assert!(first["product_id"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(first["genre_name"], json!("コーヒー豆"));
    }

    #[test]
    fn detail_body_prefers_shop_and_item() {
        let body = build_detail_body(&ProductRef {
            shop_id: Some(355510),
            item_id: Some(10000010),
            item_code: Some("ignored:1".into()),
            ..Default::default()
        })
        .unwrap();
        let c = &body["common"]["params"];
        assert_eq!(c["shopId"], json!(355510));
        assert_eq!(c["itemId"], json!(10000010));
        assert_eq!(c["locale"], json!("ja-JP"));
        assert_eq!(c["deviceType"], json!("sp"));
        assert!(body["features"].get("itemInfo").is_some());
        assert!(body["features"].get("shopInfo").is_some());
        assert!(body["features"].get("shippingInfo").is_some());
    }

    #[test]
    fn detail_body_falls_back_to_item_code() {
        let body = build_detail_body(&ProductRef {
            item_code: Some("kyowashokken:10000010".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            body["common"]["params"]["itemCode"],
            json!("kyowashokken:10000010")
        );
    }

    #[test]
    fn detail_body_requires_an_identifier() {
        assert!(build_detail_body(&ProductRef::default()).is_err());
    }

    #[test]
    fn parse_detail_fixture() {
        let result = parse_detail_response(&fixture("detail"));
        assert!(result.ok, "detail must parse as ok: {result:?}");
        let item = result.item.unwrap();
        assert!(item.title.contains("麹とコンブチャ"));
        assert_eq!(item.price.as_ref().unwrap().value, 1000);
        assert_eq!(item.review_count, Some(11803));
        assert_eq!(item.review_score, Some(4.32));
        assert_eq!(item.stock_quantity, Some(1415));
        assert_eq!(
            item.seller.as_ref().unwrap().name.as_deref(),
            Some("協和食研 楽天市場店")
        );
        assert!(item.images.as_ref().unwrap().len() >= 4);
        assert!(item.shipping_methods.as_ref().unwrap().len() >= 2);
        // tagLine and the full variant list are returned too.
        assert_eq!(
            item.tag_line.as_deref(),
            Some("【お買い物マラソン限定P2倍+α】<br>")
        );
        let variants = item.variants.as_ref().expect("variants must be returned");
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0]["sku_id"], json!("zk-uzfv-8xax"));
        assert_eq!(variants[0]["standard_price"], json!(1000));
        assert_eq!(variants[0]["tax_included_price"], json!(1000));
        assert_eq!(variants[0]["quantity"], json!(1415));
        assert_eq!(variants[0]["article_number"], json!("4573106310114"));
        let attrs = variants[0]["attributes"].as_array().unwrap();
        assert!(attrs.iter().any(|a| a["name"] == json!("カラー")));
    }

    #[test]
    fn reviews_body_shape() {
        let body = build_reviews_body(&ReviewParams {
            shop_id: 355510,
            item_id: 10000010,
            sort: "-regtime".into(),
            min_evaluation: Some(4),
            ..Default::default()
        });
        let c = &body["common"]["params"];
        assert_eq!(c["shopId"], json!(355510));
        assert_eq!(c["itemId"], json!(10000010));
        assert_eq!(c["sort"], json!("-regtime"));
        assert_eq!(c["checkDisplayCondition"], json!(true));
        assert_eq!(c["minEvaluation"], json!(4));
        assert!(c.get("sex").is_none());
        let f = &body["features"];
        assert_eq!(f["reviewListInfo"]["params"]["hits"], json!(10));
        assert_eq!(f["mediaReviewsInfo"]["params"]["imageFlag"], json!(1));
    }

    #[test]
    fn parse_reviews_fixture() {
        let out = parse_reviews_response(&fixture("review"), 355510, 10000010);
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["total"], json!(11803));
        assert_eq!(out["count"], json!(10));
        let reviews = out["reviews"].as_array().unwrap();
        assert_eq!(reviews[0]["rating"], json!("5"));
        assert!(reviews[0]["body"].as_str().unwrap().contains("飲んでます"));
        assert_eq!(out["media_total"], json!(63));
        // Fields the response carries beyond the original set.
        assert_eq!(reviews[0]["usage"], json!("自分用"));
        assert_eq!(reviews[0]["frequency"], json!("リピート"));
        assert_eq!(reviews[0]["theme"], json!("リピート"));
        assert_eq!(reviews[0]["object"], json!("実用品・普段使い"));
        assert_eq!(reviews[0]["judgement_score"], json!(0));
        assert_eq!(reviews[0]["shop_reply_flag"], json!(0));
        // The AI summary feature is requested on every call; the
        // fixture's data is the empty object the server returned.
        assert_eq!(out["ai_summary"], json!({}));
    }

    #[test]
    fn genre_search_body_and_parse() {
        let body = build_genre_search_body(&GenreSearchParams {
            genre_id: 100938,
            ..Default::default()
        });
        assert_eq!(
            body["features"]["genreSearchInfo"]["params"],
            json!({
                "genreId": "100938",
                "includeChildFlg": "1",
                "includeBrotherFlg": "0",
                "includeTagGroupFlg": "0",
                "includeTagFlg": "0",
            })
        );
        // Parse against the real response captured live for
        // this fix (genre 100938 with children).
        let raw: Value =
            serde_json::from_str(include_str!("../tests/fixtures/r1-genre.json")).unwrap();
        let out = parse_genre_search_response(&raw["body"]);
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["current"]["genre_name"], json!("ダイエット・健康"));
        assert_eq!(out["current"]["genre_id"], json!(100938));
        let children = out["children"].as_array().unwrap();
        assert_eq!(children.len(), 8);
        assert!(children.iter().any(|c| c["genre_id"] == json!(563727)));
    }

    #[test]
    fn variant_details_body_and_parse() {
        let body = build_variant_details_body(&VariantDetailsParams {
            shop_id: 355510,
            item_id: 10000010,
            variant_id: Some("zk-uzfv-8xax".into()),
            postal_code: Some("1000001".into()),
            delivery_caption_variant: Some(json!({
                "skuId": "zk-uzfv-8xax",
                "individualShipping": 1,
            })),
            ..Default::default()
        });
        let c = &body["common"]["params"];
        assert_eq!(c["shopId"], json!(355510));
        assert_eq!(c["itemId"], json!(10000010));
        assert_eq!(c["isSocialGift"], json!(false));
        assert_eq!(c["variantId"], json!("zk-uzfv-8xax"));
        // Always sent by the app; required by the server.
        assert_eq!(c["isAvailableForSale"], json!(true));
        let f = &body["features"];
        assert_eq!(f["shippingInfo"]["params"]["postalCode"], json!("1000001"));
        assert_eq!(f["shippingInfo"]["params"]["shopId"], json!(355510));
        assert_eq!(
            f["shippingInfo"]["params"]["variantId"],
            json!("zk-uzfv-8xax")
        );
        assert_eq!(
            f["deliveryCaptionsInfo"]["params"]["prefectureId"],
            json!(0)
        );
        // The server requires `variants` to be a list
        // ("variants parameters should be list!"), the
        // numeric individualShipping carried over from the
        // detail response is coerced to a boolean, and a
        // missing variantId is filled from the params.
        assert_eq!(
            f["deliveryCaptionsInfo"]["params"]["variants"],
            json!([{
                "skuId": "zk-uzfv-8xax",
                "individualShipping": true,
                "variantId": "zk-uzfv-8xax",
            }])
        );
        assert!(f.get("pointsDetailsInfo").is_some());
        assert!(f.get("deliveryCaptionsInfo").is_some());
        assert!(f.get("installmentPaymentInfo").is_some());
        assert!(f.get("makerContentInfo").is_some());

        let resp = json!({
            "shippingInfo": {"data": {"standard": {}}},
            "pointsDetailsInfo": {"data": null},
            "deliveryCaptionsInfo": {"error": {"message": "x"}},
            "installmentPaymentInfo": {"data": null},
            "makerContentInfo": {"data": null},
        });
        let out = parse_variant_details_response(&resp, 355510, 10000010);
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["shipping_info"], json!({"standard": {}}));
        assert_eq!(out["points_details"], Value::Null);
        let empty = parse_variant_details_response(&json!({}), 1, 2);
        assert_eq!(empty["ok"], json!(false));
    }

    #[test]
    fn suggest_params_and_parse() {
        let params = build_suggest_params("コーヒー", 12345);
        let map: std::collections::HashMap<_, _> = params.into_iter().collect();
        assert_eq!(map["v"], "1.0");
        assert_eq!(map["h"], "10");
        assert_eq!(map["q"], "コーヒー");
        assert_eq!(map["rid"], "12345");
        assert_eq!(map["acc"], "101");
        assert_eq!(map["aid"], "1");
        // Response shape per SearchSuggestResponse /
        // SearchSuggestion ({type, name, data}).
        let resp = json!({
            "question": {"q": "コーヒー"},
            "suggestions": [
                {"type": "keyword", "name": "コーヒー豆", "data": {"keyword": {"genre": 100938, "genreName": "食品"}}},
                {"type": "genre", "name": "飲料", "data": {"genre": {"genre": 100316, "genreName": "飲料"}}},
            ],
        });
        let out = parse_suggest_response(&resp, "コーヒー");
        assert_eq!(out["ok"], json!(true));
        assert_eq!(out["count"], json!(2));
        assert_eq!(out["suggestions"][0]["name"], json!("コーヒー豆"));
        assert_eq!(out["suggestions"][0]["genre_id"], json!(100938));
        assert_eq!(out["suggestions"][1]["genre_id"], json!(100316));
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search() {
        let client = RakutenClient::new();
        let result = client
            .search_products(SearchParams {
                query: "コーヒー豆".into(),
                sort: "price_low_to_high_all".into(),
                price_min: Some(1000),
                price_max: Some(3000),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        assert!(!result.items.is_empty());
        for item in &result.items {
            assert!(item.price.as_ref().unwrap().value >= 1000);
            assert!(item.price.as_ref().unwrap().value <= 3000);
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_product_search() {
        let client = RakutenClient::new();
        let out = client
            .search_products_aggregated(ProductSearchParams {
                keyword: "コーヒー豆".into(),
                page: 1,
                hits: 10,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
        assert!(out["total"].as_i64().unwrap() > 0);
        assert_eq!(out["count"], json!(10));
        assert!(out["products"][0]["product_id"].as_str().is_some());
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_sorts() {
        // Verifies the seven allValue sorts that shipped with
        // static-only confirmation, plus one highRelevancyValue
        // form. Sends are serial with pauses.
        let client = RakutenClient::new();
        let cases: &[(&str, &str)] = &[
            ("price_high_to_low_all", "price_desc"),
            ("newest", "any"),
            ("review_count_all", "count_desc"),
            ("review_score_all", "score_desc"),
            ("price_plus_shippingfee_low_to_high_all", "total_asc"),
            ("price_plus_shippingfee_high_to_low_all", "total_desc"),
            ("price_low_to_high", "price_asc"),
        ];
        for (sort, mode) in cases {
            let result = client
                .search_products(SearchParams {
                    query: "コーヒー豆".into(),
                    sort: sort.to_string(),
                    limit: 10,
                    ..Default::default()
                })
                .await
                .unwrap();
            assert!(result.ok, "sort {sort}");
            assert!(result.items.len() >= 5, "sort {sort}: too few items");
            let prices: Vec<i64> = result
                .items
                .iter()
                .filter_map(|i| i.price.as_ref().map(|p| p.value))
                .collect();
            let totals: Vec<i64> = result
                .items
                .iter()
                .filter_map(|i| {
                    i.price
                        .as_ref()
                        .map(|p| p.value + i.shipping_price.unwrap_or(0))
                })
                .collect();
            let counts: Vec<i64> = result.items.iter().filter_map(|i| i.review_count).collect();
            let scores: Vec<f64> = result.items.iter().filter_map(|i| i.review_score).collect();
            let asc = |v: &[i64]| v.windows(2).all(|w| w[0] <= w[1]);
            let desc = |v: &[i64]| v.windows(2).all(|w| w[0] >= w[1]);
            match *mode {
                "price_desc" => assert!(desc(&prices), "sort {sort} not descending"),
                "price_asc" => assert!(asc(&prices), "sort {sort} not ascending"),
                "total_asc" => assert!(asc(&totals), "sort {sort} not ascending"),
                "total_desc" => assert!(desc(&totals), "sort {sort} not descending"),
                "count_desc" => assert!(desc(&counts), "sort {sort} not descending"),
                "score_desc" => assert!(
                    scores.windows(2).all(|w| w[0] >= w[1]),
                    "sort {sort} not descending"
                ),
                _ => {}
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_review_filters() {
        let client = RakutenClient::new();
        // sort + sex + evaluation range + age range in one call.
        let out = client
            .get_reviews(ReviewParams {
                shop_id: 355510,
                item_id: 10000010,
                sort: "-evaluation".into(),
                sex: Some(1),
                min_evaluation: Some(4),
                min_age: Some(30),
                max_age: Some(49),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
        assert!(out["count"].as_u64().unwrap() > 0);
        for r in out["reviews"].as_array().unwrap() {
            let rating: i64 = r["rating"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            assert!(rating >= 4, "filter min_evaluation violated: {r}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        // sort=-judgment with media-only flag.
        let out = client
            .get_reviews(ReviewParams {
                shop_id: 355510,
                item_id: 10000010,
                sort: "-judgment".into(),
                image_flag: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        // sort=-regtime alone.
        let out = client
            .get_reviews(ReviewParams {
                shop_id: 355510,
                item_id: 10000010,
                sort: "-regtime".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
        assert!(out["count"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_suggest_genre_variant() {
        let client = RakutenClient::new();
        let s = client.suggest_keywords("コーヒー").await.unwrap();
        assert_eq!(s["ok"], json!(true), "{s}");
        assert!(s["count"].as_u64().unwrap() > 0);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let g = client
            .get_genre_tree(GenreSearchParams {
                genre_id: 100938,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(g["ok"], json!(true), "{g}");
        assert_eq!(g["current"]["genre_name"], json!("ダイエット・健康"));
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        // Variant details needs the item-screen state the app
        // carries over (values from the r2-detail fixture item).
        let v = client
            .get_variant_details(VariantDetailsParams {
                shop_id: 355510,
                item_id: 10000010,
                variant_id: Some("zk-uzfv-8xax".into()),
                article_number: Some("4573106310114".into()),
                standard_price: Some(1000),
                tax_included_price: Some(1000),
                inventory: Some(1415),
                is_subscription_eligible: false,
                delivery_caption_variant: Some(json!({
                    "skuId": "zk-uzfv-8xax",
                    "inventory": 1415,
                    "price": 1000,
                    "shopId": 355510,
                    "itemId": 10000010,
                    "normalDeliveryDateId": 1,
                    "backOrderDeliveryDateId": 3,
                    "includedPostage": false,
                    "individualShipping": 0,
                })),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(v["ok"], json!(true), "{v}");
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_detail_and_reviews() {
        let client = RakutenClient::new();
        let detail = client
            .get_product(ProductRef {
                shop_id: Some(355510),
                item_id: Some(10000010),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(detail.ok, "{detail:?}");
        let reviews = client
            .get_reviews(ReviewParams {
                shop_id: 355510,
                item_id: 10000010,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(reviews["ok"], json!(true));
        assert!(reviews["total"].as_i64().unwrap() > 0);
    }
}
