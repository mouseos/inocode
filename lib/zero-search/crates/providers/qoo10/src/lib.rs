//! Qoo10 (net.giosis.shopping.jp) provider.
//!
//! Search reproduces the app's native qapi RPC
//! (`GetMoveSearchData` on `mapi-move.qoo10.jp`): anonymous, the
//! only credential is the app's built-in `GiosisCertificationKey`
//! constant plus a per-request `qtid`.
//!
//! Product detail has no native method in the app: tapping a
//! result opens the search result's `CONNECT_URL` (with
//! `&AppMovePage` appended) in the in-app WebView. This provider
//! reproduces that WebView's request conditions (WebView UA with
//! the app suffix, `X-Requested-With`, no cookies) and extracts
//! the product data from the returned HTML.
//!
//! Caveat (also stated in the tool descriptions): the app suffix
//! contains a `GMKTV3_` token that the app builds by encrypting
//! on-device values; it cannot be reproduced from the APK. A
//! format-preserving placeholder is sent instead, which the
//! service accepted in live verification (2026-11-07). If the
//! edge ever starts validating that token, the detail route
//! would close.
//!
//! Note: Qoo10's service is region-bound to Japan-side access;
//! from non-Japan egress the API host answers with an error page
//! instead of JSON.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::sync::LazyLock;
use uuid::Uuid;

const API_BASE: &str =
    "https://mapi-move.qoo10.jp/GMKT.INC.Front.Mobile.ShoppingApiService/ShoppingApp.qapi";
const CERT_KEY: &str = "BhiQeRsAhG0PigAgA9zK7Wt3gxnyyVP2";
const PACKAGE: &str = "net.giosis.shopping.jp";

/// Sort values for `qoo10_search_products` (the app's `sort` int).
pub const SORTS: &[(i32, &str)] = &[
    (0, "ranking"),
    (1, "best selling"),
    (2, "newest"),
    (3, "price ascending"),
    (4, "price descending"),
    (5, "review score"),
    (6, "review count"),
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
    pub product_id: String,
    pub title: String,
    pub url: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub availability: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
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
    pub sort: i32,
    pub items: Vec<Item>,
    pub count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_available: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_pages: Option<i64>,
    /// True when the keyword itself matched nothing and the items
    /// are the server's `noSearchItems` alternatives.
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub suggested_items: bool,
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
// User-Agent construction (app format)
// ---------------------------------------------------------------------------

/// The suffix the app appends to its WebView UA and also uses as
/// the API UA core: `{deviceType}_{appName}_{versionName}_
/// {versionCode}(GMKTV3_<token>;<model>;<release>;<lang>_<country>
/// ;<affiliateCd>)`. The GMKTV3 token is an AES/GCM ciphertext of
/// on-device values and cannot be reproduced; a format-preserving
/// random placeholder is generated per client.
fn app_suffix(token: &str) -> String {
    format!("Android_Gmarket Qoo10 JP_5.6.5_389(GMKTV3_{token};Pixel 8;14;ja_JP;2000104319)")
}

fn placeholder_token() -> String {
    let mut bytes = Vec::with_capacity(32);
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    bytes.extend_from_slice(Uuid::new_v4().as_bytes());
    URL_SAFE_NO_PAD.encode(bytes)
}

// ---------------------------------------------------------------------------
// Search (GetMoveSearchData)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SearchParams {
    pub query: String,
    /// `page_no`, 1-based.
    pub page: i32,
    /// `page_size` (app default 30).
    pub limit: i32,
    /// See [`SORTS`].
    pub sort: i32,
    pub search_sub_keyword: String,
    /// Brand codes (distributed by the app's filter info).
    pub brand: Vec<String>,
    pub category: String,
    /// `"Y"` for free-shipping only, empty for no constraint.
    pub free_shipping: String,
    pub min_price: String,
    pub max_price: String,
    pub official_shop: String,
    /// `"Y"` / `"N"`.
    pub promotion_yn: String,
    /// Shipping-origin codes (filter-info distributed).
    pub shipping_from: Vec<String>,
    /// Review-score lower bound code (filter-info distributed).
    pub review_score: String,
    /// Color codes (attribute filter info distributed).
    pub color: Vec<String>,
    /// Attribute codes (attribute filter info distributed).
    pub attribute: Vec<String>,
    pub coupon_label: String,
    pub keyword_type: String,
    /// Seller number (in-shop search).
    pub sell_cust_no: String,
    /// `"N"` is the app's set value.
    pub nosearch_yn: String,
    pub page_type: String,
    pub filter_type: String,
    pub seller_promotion_no: String,
    pub only_total_count: String,
    pub last_section: String,
    pub related_bg_color: String,
}

impl Default for SearchParams {
    fn default() -> Self {
        Self {
            query: String::new(),
            page: 1,
            limit: 30,
            sort: 0,
            search_sub_keyword: String::new(),
            brand: Vec::new(),
            category: String::new(),
            free_shipping: String::new(),
            min_price: String::new(),
            max_price: String::new(),
            official_shop: String::new(),
            promotion_yn: String::new(),
            shipping_from: Vec::new(),
            review_score: String::new(),
            color: Vec::new(),
            attribute: Vec::new(),
            coupon_label: String::new(),
            keyword_type: String::new(),
            sell_cust_no: String::new(),
            nosearch_yn: "N".into(),
            page_type: String::new(),
            filter_type: String::new(),
            seller_promotion_no: String::new(),
            only_total_count: String::new(),
            last_section: String::new(),
            related_bg_color: String::new(),
        }
    }
}

/// Build the `GetMoveSearchData` request body: all 23
/// `MoveSearchValueData` fields, the way the app's Gson
/// serialization emits them. Pure; unit-tested.
pub fn build_search_body(params: &SearchParams) -> Value {
    let mut v = Map::new();
    v.insert("search_keyword".into(), json!(params.query));
    v.insert("search_subKeyword".into(), json!(params.search_sub_keyword));
    v.insert("brand".into(), json!(params.brand));
    v.insert("category".into(), json!(params.category));
    v.insert("free_shipping".into(), json!(params.free_shipping));
    v.insert("min_price".into(), json!(params.min_price));
    v.insert("max_price".into(), json!(params.max_price));
    v.insert("official_shop".into(), json!(params.official_shop));
    v.insert("promotion_yn".into(), json!(params.promotion_yn));
    v.insert("shipping_from".into(), json!(params.shipping_from));
    v.insert("review_score".into(), json!(params.review_score));
    v.insert("color".into(), json!(params.color));
    v.insert("attribute".into(), json!(params.attribute));
    v.insert("coupon_label".into(), json!(params.coupon_label));
    v.insert("keyword_type".into(), json!(params.keyword_type));
    v.insert("sell_cust_no".into(), json!(params.sell_cust_no));
    v.insert("nosearch_yn".into(), json!(params.nosearch_yn));
    v.insert("page_type".into(), json!(params.page_type));
    v.insert("filter_type".into(), json!(params.filter_type));
    v.insert(
        "seller_promotion_no".into(),
        json!(params.seller_promotion_no),
    );
    v.insert("only_total_count".into(), json!(params.only_total_count));
    v.insert("last_section".into(), json!(params.last_section));
    v.insert("related_bg_color".into(), json!(params.related_bg_color));
    json!({
        "page_no": params.page.max(1),
        "page_size": params.limit.max(1),
        "sort": params.sort,
        "value": Value::Object(v),
    })
}

fn parse_goods(g: &Value) -> Item {
    let price = g
        .get("FINAL_PRICE")
        .and_then(|p| p.as_f64().map(|f| f.round() as i64).or_else(|| p.as_i64()));
    Item {
        product_id: g["GD_NO"]
            .as_str()
            .map(str::to_string)
            .or_else(|| g["GD_NO"].as_i64().map(|n| n.to_string()))
            .unwrap_or_default(),
        title: g["TITLE"].as_str().unwrap_or_default().to_string(),
        // CONNECT_URL is kept verbatim, tracking parameters included.
        url: g["CONNECT_URL"].as_str().unwrap_or_default().to_string(),
        source: "qoo10".into(),
        price: price.map(Price::yen),
        availability: match g["SOLDOUT_YN"].as_str() {
            Some("Y") => Some("sold_out".into()),
            Some(_) => Some("in_stock".into()),
            None => None,
        },
        image: g["IMAGE_URL"].as_str().map(str::to_string),
        review_score: g["REVIEW_POINT"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .or_else(|| g["REVIEW_POINT"].as_f64()),
        review_count: g["REVIEW_COUNT"].as_i64(),
        ..Default::default()
    }
}

/// Parse a `GetMoveSearchData` response. Pure; unit-tested
/// against the captured live responses.
/// Normalize a `GetMoveSearchFilterInfo` response (`{d:
/// MoveFilterData}`) into the filter-code listing the tool returns.
/// Shapes follow the live capture of 2026-11-08 (analysis round 2):
/// brand/price/color/shippingFrom/review carry `items` arrays,
/// category is the lc/mc/sc tree, attributeDetail carries the
/// attribute groups, and coupon/freeShipping are `{id, title}`
/// toggles whose ids are the search field names.
pub fn parse_search_filters_response(body: &Value, keyword: &str) -> Value {
    let d = &body["d"];
    let items_of = |v: &Value| -> Vec<Value> { v["items"].as_array().cloned().unwrap_or_default() };
    let brand_items = {
        let b = items_of(&d["brand"]);
        if b.is_empty() {
            items_of(&d["itemBrandFilter"])
        } else {
            b
        }
    };
    let brands: Vec<Value> = brand_items
        .iter()
        .map(|b| json!({"name": b["name"], "code": b["code"]}))
        .collect();
    let price_ranges: Vec<Value> = items_of(&d["price"])
        .iter()
        .map(|p| {
            json!({
                "label": p["name"],
                "min_price": p["minPrice"],
                "max_price": p["maxPrice"],
            })
        })
        .collect();
    let colors: Vec<Value> = items_of(&d["color"])
        .iter()
        .map(|c| json!({"code": c["code"], "hex": c["hexColor"]}))
        .collect();
    let shipping_from: Vec<Value> = items_of(&d["shippingFrom"])
        .iter()
        .map(|s| json!({"name": s["name"], "code": s["code"]}))
        .collect();
    let review_scores: Vec<Value> = items_of(&d["review"])
        .iter()
        .map(|r| json!({"label": r["name"], "value": r["value"]}))
        .collect();
    let attributes: Vec<Value> = items_of(&d["attributeDetail"])
        .iter()
        .map(|group| {
            let options: Vec<Value> = group["attributes"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .map(|a| {
                    let details: Vec<Value> = a["details"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .iter()
                        .map(|det| json!({"name": det["name"], "value": det["value"]}))
                        .collect();
                    json!({"name": a["name"], "value": a["value"], "details": details})
                })
                .collect();
            json!({"name": group["name"], "attributes": options})
        })
        .collect();
    let mut toggles: Vec<String> = Vec::new();
    for key in ["coupon", "freeShipping"] {
        if let Some(id) = d[key]["id"].as_str() {
            toggles.push(id.to_string());
        }
    }
    json!({
        "provider": "qoo10",
        "ok": true,
        "query": keyword,
        "brands": brands,
        "price_ranges": price_ranges,
        "colors": colors,
        "categories": {
            "lc": d["category"]["lc"],
            "mc": d["category"]["mc"],
            "sc": d["category"]["sc"],
        },
        "shipping_from": shipping_from,
        "review_scores": review_scores,
        "attributes": attributes,
        "toggles": toggles,
    })
}

pub fn parse_search_response(body: &Value) -> (Vec<Item>, Option<i64>, Option<i64>, bool) {
    let d = &body["d"];
    let total = d["totalCount"].as_i64();
    let total_pages = d["totalPage"].as_i64();
    let mut items = Vec::new();
    if let Some(sections) = d["sections"].as_array() {
        for section in sections {
            if let Some(goods) = section["goods"].as_array() {
                items.extend(goods.iter().map(parse_goods));
            }
        }
    }
    let mut suggested = false;
    if items.is_empty() {
        if let Some(alt) = d["noSearchItems"].as_array() {
            for section in alt {
                if let Some(goods) = section["goods"].as_array() {
                    items.extend(goods.iter().map(parse_goods));
                }
            }
            suggested = !items.is_empty();
        }
    }
    (items, total, total_pages, suggested)
}

/// The detail URL the app's WebView opens for a goods code: the
/// search result's CONNECT_URL form with `&AppMovePage` appended.
pub fn build_detail_url(gd_no: &str) -> String {
    format!(
        "https://m.qoo10.jp/gmkt.inc/Mobile/goods/move/movegoods.aspx?goodscode={gd_no}&ref_move_yn=Y&AppMovePage"
    )
}

/// Extract a goods code from a CONNECT_URL / detail URL, or pass
/// a bare goods code through.
pub fn extract_gd_no(product: &str) -> String {
    let s = product.trim();
    if let Some(idx) = s.find("goodscode=") {
        let rest = &s[idx + "goodscode=".len()..];
        let code: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !code.is_empty() {
            return code;
        }
    }
    s.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(s)
        .to_string()
}

// ---------------------------------------------------------------------------
// Detail page (WebView reproduction)
// ---------------------------------------------------------------------------

static GD_NM_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"var gd_nm='([^']*)'").unwrap());
static MIN_SELL_PRICE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"var min_sell_price=([0-9]+)").unwrap());
static SHARE_PRICE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<dt>商品金額</dt>\s*<dd>([0-9,]+)円</dd>").unwrap());
static OG_IMAGE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"property="og:image" content="([^"]+)""#).unwrap());
static GOODS_IMG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(https://gd\.image-qoo10\.jp/[^"']+?\.jpg)"#).unwrap());

/// Parse a Qoo10 mobile goods page (the page the app's WebView
/// receives). Returns `None` when the page is not a product page
/// (e.g. the edge error page). Pure; unit-tested against the
/// captured live page.
pub fn parse_detail_html(html: &str, gd_no: &str, url: &str) -> Option<Item> {
    let title = GD_NM_RE.captures(html)?.get(1)?.as_str().to_string();
    let price = MIN_SELL_PRICE_RE
        .captures(html)
        .and_then(|c| c[1].parse::<i64>().ok())
        .or_else(|| {
            SHARE_PRICE_RE
                .captures(html)
                .and_then(|c| c[1].replace(',', "").parse::<i64>().ok())
        });
    let og_image = OG_IMAGE_RE.captures(html).map(|c| c[1].to_string());
    let mut images: Vec<String> = Vec::new();
    for caps in GOODS_IMG_RE.captures_iter(html) {
        let u = caps[1].to_string();
        if !images.contains(&u) {
            images.push(u);
        }
        if images.len() >= 12 {
            break;
        }
    }
    let image = og_image.or_else(|| images.first().cloned());
    Some(Item {
        product_id: gd_no.to_string(),
        title,
        url: url.to_string(),
        source: "qoo10".into(),
        price: price.map(Price::yen),
        image,
        images: if images.is_empty() {
            None
        } else {
            Some(images)
        },
        ..Default::default()
    })
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

pub struct Qoo10Client {
    client: Client,
    api_ua: String,
    webview_ua: String,
}

impl Qoo10Client {
    pub fn new() -> Self {
        let suffix = app_suffix(&placeholder_token());
        let webview_ua = format!(
            "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36 {suffix}"
        );
        Self {
            client: Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("failed to build HTTP client"),
            api_ua: suffix,
            webview_ua,
        }
    }

    async fn post_qapi(&self, method: &str, body: &Value) -> Result<Value> {
        let qtid = Uuid::new_v4().simple().to_string();
        let resp = self
            .client
            .post(format!("{API_BASE}/{method}"))
            .header("GiosisCertificationKey", CERT_KEY)
            .header("qtid", qtid)
            .header("Giosis-Auth", "")
            .header("User-Agent", &self.api_ua)
            .json(body)
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if status.as_u16() == 523 {
            return Err(anyhow!(
                "qoo10 {method}: HTTP 523 (edge rejected the request; the service requires Japan-side access)"
            ));
        }
        if !status.is_success() {
            return Err(anyhow!("qoo10 {method} returned HTTP {status}"));
        }
        serde_json::from_str(&text).map_err(|_| {
            anyhow!(
                "qoo10 {method}: non-JSON response (error page; the service requires Japan-side access)"
            )
        })
    }

    pub async fn search_products(&self, params: SearchParams) -> Result<SearchResult> {
        let body = build_search_body(&params);
        let resp = self.post_qapi("GetMoveSearchData", &body).await?;
        let (items, total, total_pages, suggested) = parse_search_response(&resp);
        Ok(SearchResult {
            provider: "qoo10".into(),
            ok: true,
            query: params.query.clone(),
            page: params.page,
            limit: params.limit,
            sort: params.sort,
            count: items.len(),
            items,
            total_available: total,
            total_pages,
            suggested_items: suggested,
        })
    }

    pub async fn suggest_keywords(&self, keyword: &str, limit: i32) -> Result<Value> {
        let resp = self
            .post_qapi("GetMoveAutoKeywordSearch", &json!({ "keyword": keyword }))
            .await?;
        let list = resp
            .pointer("/d/ResultList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let keywords: Vec<String> = list
            .iter()
            .filter_map(|e| e["keyword"].as_str().map(str::to_string))
            .take(limit.max(1) as usize)
            .collect();
        Ok(json!({
            "provider": "qoo10",
            "ok": true,
            "keyword": keyword,
            "suggestions": keywords,
            "count": keywords.len(),
        }))
    }

    /// Search filter information (`GetMoveSearchFilterInfo`, the
    /// app's filter-popup data source; request form statically
    /// determined and live-verified in round 2 of the analysis):
    /// the brand / colour / price-band / category / shipping-from /
    /// review-score / attribute codes that `search_products`
    /// accepts as filter values for this keyword context. The body
    /// is the search body form with page_no 1 / page_size 30 /
    /// sort 0, as the app's filter flow builds it.
    pub async fn get_search_filters(
        &self,
        keyword: &str,
        category: &str,
        brand: &[String],
    ) -> Result<Value> {
        let mut params = SearchParams {
            query: keyword.to_string(),
            page: 1,
            limit: 30,
            sort: 0,
            search_sub_keyword: String::new(),
            brand: brand.to_vec(),
            category: category.to_string(),
            free_shipping: String::new(),
            min_price: String::new(),
            max_price: String::new(),
            official_shop: String::new(),
            promotion_yn: String::new(),
            shipping_from: Vec::new(),
            review_score: String::new(),
            color: Vec::new(),
            attribute: Vec::new(),
            coupon_label: String::new(),
            keyword_type: String::new(),
            sell_cust_no: String::new(),
            nosearch_yn: "N".to_string(),
            page_type: String::new(),
            filter_type: String::new(),
            seller_promotion_no: String::new(),
            only_total_count: String::new(),
            last_section: String::new(),
            related_bg_color: String::new(),
        };
        params.page = 1;
        let body = build_search_body(&params);
        let resp = self.post_qapi("GetMoveSearchFilterInfo", &body).await?;
        Ok(parse_search_filters_response(&resp, keyword))
    }

    pub async fn get_product(&self, product: &str) -> Result<ItemResult> {
        let gd_no = extract_gd_no(product);
        let url = if product.contains("goodscode=") {
            // A full CONNECT_URL was given: keep it verbatim and
            // append the WebView marker like the app does.
            if product.contains("AppMovePage") {
                product.to_string()
            } else {
                format!("{product}&AppMovePage")
            }
        } else {
            build_detail_url(&gd_no)
        };
        let resp = self
            .client
            .get(&url)
            .header("User-Agent", &self.webview_ua)
            .header("X-Requested-With", PACKAGE)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "ja")
            .send()
            .await?;
        let status = resp.status();
        let html = resp.text().await?;
        if status.as_u16() == 523 {
            return Ok(ItemResult {
                provider: "qoo10".into(),
                ok: false,
                item: None,
                error: Some(
                    "qoo10 detail: HTTP 523 (edge rejected the request; the service requires Japan-side access)"
                        .into(),
                ),
            });
        }
        match parse_detail_html(&html, &gd_no, &url) {
            Some(item) => Ok(ItemResult {
                provider: "qoo10".into(),
                ok: true,
                item: Some(item),
                error: None,
            }),
            None => Ok(ItemResult {
                provider: "qoo10".into(),
                ok: false,
                item: None,
                error: Some(format!(
                    "qoo10 detail: no product data in the returned page (HTTP {status})"
                )),
            }),
        }
    }
}

impl Default for Qoo10Client {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_json(name: &str) -> Value {
        let raw = match name {
            "search4" => include_str!("../tests/fixtures/search4.json"),
            "sort" => include_str!("../tests/fixtures/r3-sort.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(raw).unwrap()
    }

    #[test]
    fn search_body_has_all_23_value_fields() {
        let body = build_search_body(&SearchParams {
            query: "マスク".into(),
            sort: 3,
            free_shipping: "Y".into(),
            brand: vec!["B1".into()],
            ..Default::default()
        });
        assert_eq!(body["page_no"], json!(1));
        assert_eq!(body["page_size"], json!(30));
        assert_eq!(body["sort"], json!(3));
        let v = &body["value"];
        for key in [
            "search_keyword",
            "search_subKeyword",
            "brand",
            "category",
            "free_shipping",
            "min_price",
            "max_price",
            "official_shop",
            "promotion_yn",
            "shipping_from",
            "review_score",
            "color",
            "attribute",
            "coupon_label",
            "keyword_type",
            "sell_cust_no",
            "nosearch_yn",
            "page_type",
            "filter_type",
            "seller_promotion_no",
            "only_total_count",
            "last_section",
            "related_bg_color",
        ] {
            assert!(v.get(key).is_some(), "missing value field {key}");
        }
        assert_eq!(v["search_keyword"], json!("マスク"));
        assert_eq!(v["free_shipping"], json!("Y"));
        assert_eq!(v["nosearch_yn"], json!("N"));
        assert_eq!(v["brand"], json!(["B1"]));
    }

    #[test]
    fn parse_search_fixture() {
        let (items, total, pages, suggested) = parse_search_response(&fixture_json("search4"));
        assert_eq!(total, Some(129));
        assert_eq!(pages, Some(5));
        assert!(!suggested);
        assert_eq!(items.len(), 30);
        let first = &items[0];
        assert!(!first.product_id.is_empty());
        assert!(first.url.contains("goodscode="));
        assert!(first.price.is_some());
    }

    #[test]
    fn parse_sort_fixture_is_price_ascending() {
        let (items, total, _, _) = parse_search_response(&fixture_json("sort"));
        assert_eq!(total, Some(129));
        let prices: Vec<i64> = items
            .iter()
            .filter_map(|i| i.price.as_ref().map(|p| p.value))
            .collect();
        assert_eq!(prices.len(), 30);
        let mut sorted = prices.clone();
        sorted.sort_unstable();
        assert_eq!(prices, sorted, "sort=3 must be price ascending");
    }

    #[test]
    fn detail_url_and_gd_no_extraction() {
        assert_eq!(
            build_detail_url("1131327051"),
            "https://m.qoo10.jp/gmkt.inc/Mobile/goods/move/movegoods.aspx?goodscode=1131327051&ref_move_yn=Y&AppMovePage"
        );
        assert_eq!(
            extract_gd_no("https://m.qoo10.jp/gmkt.inc/Mobile/goods/move/movegoods.aspx?goodscode=1131327051&ref_move_yn=Y"),
            "1131327051"
        );
        assert_eq!(extract_gd_no("1131327051"), "1131327051");
    }

    #[test]
    fn parse_detail_fixture() {
        let html = include_str!("../tests/fixtures/detail.html");
        let item = parse_detail_html(html, "1131327051", "https://m.qoo10.jp/x")
            .expect("detail must parse");
        assert!(item.title.contains("スキンフード"));
        assert_eq!(item.price.as_ref().unwrap().value, 275);
        assert!(item.image.as_ref().unwrap().contains("image-qoo10.jp"));
    }

    #[test]
    fn detail_error_page_is_not_a_product() {
        let html = "<html><head><title>523 Error</title></head><body>523 Error</body></html>";
        assert!(parse_detail_html(html, "1", "https://m.qoo10.jp/x").is_none());
    }

    #[test]
    fn test_parse_search_filters_response() {
        // Shape follows the live capture described in analysis
        // round 2 (keyword マスク): every section present, one entry
        // each, plus the two {id, title} toggles.
        let body = json!({
            "d": {
                "brand": {"items": [{"name": "ブランドA", "nameEn": "Brand A", "code": "B001"}]},
                "price": {"items": [{"name": "~ 1,110円", "minPrice": 0, "maxPrice": 1110}]},
                "color": {"items": [{"hexColor": "#000000", "code": "C01"}]},
                "category": {"lc": [{"CategoryCode": "1"}], "mc": [], "sc": [{"CategoryCode": "11"}]},
                "shippingFrom": {"items": [{"name": "国内", "code": "JP"}]},
                "review": {"items": [{"name": "4.5以上", "value": "4.5"}]},
                "attributeDetail": {"items": [{"name": "サイズ", "attributes": [{"name": "容量", "value": "GATR0001", "details": [{"name": "500ml", "value": "ATR0009"}]}]}]},
                "coupon": {"id": "coupon_label", "title": "クーポン"},
                "freeShipping": {"id": "free_shipping", "title": "送料無料"}
            }
        });
        let out = parse_search_filters_response(&body, "マスク");
        assert_eq!(out["brands"][0]["code"], "B001");
        assert_eq!(out["price_ranges"][0]["max_price"], 1110);
        assert_eq!(out["colors"][0]["hex"], "#000000");
        assert_eq!(out["categories"]["lc"][0]["CategoryCode"], "1");
        assert_eq!(out["shipping_from"][0]["code"], "JP");
        assert_eq!(out["review_scores"][0]["value"], "4.5");
        assert_eq!(
            out["attributes"][0]["attributes"][0]["details"][0]["value"],
            "ATR0009"
        );
        assert_eq!(out["toggles"], json!(["coupon_label", "free_shipping"]));
        // Empty/absent sections normalize to empty lists, and the
        // itemBrandFilter fallback supplies brands.
        let out2 = parse_search_filters_response(
            &json!({"d": {"itemBrandFilter": {"items": [{"name": "B", "code": "B9"}]}}}),
            "x",
        );
        assert_eq!(out2["brands"][0]["code"], "B9");
        assert_eq!(out2["price_ranges"], json!([]));
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored (requires Japan-side access)"]
    async fn test_live_search() {
        let client = Qoo10Client::new();
        let result = client
            .search_products(SearchParams {
                query: "マスク".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok);
        assert!(result.total_available.unwrap_or(0) > 0);
        assert!(!result.items.is_empty());
    }
}
