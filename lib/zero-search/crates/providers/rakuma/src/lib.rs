use anyhow::Result;
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::LazyLock;

const BASE: &str = "https://fril.jp";
const ITEM_BASE: &str = "https://item.fril.jp";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

static RAT_ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"<a\s+href="(https://item\.fril\.jp/([a-f0-9]+))"[^>]*data-rat-itemid="([^"]+)"[^>]*data-rat-item_name="([^"]+)"[^>]*data-rat-igenre="([^"]+)"[^>]*data-rat-price="([^"]+)""#
    ).unwrap()
});
static RAT_TOTAL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-rat-cp-totalresults="([^"]+)""#).unwrap());
static IMG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<img[^>]*(?:data-original|src)="(https://img\.fril\.jp/img/(\d+)/m/[^"]+)""#)
        .unwrap()
});
static JSONLD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<script type="application/ld\+json">(.*?)</script>"#).unwrap()
});
static DETAIL_IMG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(https://img\.fril\.jp/img/\d+/l/[^"]+)""#).unwrap());
/// The seller's shop identifier in the item page's shop link.
/// Most sellers carry a 32-hex id, but store and official sellers
/// use a vanity slug instead (`fril.jp/shop/star55`,
/// `fril.jp/shop/blackscorpion` — measured live in Z12, where 3 of
/// 8 sampled items had slug sellers whose id the old hex-only
/// pattern missed, which in turn broke review lookups for them).
/// Both forms work in the `/shop/<id>/review` URL, so the field
/// (named `seller_hash` for API compatibility) holds either.
static SELLER_HASH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href="https://fril\.jp/shop/([A-Za-z0-9_-]+)""#).unwrap());
static REVIEW_ITEM_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<article[^>]*class="[^"]*review-item[^"]*"[^>]*>.*?<p[^>]*class="[^"]*review-item-title[^"]*"[^>]*>.*?</i>(.*?)</p>.*?<div[^>]*class="[^"]*review-item-text[^"]*"[^>]*>(.*?)</div>\s*<div[^>]*class="row"[^>]*>\s*<div[^>]*class="[^"]*review-item-name[^"]*"[^>]*>\s*(.*?)\s*</div>.*?<p[^>]*class="[^"]*review-item-date[^"]*"[^>]*>(.*?)</p>"#
    ).unwrap()
});

/// [S1-36] Canonical sort vocabulary for `rakuma_search_products`.
/// Aliases (`relevance` for default, `lowest_price` / `highest_price`,
/// `new`, `popular`) are also accepted by [`sort_params`].
pub const SEARCH_SORT_VALUES: &[&str] = &["default", "price_asc", "price_desc", "newest", "likes"];

/// All spellings accepted by [`sort_params`], including aliases.
pub const SEARCH_SORT_SPELLINGS: &[&str] = &[
    "",
    "default",
    "relevance",
    "price_asc",
    "lowest_price",
    "price_desc",
    "highest_price",
    "newest",
    "new",
    "likes",
    "popular",
];

/// [S1-36] Unknown sorts are an explicit error instead of silently
/// degrading to an unsorted default search.
fn sort_params(s: &str) -> anyhow::Result<(&'static str, &'static str)> {
    match s.to_lowercase().as_str() {
        "" | "default" | "relevance" => Ok(("", "")),
        "price_asc" | "lowest_price" => Ok(("sell_price", "asc")),
        "price_desc" | "highest_price" => Ok(("sell_price", "desc")),
        "newest" | "new" => Ok(("created_at", "desc")),
        "likes" | "popular" => Ok(("like_count", "desc")),
        other => anyhow::bail!(
            "unknown sort '{other}' for rakuma search; valid sorts: {} (aliases: lowest_price, highest_price, new, popular, relevance)",
            SEARCH_SORT_VALUES.join(", ")
        ),
    }
}

/// [S1-34] Review-page URL for a 1-based page. Page 1 keeps the original
/// bare URL for every page. Live probing (2026-11-08) settled the
/// question the earlier `?page=N` form left open: the review page
/// ignores `?page=` entirely (page 2 was byte-identical) and carries
/// NO pagination links — it server-renders one long list (400 reviews
/// observed on a single page). Pagination is therefore done by
/// [`RakumaClient::get_reviews`] slicing that list client-side; the
/// `page` argument is retained for signature compatibility only.
pub fn review_page_url(seller_hash: &str, page: i32) -> String {
    let _ = page.max(1);
    format!("{BASE}/shop/{seller_hash}/review")
}

/// [S1-34] Slice one page out of the review page's full list.
/// Returns the window for (`page`, `limit`) and the total number of
/// reviews the page carried. Pages beyond the carried list yield an
/// empty window — never a repeat of page 1.
pub fn review_page_window(all: &[Value], page: i32, limit: i32) -> (Vec<Value>, usize) {
    let page = page.max(1) as usize;
    let limit = limit.max(1) as usize;
    let total = all.len();
    let start = (page - 1) * limit;
    if start >= total {
        return (Vec::new(), total);
    }
    let end = (start + limit).min(total);
    (all[start..end].to_vec(), total)
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub title: String,
    pub url: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Price>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
    /// [S1-35] Category name from the detail JSON-LD `category` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// [S1-35] Brand name from the detail JSON-LD `brand` field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand: Option<String>,
    /// [S1-35] Like count from a schema.org `interactionStatistic`
    /// LikeAction counter in the JSON-LD, when the page carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<i64>,
    /// [S1-35] Shipping fee (JPY) from `offers.shippingDetails`
    /// (`OfferShippingDetails.shippingRate.value`) in the JSON-LD,
    /// when the page carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shipping_fee: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub condition: Option<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SellerInfo {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The seller's shop identifier: a 32-hex id for most sellers,
    /// a vanity slug (e.g. `star55`) for store/official sellers.
    /// Either form addresses the shop and its review page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seller_hash: Option<String>,
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

#[derive(Debug, Clone, Default)]
pub struct SearchParams {
    pub query: String,
    pub limit: i32,
    pub page: i32,
    pub sort: String,
    pub min_price: Option<i64>,
    pub max_price: Option<i64>,
    pub category_id: String,
    pub brand_id: String,
}

pub struct RakumaClient {
    client: Client,
}

impl RakumaClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent(USER_AGENT)
                .redirect(reqwest::redirect::Policy::limited(10))
                .build()
                .expect("failed to build HTTP client"),
        }
    }

    pub async fn search(&self, params: SearchParams) -> Result<SearchResult> {
        // Validated before any HTTP so an unknown sort fails offline too.
        let (sort_key, order) = sort_params(&params.sort)?;
        let limit = if params.limit > 0 { params.limit } else { 40 };
        let page = if params.page > 0 { params.page } else { 1 };

        let mut qp: Vec<(String, String)> = vec![("query".into(), params.query.clone())];
        if !sort_key.is_empty() {
            qp.push(("sort".into(), sort_key.into()));
            qp.push(("order".into(), order.into()));
        }
        if page > 1 {
            qp.push(("page".into(), page.to_string()));
        }
        if let Some(v) = params.min_price {
            qp.push(("min".into(), v.to_string()));
        }
        if let Some(v) = params.max_price {
            qp.push(("max".into(), v.to_string()));
        }
        if !params.category_id.is_empty() {
            qp.push(("category_id".into(), params.category_id.clone()));
        }
        if !params.brand_id.is_empty() {
            qp.push(("brand_id".into(), params.brand_id.clone()));
        }

        let url = format!("{BASE}/s");
        let resp = self.client.get(&url).query(&qp).send().await?;
        let html = if resp.status().is_server_error() && sort_key.is_empty() {
            // fril.jp's relevance (default) search path returns HTTP 500
            // for some keywords (measured 2026-10-31: query=バッグ fails
            // with the site's own sort=relevance&order=desc URLs too,
            // while explicit sorts return the same result set, ~6.4M
            // items). Retry once with an explicit newest-first sort,
            // which only changes ordering, not the result set.
            drop(resp);
            let mut retry_qp = qp.clone();
            retry_qp.push(("sort".into(), "created_at".into()));
            retry_qp.push(("order".into(), "desc".into()));
            let retry_resp = self.client.get(&url).query(&retry_qp).send().await?;
            retry_resp.error_for_status()?.text().await?
        } else {
            resp.error_for_status()?.text().await?
        };

        let (items, total) = extract_search_items(&html);
        let count = items.len().min(limit as usize);

        Ok(SearchResult {
            provider: "rakuma".into(),
            ok: true,
            query: params.query,
            page,
            limit,
            sort: params.sort,
            items: items.into_iter().take(limit as usize).collect(),
            count,
            total_available: total,
        })
    }

    pub async fn get_item(&self, item_hash: &str) -> Result<ItemResult> {
        let url = format!("{ITEM_BASE}/{item_hash}");
        let resp = self.client.get(&url).send().await?.error_for_status()?;
        let html = resp.text().await?;

        match extract_detail_item(&html, item_hash) {
            Some(item) => Ok(ItemResult {
                provider: "rakuma".into(),
                ok: true,
                item: Some(item),
                error: None,
            }),
            None => Ok(ItemResult {
                provider: "rakuma".into(),
                ok: false,
                item: None,
                error: Some("no_jsonld_data".into()),
            }),
        }
    }

    pub async fn get_reviews(&self, seller_hash: &str, page: i32, limit: i32) -> Result<Value> {
        let limit = if limit > 0 { limit } else { 20 };
        let page = page.max(1);

        // Step 1: Fetch shop page for aggregate rating from JSON-LD
        let shop_url = format!("{BASE}/shop/{seller_hash}");
        let shop_resp = self
            .client
            .get(&shop_url)
            .send()
            .await?
            .error_for_status()?;
        let shop_html = shop_resp.text().await?;

        let (aggregate_rating, seller_name) = extract_aggregate_rating(&shop_html);

        // Step 2: Fetch the review page and slice the requested page
        // out of its full list [S1-34]. The page has no server-side
        // pagination (live-verified 2026-11-08: `?page=` ignored, no
        // pagination links, 400 reviews carried in one page).
        let review_url = review_page_url(seller_hash, page);
        let review_resp = self
            .client
            .get(&review_url)
            .send()
            .await?
            .error_for_status()?;
        let review_html = review_resp.text().await?;

        let all = extract_reviews(&review_html, 100_000);
        let (reviews, total_on_page) = review_page_window(&all, page, limit);
        let count = reviews.len();
        let has_next_on_page = (page as usize) * (limit as usize) < total_on_page;

        Ok(json!({
            "provider": "rakuma",
            "ok": true,
            "seller_hash": seller_hash,
            "seller_name": seller_name.unwrap_or_default(),
            "aggregate_rating": aggregate_rating,
            "page": page,
            "reviews": reviews,
            "count": count,
            "total_on_page": total_on_page,
            "has_next_on_page": has_next_on_page,
        }))
    }
}

impl Default for RakumaClient {
    fn default() -> Self {
        Self::new()
    }
}

fn extract_search_items(html: &str) -> (Vec<Item>, Option<i64>) {
    let mut img_map: HashMap<String, String> = HashMap::new();
    for caps in IMG_RE.captures_iter(html) {
        img_map.insert(caps[2].to_string(), caps[1].to_string());
    }

    let total = RAT_TOTAL_RE
        .captures(html)
        .and_then(|c| c[1].parse::<i64>().ok());

    let mut seen = HashSet::new();
    let mut items = vec![];

    for caps in RAT_ITEM_RE.captures_iter(html) {
        let item_url = &caps[1];
        let item_hash = &caps[2];
        let rat_id = &caps[3];
        let name = &caps[4];
        let genre = &caps[5];
        let price_str = &caps[6];

        if !seen.insert(item_hash.to_string()) {
            continue;
        }

        let parts: Vec<&str> = rat_id.split('/').collect();
        let (seller_id, item_id) = if parts.len() > 1 {
            (Some(parts[0].to_string()), parts[1].to_string())
        } else {
            (None, parts[0].to_string())
        };

        let price_val = price_str.parse::<i64>().unwrap_or(0);
        let image = img_map.get(&item_id).cloned();

        items.push(Item {
            product_id: item_hash.to_string(),
            item_id: Some(item_id),
            title: name.to_string(),
            url: item_url.to_string(),
            source: "rakuma".into(),
            price: Some(Price::yen(price_val)),
            category_id: Some(genre.to_string()),
            seller: seller_id.map(|id| SellerInfo {
                id: Some(id),
                name: None,
                seller_hash: None,
            }),
            image,
            ..Default::default()
        });
    }

    (items, total)
}

/// [S1-35] A schema.org name-bearing field: plain string, `{"name": …}`
/// object, or an array of either (last entry wins, matching how the
/// yahoo-flea detail extractor treats brand/category lists).
fn jsonld_name(v: &Value) -> Option<String> {
    if let Some(s) = v.as_str().filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    if let Some(obj) = v.as_object() {
        return obj
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
    }
    if let Some(arr) = v.as_array() {
        return arr.iter().rev().find_map(jsonld_name);
    }
    None
}

/// [S1-35] Like count from schema.org `interactionStatistic` entries
/// whose interaction type is a LikeAction.
fn jsonld_like_count(prod: &Value) -> Option<i64> {
    let stats = prod.get("interactionStatistic")?;
    let entries: Vec<&Value> = match stats {
        Value::Array(arr) => arr.iter().collect(),
        Value::Object(_) => vec![stats],
        _ => return None,
    };
    for e in entries {
        let ty = e
            .get("interactionType")
            .and_then(|t| {
                t.as_str()
                    .or_else(|| t.get("@type").and_then(|v| v.as_str()))
            })
            .unwrap_or("");
        if ty.ends_with("LikeAction") {
            if let Some(n) = e.get("userInteractionCount").and_then(|c| {
                c.as_i64()
                    .or_else(|| c.as_str().and_then(|s| s.parse().ok()))
            }) {
                return Some(n);
            }
        }
    }
    None
}

/// [S1-35] Shipping fee from `offers.shippingDetails.shippingRate.value`
/// (schema.org OfferShippingDetails / MonetaryAmount).
fn jsonld_shipping_fee(offers: &Value) -> Option<i64> {
    let rate = offers.get("shippingDetails")?.get("shippingRate")?;
    let v = rate.get("value")?;
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .or_else(|| v.as_f64().map(|f| f as i64))
}

fn extract_detail_item(html: &str, item_hash: &str) -> Option<Item> {
    let mut product: Option<Value> = None;
    for caps in JSONLD_RE.captures_iter(html) {
        if let Ok(data) = serde_json::from_str::<Value>(&caps[1]) {
            if data["@type"].as_str() == Some("Product") {
                product = Some(data);
                break;
            }
        }
    }

    let prod = product?;
    let offers = &prod["offers"];

    let price_val = offers["price"]
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| offers["price"].as_i64());

    let condition_url = offers["itemCondition"].as_str().unwrap_or_default();
    let condition = if condition_url.contains("NewCondition") {
        Some("新品".into())
    } else if condition_url.contains("UsedCondition") {
        Some("中古".into())
    } else if condition_url.contains("RefurbishedCondition") {
        Some("再生品".into())
    } else {
        None
    };

    let availability_url = offers["availability"].as_str().unwrap_or_default();
    let availability = if availability_url.contains("InStock") {
        Some("販売中".into())
    } else if availability_url.contains("SoldOut") || availability_url.contains("OutOfStock") {
        Some("売り切れ".into())
    } else {
        None
    };

    let description = prod["description"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);
    let main_image = prod["image"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    let all_images: Vec<String> = {
        let mut set = BTreeSet::new();
        for caps in DETAIL_IMG_RE.captures_iter(html) {
            set.insert(caps[1].to_string());
        }
        set.into_iter().collect()
    };

    let seller_name = offers["seller"]["name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    let seller_hash = SELLER_HASH_RE.captures(html).map(|c| c[1].to_string());

    let seller = if seller_name.is_some() || seller_hash.is_some() {
        Some(SellerInfo {
            id: None,
            name: seller_name,
            seller_hash,
        })
    } else {
        None
    };

    // [S1-35] Extra structured fields from the same JSON-LD block.
    let brand = prod.get("brand").and_then(jsonld_name);
    let category = prod.get("category").and_then(jsonld_name);
    let likes = jsonld_like_count(&prod);
    let shipping_fee = jsonld_shipping_fee(offers);

    Some(Item {
        product_id: item_hash.to_string(),
        title: prod["name"].as_str().unwrap_or_default().to_string(),
        url: format!("{ITEM_BASE}/{item_hash}"),
        source: "rakuma".into(),
        price: price_val.map(Price::yen),
        category,
        brand,
        likes,
        shipping_fee,
        condition,
        availability,
        description,
        image: main_image,
        images: if all_images.is_empty() {
            None
        } else {
            Some(all_images)
        },
        seller,
        ..Default::default()
    })
}

fn extract_aggregate_rating(html: &str) -> (Value, Option<String>) {
    for caps in JSONLD_RE.captures_iter(html) {
        if let Ok(data) = serde_json::from_str::<Value>(&caps[1]) {
            if data["@type"].as_str() == Some("Store") {
                let agg = &data["aggregateRating"];
                let seller_name = data["name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let rating = json!({
                    "rating_value": agg["ratingValue"].as_str().unwrap_or("0"),
                    "rating_count": agg["ratingCount"].as_str()
                        .and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
                    "best_rating": agg["bestRating"].as_str().unwrap_or("5"),
                    "worst_rating": agg["worstRating"].as_str().unwrap_or("0"),
                });
                return (rating, seller_name);
            }
        }
    }
    (json!(null), None)
}

fn strip_html_tags(s: &str) -> String {
    static TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
    let text = s
        .replace("<br />", "\n")
        .replace("<br>", "\n")
        .replace("<br/>", "\n");
    TAG_RE.replace_all(&text, "").trim().to_string()
}

fn extract_reviews(html: &str, limit: i32) -> Vec<Value> {
    let mut reviews = Vec::new();

    for caps in REVIEW_ITEM_RE.captures_iter(html) {
        if reviews.len() >= limit as usize {
            break;
        }

        let rating = caps[1].trim().to_string();
        let message = strip_html_tags(&caps[2]);
        let reviewer_name = caps[3].trim().to_string();
        let date = caps[4].trim().to_string();

        reviews.push(json!({
            "rating": rating,
            "message": message,
            "reviewer_name": reviewer_name,
            "date": date,
        }));
    }

    reviews
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sort_params() {
        assert_eq!(sort_params("").unwrap(), ("", ""));
        assert_eq!(sort_params("price_asc").unwrap(), ("sell_price", "asc"));
        assert_eq!(sort_params("newest").unwrap(), ("created_at", "desc"));
        assert_eq!(sort_params("likes").unwrap(), ("like_count", "desc"));
    }

    #[test]
    fn test_sort_params_case_insensitive() {
        assert_eq!(sort_params("PRICE_ASC").unwrap(), ("sell_price", "asc"));
        assert_eq!(sort_params("Price_Desc").unwrap(), ("sell_price", "desc"));
    }

    // [S1-36] Unknown sort must error explicitly, naming the vocabulary.
    #[test]
    fn test_sort_params_unknown_is_error() {
        let err = sort_params("unknown_sort").unwrap_err().to_string();
        assert!(err.contains("unknown sort 'unknown_sort'"), "err: {err}");
        for v in SEARCH_SORT_VALUES {
            assert!(err.contains(v), "err must list {v}: {err}");
        }
        for s in SEARCH_SORT_SPELLINGS {
            assert!(sort_params(s).is_ok(), "spelling {s} must be accepted");
        }
    }

    // [S1-36] search() must reject an unknown sort before any HTTP.
    #[tokio::test]
    async fn test_search_unknown_sort_errors_offline() {
        let client = RakumaClient::new();
        let err = client
            .search(SearchParams {
                query: "x".into(),
                sort: "bogus_sort".into(),
                ..Default::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown sort"), "err: {err}");
    }

    // [Z12] Shop ids come in two forms: 32-hex for most sellers,
    // vanity slugs for store/official sellers. Both must be
    // captured — the hex-only pattern left slug sellers without a
    // seller_hash and broke review lookups for their items.
    #[test]
    fn test_seller_shop_id_forms() {
        let hex_page = r#"<a href="https://fril.jp/shop/03a7da03a4853007948264b5b7aa3dfa">shop</a>"#;
        let slug_page = r#"<a href="https://fril.jp/shop/star55">shop</a><a href="https://fril.jp/shop/star55/review">reviews</a>"#;
        assert_eq!(
            SELLER_HASH_RE.captures(hex_page).unwrap()[1].to_string(),
            "03a7da03a4853007948264b5b7aa3dfa"
        );
        assert_eq!(
            SELLER_HASH_RE.captures(slug_page).unwrap()[1].to_string(),
            "star55"
        );
        assert_eq!(
            review_page_url("star55", 1),
            "https://fril.jp/shop/star55/review"
        );
    }

    // [S1-34] The review page URL is page-independent (live-verified:
    // `?page=` is ignored server-side); pagination slices client-side.
    #[test]
    fn test_review_page_url() {
        let h = "0123456789abcdef0123456789abcdef";
        let base = "https://fril.jp/shop/0123456789abcdef0123456789abcdef/review";
        assert_eq!(review_page_url(h, 1), base);
        assert_eq!(review_page_url(h, 2), base);
        assert_eq!(review_page_url(h, 0), base);
        assert_eq!(review_page_url(h, 5), base);
    }

    // [S1-34]
    #[test]
    fn test_review_page_window() {
        let all: Vec<Value> = (0..12).map(|i| json!({"n": i})).collect();
        let (w1, t1) = review_page_window(&all, 1, 5);
        assert_eq!(t1, 12);
        assert_eq!(w1.len(), 5);
        assert_eq!(w1[0]["n"], 0);
        let (w2, _) = review_page_window(&all, 2, 5);
        assert_eq!(w2.len(), 5);
        assert_eq!(w2[0]["n"], 5);
        let (w3, _) = review_page_window(&all, 3, 5);
        assert_eq!(w3.len(), 2);
        let (w4, _) = review_page_window(&all, 4, 5);
        assert!(w4.is_empty(), "beyond the carried list must be empty");
    }

    // [S1-35] Detail extraction from a JSON-LD fixture carrying the
    // schema.org brand / category / likes / shipping fields.
    #[test]
    fn test_extract_detail_item_jsonld_extras() {
        let html = r#"<html><head>
<script type="application/ld+json">
{"@context":"https://schema.org","@type":"Product","name":"テスト商品",
 "description":"説明文","image":"https://img.fril.jp/img/1/l/sample.jpg",
 "brand":{"@type":"Brand","name":"テストブランド"},
 "category":"レディース > バッグ",
 "interactionStatistic":{"@type":"InteractionCounter",
   "interactionType":"https://schema.org/LikeAction","userInteractionCount":27},
 "offers":{"@type":"Offer","price":"4500",
   "itemCondition":"https://schema.org/UsedCondition",
   "availability":"https://schema.org/InStock",
   "shippingDetails":{"@type":"OfferShippingDetails",
     "shippingRate":{"@type":"MonetaryAmount","value":"210","currency":"JPY"}},
   "seller":{"@type":"Person","name":"出品者A"}}}
</script></head><body>
<a href="https://fril.jp/shop/0123456789abcdef0123456789abcdef">shop</a>
</body></html>"#;
        let item = extract_detail_item(html, "abc123").expect("item");
        assert_eq!(item.brand.as_deref(), Some("テストブランド"));
        assert_eq!(item.category.as_deref(), Some("レディース > バッグ"));
        assert_eq!(item.likes, Some(27));
        assert_eq!(item.shipping_fee, Some(210));
        assert_eq!(item.condition.as_deref(), Some("中古"));
        assert_eq!(item.price.map(|p| p.value), Some(4500));
        // Plain-string brand + array category forms.
        let html2 = r#"<script type="application/ld+json">
{"@type":"Product","name":"x","brand":"文字列ブランド",
 "category":[{"name":"親"},{"name":"子カテゴリ"}],
 "offers":{"price":100}}
</script>"#;
        let item2 = extract_detail_item(html2, "def456").expect("item2");
        assert_eq!(item2.brand.as_deref(), Some("文字列ブランド"));
        assert_eq!(item2.category.as_deref(), Some("子カテゴリ"));
        assert_eq!(item2.likes, None);
        assert_eq!(item2.shipping_fee, None);
    }

    // [S1-35] A minimal JSON-LD (the previously supported shape) still
    // parses and leaves the new fields empty rather than failing.
    #[test]
    fn test_extract_detail_item_minimal_jsonld() {
        let html = r#"<script type="application/ld+json">
{"@type":"Product","name":"最小商品","offers":{"price":"900"}}
</script>"#;
        let item = extract_detail_item(html, "abc999").expect("item");
        assert_eq!(item.title, "最小商品");
        assert_eq!(item.brand, None);
        assert_eq!(item.category, None);
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search() {
        let client = RakumaClient::new();
        let result = client
            .search(SearchParams {
                query: "バッグ".into(),
                limit: 5,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok, "search must succeed");
        assert_eq!(result.provider, "rakuma");
        assert!(
            !result.items.is_empty(),
            "search returned 0 items — API may be broken"
        );
        assert_eq!(result.count, result.items.len());
        for item in &result.items {
            assert!(!item.product_id.is_empty());
            assert!(!item.title.is_empty());
            assert!(item.url.contains("fril.jp"), "bad url: {}", item.url);
            if let Some(price) = &item.price {
                assert_eq!(price.currency, "JPY");
                assert!(price.value > 0);
            }
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_multiword() {
        let client = RakumaClient::new();
        let result = client
            .search(SearchParams {
                query: "ワイヤレス イヤホン".into(),
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(result.ok, "multi-word search must succeed");
        assert!(
            !result.items.is_empty(),
            "multi-word search returned 0 items"
        );
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_search_then_get_item() {
        let client = RakumaClient::new();
        let search = client
            .search(SearchParams {
                query: "カメラ".into(),
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(search.ok);
        assert!(!search.items.is_empty(), "need items to test get_item");
        let first = &search.items[0];
        let detail = client.get_item(&first.product_id).await.unwrap();
        assert!(
            detail.ok,
            "get_item FAILED for {}: {:?}",
            first.product_id, detail
        );
        assert_eq!(detail.provider, "rakuma");
        let item = detail.item.expect("item must be present");
        assert!(!item.title.is_empty());
        if let Some(price) = &item.price {
            assert_eq!(price.currency, "JPY");
        }
    }

    // [S1-35 probe] Print a real detail item so the presence of the
    // new JSON-LD-derived fields (category/brand/likes/shipping_fee)
    // on live pages can be checked.
    #[tokio::test]
    #[ignore = "live probe: print a detail item's new fields"]
    async fn test_live_probe_detail_new_fields() {
        let client = RakumaClient::new();
        let search = client
            .search(SearchParams {
                query: "バッグ".into(),
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        for cand in search.items.iter().take(3) {
            let detail = client.get_item(&cand.product_id).await.unwrap();
            if let Some(item) = &detail.item {
                println!(
                    "PROBE item {} category={:?} brand={:?} likes={:?} shipping_fee={:?}",
                    item.product_id, item.category, item.brand, item.likes, item.shipping_fee
                );
            }
        }
    }

    // [S1-34 probe] `?page=N` was ignored live (2026-11-08). Inspect the
    // review page itself: how many reviews the page-1 HTML carries and
    // what pagination links it contains.
    #[tokio::test]
    #[ignore = "live probe: inspect rakuma review page pagination"]
    async fn test_live_probe_review_page_pagination() {
        let client = RakumaClient::new();
        let search = client
            .search(SearchParams {
                query: "バッグ".into(),
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        let first = &search.items[0];
        let detail = client.get_item(&first.product_id).await.unwrap();
        let item = detail.item.expect("item must be present");
        let seller_hash = item
            .seller
            .expect("seller")
            .seller_hash
            .expect("seller_hash");
        let url = review_page_url(&seller_hash, 1);
        let html = client
            .client
            .get(&url)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        let all = extract_reviews(&html, 100_000);
        println!("PROBE review page url: {url}");
        println!("PROBE reviews present in page-1 HTML: {}", all.len());
        let href_re = Regex::new(r#"href="([^"]*(?:review|page)[^"]*)""#).unwrap();
        let mut seen = std::collections::BTreeSet::new();
        for caps in href_re.captures_iter(&html) {
            seen.insert(caps[1].to_string());
        }
        for h in seen.iter().take(20) {
            println!("PROBE href: {h}");
        }
        println!("PROBE total pagination-ish hrefs: {}", seen.len());
    }

    // [S1-34] LIVE CHECK: review page 2 must differ from page 1 when
    // page 1 is full.
    #[tokio::test]
    #[ignore = "live test: verify rakuma reviews page1/page2 differ"]
    async fn test_live_get_reviews_page2_differs() {
        let client = RakumaClient::new();
        let search = client
            .search(SearchParams {
                query: "バッグ".into(),
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(search.ok);
        // Not every item's seller carries a seller_hash; take the
        // first of the top results that does.
        let mut seller_hash = None;
        for cand in search.items.iter().take(3) {
            let detail = client.get_item(&cand.product_id).await.unwrap();
            if let Some(item) = detail.item {
                if let Some(hash) = item.seller.and_then(|s| s.seller_hash) {
                    seller_hash = Some(hash);
                    break;
                }
            }
        }
        let seller_hash = seller_hash.expect("a seller_hash from the top results");
        let p1 = client.get_reviews(&seller_hash, 1, 5).await.unwrap();
        assert_eq!(p1["page"], 1);
        let r1 = p1["reviews"].as_array().unwrap();
        if r1.len() >= 5 {
            // Page 2 must be the NEXT window of the same list: equal to
            // reviews[5..10] of a limit-10 page-1 fetch, and different
            // from page 1 itself.
            let big = client.get_reviews(&seller_hash, 1, 10).await.unwrap();
            let p2 = client.get_reviews(&seller_hash, 2, 5).await.unwrap();
            assert_eq!(p2["page"], 2);
            assert_ne!(
                p1["reviews"], p2["reviews"],
                "page2 reviews identical to page1"
            );
            let big_reviews = big["reviews"].as_array().unwrap();
            if big_reviews.len() >= 10 {
                assert_eq!(
                    p2["reviews"],
                    json!(big_reviews[5..10].to_vec()),
                    "page2 must be the second window of the page-1 list"
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "live test: run with --ignored"]
    async fn test_live_get_reviews() {
        let client = RakumaClient::new();

        // Search to get an item
        let search = client
            .search(SearchParams {
                query: "バッグ".into(),
                limit: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(search.ok);
        assert!(!search.items.is_empty(), "need items to find seller");

        // Get item detail to extract seller_hash
        let first = &search.items[0];
        let detail = client.get_item(&first.product_id).await.unwrap();
        assert!(detail.ok, "get_item must succeed");
        let item = detail.item.expect("item must be present");
        let seller = item.seller.expect("seller info must be present");
        let seller_hash = seller
            .seller_hash
            .expect("seller_hash must be present in item detail");
        assert_eq!(seller_hash.len(), 32, "seller_hash must be 32-char hex");

        // Fetch reviews
        let reviews = client.get_reviews(&seller_hash, 1, 5).await.unwrap();
        assert_eq!(reviews["provider"], "rakuma");
        assert_eq!(reviews["ok"], true);
        assert_eq!(reviews["seller_hash"], seller_hash);
        assert!(
            !reviews["seller_name"].as_str().unwrap_or("").is_empty(),
            "seller_name must be present"
        );

        // Check aggregate rating
        let agg = &reviews["aggregate_rating"];
        if !agg.is_null() {
            assert!(agg["rating_count"].as_i64().unwrap_or(0) >= 0);
        }

        // Check individual reviews
        let review_arr = reviews["reviews"]
            .as_array()
            .expect("reviews must be array");
        assert!(!review_arr.is_empty(), "should have at least one review");
        assert!(review_arr.len() <= 5, "should respect limit");
        for r in review_arr {
            assert!(
                !r["rating"].as_str().unwrap_or("").is_empty(),
                "rating must not be empty"
            );
            assert!(
                !r["date"].as_str().unwrap_or("").is_empty(),
                "date must not be empty"
            );
        }
    }
}
