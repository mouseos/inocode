//! Request builders — a faithful port of `goofish_client/requests.py`
//! and `goofish_client/filters.py`. Only statically confirmed fields are
//! emitted; `None` values are omitted (fastjson does not serialise null
//! reference fields); runtime-generated values are never invented.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

use crate::registry::get_api;

// ---------------------------------------------------------------------------
// RequestPlan
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct RequestPlan {
    pub api_name: String,
    pub version: String,
    pub data: Value,
    pub feature_id: String,
    pub pagination_kind: String,
}

impl RequestPlan {
    /// The `data` form field value (compact JSON, non-ASCII unescaped,
    /// as fastjson emits).
    pub fn data_json(&self) -> String {
        serde_json::to_string(&self.data).unwrap_or_else(|_| "{}".to_string())
    }

    /// Copy with data fields replaced; a `None` override removes the key
    /// (mirrors `RequestPlan.with_fields`).
    pub fn with_fields(&self, overrides: &Map<String, Value>) -> RequestPlan {
        let mut data = self.data.as_object().cloned().unwrap_or_default();
        for (k, v) in overrides {
            if v.is_null() {
                data.remove(k);
            } else {
                data.insert(k.clone(), v.clone());
            }
        }
        let mut plan = self.clone();
        plan.data = Value::Object(data);
        plan
    }
}

fn plan(api_name: &str, data: Map<String, Value>, version: Option<&str>) -> Result<RequestPlan> {
    let spec = get_api(api_name)?;
    let data: Map<String, Value> = data.into_iter().filter(|(_, v)| !v.is_null()).collect();
    Ok(RequestPlan {
        api_name: api_name.to_string(),
        version: version.map(|s| s.to_string()).unwrap_or(spec.version),
        data: Value::Object(data),
        feature_id: spec.feature_id,
        pagination_kind: spec.pagination_kind,
    })
}

fn merge(base: &mut Map<String, Value>, extra: Option<&Map<String, Value>>) {
    if let Some(extra) = extra {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
}

fn require_positive_int(name: &str, value: i64) -> Result<i64> {
    if value < 1 {
        return Err(anyhow!("{name} must be >= 1, got {value}"));
    }
    Ok(value)
}

fn require_item_id(value: &str) -> Result<&str> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!(
            "item_id must be a non-empty numeric string, got {value:?}"
        ));
    }
    Ok(value)
}

fn blank_to_none(value: Option<&str>) -> Option<&str> {
    match value {
        Some("") => None,
        other => other,
    }
}

fn insert_str(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        map.insert(key.to_string(), Value::String(v.to_string()));
    }
}

// ---------------------------------------------------------------------------
// filters.py
// ---------------------------------------------------------------------------

/// Validate a `priceRange` single value (`"min,max"` form, either side
/// may be empty, each present side a non-negative number).
pub fn validate_price_range(value: &str) -> Result<&str> {
    let parts: Vec<&str> = value.split(',').collect();
    if parts.len() != 2 {
        return Err(anyhow!(
            "price_range must be in 'min,max' form, got {value:?}"
        ));
    }
    let (lo, hi) = (parts[0], parts[1]);
    if lo.is_empty() && hi.is_empty() {
        return Err(anyhow!("price_range must set at least one side"));
    }
    for side in [lo, hi] {
        if !side.is_empty() && !is_price_side(side) {
            return Err(anyhow!(
                "price_range sides must be non-negative numbers, got {value:?}"
            ));
        }
    }
    Ok(value)
}

fn is_price_side(side: &str) -> bool {
    // ^\d+(\.\d+)?$
    let mut split = side.splitn(2, '.');
    let int_part = split.next().unwrap_or("");
    if int_part.is_empty() || !int_part.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    match split.next() {
        None => true,
        Some(frac) => !frac.is_empty() && frac.chars().all(|c| c.is_ascii_digit()),
    }
}

/// Serialise CPV selections to the propValueStr JSON string
/// (`{"searchFilter": "pid:vid1,vid2;pid2:vid3"}`), TransformUtil
/// semantics: values are a LinkedHashSet (dedupe, order preserved),
/// empties skipped, nothing remaining -> None.
pub fn build_prop_value_str(
    selections: &[(String, Vec<String>)],
    price_range: Option<&str>,
) -> Result<Option<String>> {
    let mut parts: Vec<String> = Vec::new();
    for (pid, vids) in selections {
        let mut values: Vec<String> = Vec::new();
        for v in vids {
            if !v.is_empty() && !values.contains(v) {
                values.push(v.clone());
            }
        }
        if values.is_empty() {
            continue;
        }
        parts.push(format!("{}:{}", pid, values.join(",")));
    }
    if let Some(pr) = price_range {
        validate_price_range(pr)?;
        parts.push(format!("priceRange:{pr}"));
    }
    if parts.is_empty() {
        return Ok(None);
    }
    let body = parts.join(";");
    Ok(Some(json!({"searchFilter": body}).to_string()))
}

/// Inverse of `build_prop_value_str` (mirrors transPropValueStr2Map;
/// duplicate pids are merged in order).
#[allow(clippy::type_complexity)]
pub fn parse_prop_value_str(text: &str) -> Result<(Vec<(String, Vec<String>)>, Option<String>)> {
    let obj: Value = serde_json::from_str(text)?;
    let body = obj
        .get("searchFilter")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let mut selections: Vec<(String, Vec<String>)> = Vec::new();
    let mut price_range = None;
    if !body.is_empty() {
        for part in body.split(';') {
            if part.is_empty() {
                continue;
            }
            let (key, rest) = match part.split_once(':') {
                Some((k, r)) => (k, r),
                None => (part, ""),
            };
            if key == "priceRange" {
                price_range = Some(rest.to_string());
            } else {
                let values: Vec<String> = if rest.is_empty() {
                    Vec::new()
                } else {
                    rest.split(',').map(|s| s.to_string()).collect()
                };
                if let Some(existing) = selections.iter_mut().find(|(k, _)| k == key) {
                    existing.1.extend(values);
                } else {
                    selections.push((key.to_string(), values));
                }
            }
        }
    }
    Ok((selections, price_range))
}

/// Build the extraFilterValue JSON string for a region selection.
/// Key order inside the division entry: area, city, province
/// (AreaLocation bean field-declaration order).
pub fn build_extra_filter_value(
    province: Option<&str>,
    city: Option<&str>,
    area: Option<&str>,
) -> Option<String> {
    let mut entry = Map::new();
    if let Some(a) = area {
        entry.insert("area".into(), json!(a));
    }
    if let Some(c) = city {
        entry.insert("city".into(), json!(c));
    }
    if let Some(p) = province {
        entry.insert("province".into(), json!(p));
    }
    if entry.is_empty() {
        return None;
    }
    Some(json!({"divisionList": [Value::Object(entry)]}).to_string())
}

/// The extraFilterValue sent when no region is selected:
/// `{"divisionList":[{}]}` (fastjson rendering of an all-null
/// AreaLocation; the key is present, not omitted).
pub fn default_extra_filter_value() -> String {
    "{\"divisionList\":[{}]}".to_string()
}

#[derive(Debug, Clone, Default)]
pub struct SearchFilters {
    /// CPV selections: (property id, selected value ids), order preserved.
    pub prop_selections: Vec<(String, Vec<String>)>,
    pub price_range_value: Option<String>,
    pub start_price: Option<String>,
    pub end_price: Option<String>,
    pub sort_field: Option<String>,
    pub sort_value: Option<String>,
    pub province: Option<String>,
    pub city: Option<String>,
    pub area: Option<String>,
    pub category_id: Option<String>,
    pub front_cat_id: Option<String>,
    pub leaf_id: Option<String>,
}

impl SearchFilters {
    pub fn to_search_fields(&self) -> Result<Map<String, Value>> {
        // The app always writes the sort pair together.
        if self.sort_field.is_none() != self.sort_value.is_none() {
            return Err(anyhow!(
                "sort_field and sort_value must be set together (the app writes the pair from one delivered Value)"
            ));
        }
        let mut out = Map::new();
        if !self.prop_selections.is_empty() || self.price_range_value.is_some() {
            if let Some(pvs) =
                build_prop_value_str(&self.prop_selections, self.price_range_value.as_deref())?
            {
                out.insert("propValueStr".into(), json!(pvs));
            }
        }
        insert_str(&mut out, "startPrice", self.start_price.as_deref());
        insert_str(&mut out, "endPrice", self.end_price.as_deref());
        insert_str(&mut out, "sortField", self.sort_field.as_deref());
        insert_str(&mut out, "sortValue", self.sort_value.as_deref());
        insert_str(&mut out, "province", self.province.as_deref());
        insert_str(&mut out, "city", self.city.as_deref());
        insert_str(&mut out, "area", self.area.as_deref());
        if let Some(extra) = build_extra_filter_value(
            self.province.as_deref(),
            self.city.as_deref(),
            self.area.as_deref(),
        ) {
            out.insert("extraFilterValue".into(), json!(extra));
        }
        insert_str(&mut out, "categoryId", self.category_id.as_deref());
        insert_str(&mut out, "frontCatId", self.front_cat_id.as_deref());
        insert_str(&mut out, "leafId", self.leaf_id.as_deref());
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Main search: mtop.taobao.idlemtopsearch.search 1.0
// ---------------------------------------------------------------------------

pub const AB_CONFIGS_JSON: &str =
    "{\"isSearchResultNative\":\"true\",\"isEnterResultNative\":\"true\"}";

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub page_number: i64,
    pub rows_per_page: i64,
    pub filters: Option<SearchFilters>,
    /// Tri-state: Some(true)/Some(false) emit the key, None omits it.
    /// Defaults to Some(true) (router-built first request).
    pub first_request: Option<bool>,
    pub from_filter: bool,
    pub sqi_control_fields_json: Option<String>,
    pub extra_filter_value: Option<String>,
    pub target_user_id: Option<String>,
    pub extra_fields: Option<Map<String, Value>>,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            page_number: 1,
            rows_per_page: 10,
            filters: None,
            first_request: Some(true),
            from_filter: false,
            sqi_control_fields_json: None,
            extra_filter_value: None,
            target_user_id: None,
            extra_fields: None,
        }
    }
}

pub fn build_search_request(keyword: Option<&str>, opts: &SearchOptions) -> Result<RequestPlan> {
    let keyword = blank_to_none(keyword);
    let page_number = require_positive_int("page_number", opts.page_number)?;
    let rows_per_page = require_positive_int("rows_per_page", opts.rows_per_page)?;
    let mut data = Map::new();
    data.insert("pageNumber".into(), json!(page_number));
    data.insert("rowsPerPage".into(), json!(rows_per_page));
    data.insert("page".into(), json!(1));
    data.insert("relateResultPageNumber".into(), json!(1));
    data.insert("bizFrom".into(), json!("home"));
    data.insert("searchReqFromPage".into(), json!("xyHome"));
    data.insert("searchReqFromActivatePagePart".into(), json!("default"));
    data.insert("searchTabType".into(), json!("SEARCH_TAB_MAIN"));
    data.insert("supportFlexFilter".into(), json!(true));
    data.insert("smartUIFilter".into(), json!(true));
    data.insert("resultListLastIndex".into(), json!(0));
    data.insert("relateResultListLastIndex".into(), json!(0));
    data.insert("disableHierarchicalSort".into(), json!(0));
    data.insert("activeSearch".into(), json!(false));
    data.insert("forceUseInputKeyword".into(), json!(false));
    data.insert("forceUseTppRepair".into(), json!(false));
    data.insert("fromSuggest".into(), json!(false));
    data.insert("fromShade".into(), json!(false));
    data.insert("fromKits".into(), json!(false));
    data.insert("fromLeaf".into(), json!(false));
    data.insert("fromFilter".into(), json!(opts.from_filter));
    data.insert("abConfigs".into(), json!(AB_CONFIGS_JSON));
    insert_str(&mut data, "keyword", keyword);
    if let Some(fr) = opts.first_request {
        data.insert("firstRequest".into(), json!(fr));
    }
    if let Some(filters) = &opts.filters {
        for (k, v) in filters.to_search_fields()? {
            data.insert(k, v);
        }
    }
    if opts.from_filter
        && !data.contains_key("fromCombo")
        && !(opts
            .extra_fields
            .as_ref()
            .map(|e| e.contains_key("fromCombo"))
            .unwrap_or(false))
    {
        data.insert("fromCombo".into(), json!("Filter"));
    }
    insert_str(
        &mut data,
        "sqiControlFieldsJson",
        opts.sqi_control_fields_json.as_deref(),
    );
    if let Some(efv) = &opts.extra_filter_value {
        data.insert("extraFilterValue".into(), json!(efv));
    } else if !data.contains_key("extraFilterValue") {
        data.insert(
            "extraFilterValue".into(),
            json!(default_extra_filter_value()),
        );
    }
    insert_str(&mut data, "targetUserId", opts.target_user_id.as_deref());
    merge(&mut data, opts.extra_fields.as_ref());
    plan("mtop.taobao.idlemtopsearch.search", data, None)
}

// ---------------------------------------------------------------------------
// Suggest: mtop.taobao.idlemtopsearch.search.suggest 4.0
// ---------------------------------------------------------------------------

pub fn build_suggest_request(
    input_words: &str,
    search_req_from_page: Option<&str>,
    bucket_id: i64,
    target_user_id: Option<&str>,
) -> Result<RequestPlan> {
    let mut data = Map::new();
    data.insert("inputWords".into(), json!(input_words));
    data.insert("type".into(), json!(0));
    data.insert("bucketId".into(), json!(bucket_id));
    insert_str(&mut data, "searchReqFromPage", search_req_from_page);
    insert_str(&mut data, "targetUserId", target_user_id);
    plan("mtop.taobao.idlemtopsearch.search.suggest", data, None)
}

// ---------------------------------------------------------------------------
// Category guide: mtop.taobao.idle.category.guide 2.1
// ---------------------------------------------------------------------------

pub fn build_category_request(name: Option<&str>) -> Result<RequestPlan> {
    let mut data = Map::new();
    insert_str(&mut data, "name", name);
    plan("mtop.taobao.idle.category.guide", data, None)
}

// ---------------------------------------------------------------------------
// Item detail: mtop.taobao.idle.awesome.detail.unit 1.0
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DetailOptions {
    pub city: Option<String>,
    pub fishpool_topic_id: Option<String>,
    pub groupon_id: Option<String>,
    pub enter_type: Option<String>,
    pub need_simple_detail: bool,
    pub is_bundle_group: bool,
    pub ab_info: Option<String>,
    pub extra: Option<String>,
    pub extra_param: Option<String>,
    pub commerce_ad_plan_id: Option<String>,
    pub client_partial_refresh_scene: Option<String>,
    pub query_scene: Option<String>,
    pub super_link_item_ids: Option<String>,
    pub flow_version: String,
    pub extra_fields: Option<Map<String, Value>>,
}

pub fn build_item_detail_request(item_id: &str, opts: &DetailOptions) -> Result<RequestPlan> {
    require_item_id(item_id)?;
    if matches!(
        opts.enter_type.as_deref(),
        Some("newPublish") | Some("edit")
    ) {
        return Err(anyhow!(
            "enter_type 'newPublish'/'edit' selects the non-unit detail API (mtop.taobao.idle.awesome.detail 1.0) in the app; this builder only builds the unit API"
        ));
    }
    let query_scene = blank_to_none(opts.query_scene.as_deref());
    let mut data = Map::new();
    data.insert("itemId".into(), json!(item_id));
    insert_str(&mut data, "city", opts.city.as_deref());
    insert_str(
        &mut data,
        "fishpoolTopicId",
        blank_to_none(opts.fishpool_topic_id.as_deref()),
    );
    insert_str(
        &mut data,
        "grouponId",
        blank_to_none(opts.groupon_id.as_deref()),
    );
    insert_str(
        &mut data,
        "enterType",
        blank_to_none(opts.enter_type.as_deref()),
    );
    data.insert("needSimpleDetail".into(), json!(opts.need_simple_detail));
    insert_str(&mut data, "abInfo", opts.ab_info.as_deref());
    insert_str(&mut data, "extra", opts.extra.as_deref());
    insert_str(&mut data, "extraParam", opts.extra_param.as_deref());
    insert_str(
        &mut data,
        "commerceAdPlanId",
        blank_to_none(opts.commerce_ad_plan_id.as_deref()),
    );
    insert_str(
        &mut data,
        "clientPartialRefreshScene",
        blank_to_none(opts.client_partial_refresh_scene.as_deref()),
    );
    insert_str(&mut data, "queryScene", query_scene);
    // P-31: the app only copies superLinkItemIds when queryScene is non-empty.
    if query_scene.is_some() {
        insert_str(
            &mut data,
            "superLinkItemIds",
            blank_to_none(opts.super_link_item_ids.as_deref()),
        );
    }
    let flow_version = if opts.flow_version.is_empty() {
        "8.0"
    } else {
        opts.flow_version.as_str()
    };
    data.insert("flowVersion".into(), json!(flow_version));
    data.insert("isOld".into(), json!(false));
    data.insert("isBundleGroup".into(), json!(opts.is_bundle_group));
    data.insert("hitNativeDetail".into(), json!(true));
    merge(&mut data, opts.extra_fields.as_ref());
    plan("mtop.taobao.idle.awesome.detail.unit", data, None)
}

// ---------------------------------------------------------------------------
// Public comments: mtop.taobao.idle.comment.list 5.0
// ---------------------------------------------------------------------------

pub fn build_comment_list_request(
    item_id: &str,
    rows_per_page: i64,
    fish_ad_code: Option<&str>,
    ad_item_id: Option<&str>,
) -> Result<RequestPlan> {
    require_item_id(item_id)?;
    let mut data = Map::new();
    data.insert("itemId".into(), json!(item_id));
    data.insert("rowsPerPage".into(), json!(rows_per_page));
    insert_str(&mut data, "fishAdCode", fish_ad_code);
    insert_str(&mut data, "adItemId", ad_item_id);
    plan("mtop.taobao.idle.comment.list", data, None)
}

#[allow(clippy::too_many_arguments)]
pub fn build_fun_comment_list_request(
    target_id: &str,
    target_type: &str,
    namespace: &str,
    source_id: &str,
    begin_cursor: Option<&str>,
    page_size: i64,
    rows_per_page: i64,
    top_comment_id: Option<&str>,
) -> Result<RequestPlan> {
    let mut data = Map::new();
    data.insert("targetId".into(), json!(target_id));
    data.insert("targetType".into(), json!(target_type));
    data.insert("namespace".into(), json!(namespace));
    data.insert("sourceId".into(), json!(source_id));
    data.insert("pageSize".into(), json!(page_size));
    data.insert("rowsPerPage".into(), json!(rows_per_page));
    insert_str(&mut data, "beginCursor", begin_cursor);
    insert_str(&mut data, "topCommentId", top_comment_id);
    plan("mtop.taobao.idle.fun.general.comment.list", data, None)
}

// ---------------------------------------------------------------------------
// Rates: mtop.taobao.idle.rate.list 6.0 (pageNumber is a String)
// ---------------------------------------------------------------------------

pub fn build_rate_list_request(
    item_id: &str,
    rated_uid: &str,
    page_number: &str,
) -> Result<RequestPlan> {
    require_item_id(item_id)?;
    let mut data = Map::new();
    data.insert("itemId".into(), json!(item_id));
    data.insert("ratedUid".into(), json!(rated_uid));
    data.insert("rateType".into(), json!("0"));
    data.insert("pageNumber".into(), json!(page_number));
    data.insert("foldFlag".into(), json!("0"));
    data.insert("raterType".into(), json!("0"));
    data.insert("rowsPerPage".into(), json!(20));
    plan("mtop.taobao.idle.rate.list", data, None)
}

// ---------------------------------------------------------------------------
// User head info: mtop.taobao.idle.user.headinfo.get 4.0
// ---------------------------------------------------------------------------

/// Build a user.headinfo.get plan from the statically typed request
/// class (classes10 `ApiUserHeadinfoGetRequest`, see
/// seller-headinfo-api-trace.md §3): `userId` (String, the primary
/// target selector), `nick` (String, alternative selector) and
/// `myUserId` (Long, the viewer's own id). `None` fields are omitted.
/// The class's `appKey` field is *not* part of the data map: it is
/// supplied by the sending layer (the wire form carries the public
/// app key as the `x-appkey` header; see `protocol::build_form`).
///
/// Caveat, recorded in round 36: no construction site for this
/// request class exists in any of the 11 DEX files of this APK
/// version — it is a legacy fleamarket-layer definition with no
/// confirmed in-app caller. The request *shape* is statically
/// determined, so the builder exists, but the API is unmeasured live
/// and its in-app reachability is unconfirmed.
pub fn build_user_headinfo_request(
    user_id: Option<&str>,
    nick: Option<&str>,
    my_user_id: Option<i64>,
) -> Result<RequestPlan> {
    let mut data = Map::new();
    insert_str(&mut data, "userId", user_id);
    insert_str(&mut data, "nick", nick);
    if let Some(id) = my_user_id {
        data.insert("myUserId".into(), json!(id));
    }
    plan("mtop.taobao.idle.user.headinfo.get", data, None)
}

// ---------------------------------------------------------------------------
// Seller / own published items: mtop.taobao.idle.user.publish.items 6.0
// ---------------------------------------------------------------------------

/// Build a user.publish.items plan from the only statically
/// confirmed consumer (classes10 `MyIdleItemListController`, see
/// seller-public-items-trace.md §2): its parameter object carries
/// exactly `pageNumber` (int, default 1) and `pageSize` (int,
/// default 20), sent through the generic `ApiProtocol` route.
///
/// Caveat: the confirmed request has **no target-user parameter**.
/// The confirmed consumer is the message screen's own-item selection
/// (the signed-in user's own listings); whether another user's page
/// uses this API, and with what selector field, was not confirmed by
/// static analysis. This builder therefore deliberately exposes no
/// user selector — do not treat the resulting call as a public
/// other-user listing API.
pub fn build_seller_items_request(page_number: i64, page_size: i64) -> Result<RequestPlan> {
    let page_number = require_positive_int("page_number", page_number)?;
    let page_size = require_positive_int("page_size", page_size)?;
    let mut data = Map::new();
    data.insert("pageNumber".into(), json!(page_number));
    data.insert("pageSize".into(), json!(page_size));
    plan("mtop.taobao.idle.user.publish.items", data, None)
}

// ---------------------------------------------------------------------------
// Recommendations: mtop.taobao.idle.item.recommend.list 5.0
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct RecommendOptions {
    pub page_num: i64,
    pub page_size: i64,
    pub current_size: i64,
    pub history_item_ids: Vec<String>,
    pub req_params: Option<Map<String, Value>>,
    pub tab_id: Option<Value>,
    pub leaf_cat_ids: Option<Vec<Value>>,
    pub predict_trace: Option<String>,
    pub tab_extra: Option<Map<String, Value>>,
    pub extra_fields: Option<Map<String, Value>>,
}

impl RecommendOptions {
    pub fn new() -> Self {
        Self {
            page_num: 0,
            page_size: 20,
            current_size: 0,
            history_item_ids: Vec::new(),
            req_params: None,
            tab_id: None,
            leaf_cat_ids: None,
            predict_trace: None,
            tab_extra: None,
            extra_fields: None,
        }
    }
}

pub fn build_recommend_request(opts: &RecommendOptions) -> Result<RequestPlan> {
    let mut data = opts.req_params.clone().unwrap_or_default();
    if let Some(extra) = data.get("extra") {
        if !extra.is_null() && !extra.is_object() {
            return Err(anyhow!(
                "req_params['extra'] must be a JSON object (mapping); RecommendReq.extra is a JSONObject"
            ));
        }
    }
    if let Some(tab_id) = &opts.tab_id {
        data.insert("tabId".into(), tab_id.clone());
    }
    if let Some(leaf) = &opts.leaf_cat_ids {
        data.insert("xyLeafCatIdList".into(), Value::Array(leaf.clone()));
    }
    insert_str(&mut data, "predictTrace", opts.predict_trace.as_deref());
    if let Some(tab_extra) = &opts.tab_extra {
        let mut merged_extra = match data.get("extra") {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        for (k, v) in tab_extra {
            merged_extra.insert(k.clone(), v.clone());
        }
        data.insert("extra".into(), Value::Object(merged_extra));
    }
    data.entry("isMiddle".to_string()).or_insert(json!(false));
    data.entry("isWaterful".to_string()).or_insert(json!(false));
    data.insert("pageNum".into(), json!(opts.page_num));
    data.insert("pageSize".into(), json!(opts.page_size));
    data.insert("currentSize".into(), json!(opts.current_size));
    data.insert(
        "historyItemIds".into(),
        json!(opts.history_item_ids.join(",")),
    );
    merge(&mut data, opts.extra_fields.as_ref());
    plan("mtop.taobao.idle.item.recommend.list", data, None)
}

// ---------------------------------------------------------------------------
// Home feed: mtop.taobao.idlehome.home.nextfresh 8.0
// ---------------------------------------------------------------------------

pub const HOME_SPM_PREFIX: &str = "a2170.7897990.6801272.";
pub const HOME_TRACK_NAME: &str = "Item";

#[derive(Debug, Clone)]
pub struct HomeFeedOptions {
    pub page_number: i64,
    pub last_response_count: i64,
    pub client_recycle_params: Option<String>,
    pub city: Option<String>,
    pub spm_prefix: String,
    pub track_name: String,
    pub is_wide_page: bool,
    pub ut_data_json: Option<String>,
    pub luxury_custom_data: Option<String>,
    pub extra_fields: Option<Map<String, Value>>,
}

impl Default for HomeFeedOptions {
    fn default() -> Self {
        Self {
            page_number: 1,
            last_response_count: 0,
            client_recycle_params: None,
            city: None,
            spm_prefix: HOME_SPM_PREFIX.to_string(),
            track_name: HOME_TRACK_NAME.to_string(),
            is_wide_page: false,
            ut_data_json: None,
            luxury_custom_data: None,
            extra_fields: None,
        }
    }
}

pub fn build_home_feed_request(opts: &HomeFeedOptions) -> Result<RequestPlan> {
    let mut data = Map::new();
    data.insert("pageNumber".into(), json!(opts.page_number));
    data.insert("lastResponseCount".into(), json!(opts.last_response_count));
    insert_str(
        &mut data,
        "clientRecycleParams",
        opts.client_recycle_params.as_deref(),
    );
    insert_str(&mut data, "city", opts.city.as_deref());
    data.insert("spmPrefix".into(), json!(opts.spm_prefix));
    data.insert("trackName".into(), json!(opts.track_name));
    data.insert(
        "data".into(),
        json!(opts.ut_data_json.as_deref().unwrap_or("{}")),
    );
    data.insert("isWidePage".into(), json!(opts.is_wide_page));
    data.insert(
        "luxuryCustomData".into(),
        json!(opts.luxury_custom_data.as_deref().unwrap_or("{}")),
    );
    if opts.page_number == 1 {
        data.insert("needCustomsUrlParams".into(), json!(true));
    }
    merge(&mut data, opts.extra_fields.as_ref());
    plan("mtop.taobao.idlehome.home.nextfresh", data, None)
}

// ---------------------------------------------------------------------------
// City feed: mtop.taobao.idle.local.home(.feeds) — version is tabConfig-driven
// ---------------------------------------------------------------------------

pub const CITY_SPM_PREFIX: &str = "a2170.14016119.6802272.";
pub const CITY_CUSTOM_REQ_TYPE: &str = "idle_local_feeds";

#[derive(Debug, Clone)]
pub struct CityFeedOptions {
    pub version: String,
    pub data_source_id: String,
    pub tab_name: Option<String>,
    pub page_number: String,
    pub last_response_count: String,
    pub keywords: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub feeds_api: bool,
    pub extra_fields: Option<Map<String, Value>>,
}

pub fn build_city_feed_request(opts: &CityFeedOptions) -> Result<RequestPlan> {
    let mut data = Map::new();
    data.insert("dataSourceId".into(), json!(opts.data_source_id));
    data.insert("pageNumber".into(), json!(opts.page_number));
    data.insert("lastResponseCount".into(), json!(opts.last_response_count));
    data.insert("name".into(), json!("SAME_CITY"));
    data.insert("needFrontPage".into(), json!("false"));
    data.insert("needBanner".into(), json!("true"));
    data.insert("needMario".into(), json!("false"));
    data.insert("showAd".into(), json!("1"));
    data.insert("customReqType".into(), json!(CITY_CUSTOM_REQ_TYPE));
    data.insert("spmPrefix".into(), json!(CITY_SPM_PREFIX));
    data.insert("trackName".into(), json!("Item"));
    data.insert(
        "keyWords".into(),
        json!(opts.keywords.as_deref().unwrap_or("")),
    );
    insert_str(&mut data, "itemName", opts.tab_name.as_deref());
    if let Some(lat) = opts.latitude {
        data.insert("latitude".into(), json!(lat));
    }
    if let Some(lng) = opts.longitude {
        data.insert("longitude".into(), json!(lng));
    }
    merge(&mut data, opts.extra_fields.as_ref());
    let api_name = if opts.feeds_api {
        "mtop.taobao.idle.local.home.feeds"
    } else {
        "mtop.taobao.idle.local.home"
    };
    plan(api_name, data, Some(&opts.version))
}

pub fn build_city_nearby_request(
    city: Option<&str>,
    latitude: Option<&str>,
    longitude: Option<&str>,
    select_city: Option<&str>,
) -> Result<RequestPlan> {
    let mut data = Map::new();
    insert_str(&mut data, "city", city);
    insert_str(&mut data, "latitude", latitude);
    insert_str(&mut data, "longitude", longitude);
    insert_str(&mut data, "selectCity", select_city);
    plan("mtop.taobao.idle.local.near.by.corner.info", data, None)
}

// ---------------------------------------------------------------------------
// Similar items: mtop.taobao.idle.search.spusimilar.itemlist 1.0
// ---------------------------------------------------------------------------

pub fn build_spusimilar_request(
    item_id: &str,
    conditions: Option<Map<String, Value>>,
    page_no: i64,
    page_size: i64,
) -> Result<RequestPlan> {
    require_item_id(item_id)?;
    let mut data = Map::new();
    data.insert("itemId".into(), json!(item_id));
    if let Some(c) = conditions {
        data.insert("conditions".into(), Value::Object(c));
    }
    data.insert("pageNo".into(), json!(page_no));
    data.insert("pageSize".into(), json!(page_size));
    plan("mtop.taobao.idle.search.spusimilar.itemlist", data, None)
}

// ---------------------------------------------------------------------------
// Fish ponds
// ---------------------------------------------------------------------------

pub const FISHPOND_ASAC: &str = "D4JVA4ORIFHK5PGKBVPA";

fn require_pool_id(value: i64) -> Result<i64> {
    if value <= 0 {
        return Err(anyhow!("pool_id must be positive, got {value}"));
    }
    Ok(value)
}

pub fn build_fishpond_list_request(
    page_number: i64,
    rows_per_page: i64,
    nearby: bool,
    include_star_pool: Option<bool>,
    extra_fields: Option<&Map<String, Value>>,
) -> Result<RequestPlan> {
    if nearby {
        return Err(anyhow!(
            "nearby=true routes to com.taobao.idle.fishpool.nearby.list in the app, which is not covered by this builder"
        ));
    }
    let mut data = Map::new();
    data.insert("pageNumber".into(), json!(page_number));
    data.insert("rowsPerPage".into(), json!(rows_per_page));
    data.insert("nearby".into(), json!(false));
    data.insert("type".into(), json!(1));
    data.insert("asac".into(), json!(FISHPOND_ASAC));
    if let Some(v) = include_star_pool {
        data.insert("includeStarPool".into(), json!(v));
    }
    merge(&mut data, extra_fields);
    plan("com.taobao.idle.fishpool.list", data, None)
}

pub fn build_fishpond_info_request(
    pool_id: i64,
    extra_fields: Option<&Map<String, Value>>,
) -> Result<RequestPlan> {
    let mut data = Map::new();
    data.insert("fishPoolId".into(), json!(require_pool_id(pool_id)?));
    data.insert("type".into(), json!(4));
    data.insert("asac".into(), json!(FISHPOND_ASAC));
    merge(&mut data, extra_fields);
    plan("mtop.taobao.idle.fishpool.id", data, None)
}

pub fn build_fishpond_comments_request(
    pool_id: i64,
    page_number: i64,
    latitude: Option<f64>,
    longitude: Option<f64>,
    extra_fields: Option<&Map<String, Value>>,
) -> Result<RequestPlan> {
    let mut data = Map::new();
    data.insert("poolId".into(), json!(require_pool_id(pool_id)?));
    data.insert("pageNumber".into(), json!(page_number));
    if let Some(lat) = latitude {
        data.insert("lat".into(), json!(lat));
    }
    if let Some(lng) = longitude {
        data.insert("lang".into(), json!(lng));
    }
    merge(&mut data, extra_fields);
    plan("com.taobao.idle.fishpool.comment.list", data, None)
}

// ---------------------------------------------------------------------------
// Video play metadata: com.taobao.idle.video.play 5.0
// ---------------------------------------------------------------------------

pub fn build_video_play_request(
    video_id: i64,
    item_id: i64,
    extra_fields: Option<&Map<String, Value>>,
) -> Result<RequestPlan> {
    if video_id <= 0 {
        return Err(anyhow!("video_id must be positive, got {video_id}"));
    }
    if item_id < 0 {
        return Err(anyhow!("item_id must be 0 or positive, got {item_id}"));
    }
    let mut data = Map::new();
    data.insert("itemid".into(), json!(item_id));
    data.insert("videoid".into(), json!(video_id));
    merge(&mut data, extra_fields);
    plan("com.taobao.idle.video.play", data, None)
}

/// Build a video.play plan from a normalized feed card (the JSON form
/// produced by `parse_feed_item_card`: keys item_id / video_id /
/// has_video). Port of `build_video_play_request_from_feed_card`.
pub fn build_video_play_request_from_feed_card(
    card: &Value,
    item_id: Option<i64>,
    extra_fields: Option<&Map<String, Value>>,
) -> Result<RequestPlan> {
    let video_id_v = card.get("video_id");
    let has_video_v = card.get("has_video");
    if video_id_v.is_none() && has_video_v.is_none() {
        return Err(anyhow!(
            "card does not expose the FeedItemCard video view (video_id / has_video attributes)"
        ));
    }
    let has_video = has_video_v.and_then(|v| v.as_bool());
    if has_video != Some(true) || video_id_v.map(|v| v.is_null()).unwrap_or(true) {
        return Err(anyhow!(
            "card carries no confirmed video id (has_video is not true or videoInfo.videoId is missing)"
        ));
    }
    let video_id = match video_id_v.unwrap() {
        Value::Number(n) => n
            .as_i64()
            .ok_or_else(|| anyhow!("video_id must be an integer"))?,
        Value::String(s) => s
            .parse::<i64>()
            .map_err(|_| anyhow!("video_id must be a numeric string"))?,
        _ => return Err(anyhow!("video_id must be int (Long)")),
    };
    let resolved_item_id = match item_id {
        None => 0,
        Some(iid) => {
            if let Some(card_item_id) = card.get("item_id") {
                let card_str = match card_item_id {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                };
                if let Some(cs) = card_str {
                    if cs != iid.to_string() {
                        return Err(anyhow!(
                            "item_id does not match the card's own item_id; refusing to pair one item's id with another's video"
                        ));
                    }
                }
            }
            iid
        }
    };
    build_video_play_request(video_id, resolved_item_id, extra_fields)
}

// ---------------------------------------------------------------------------
// Search pagination state (pagination.py SearchPagination)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct SearchPagination {
    pub page_number: i64,
    pub relate_page_number: i64,
    pub result_list_last_index: i64,
    pub relate_result_list_last_index: i64,
    pub sqi_control_fields_json: Option<String>,
    pub extra_filter_value: Option<String>,
}

impl SearchPagination {
    pub fn new() -> Self {
        Self {
            page_number: 1,
            relate_page_number: 1,
            result_list_last_index: 0,
            relate_result_list_last_index: 0,
            sqi_control_fields_json: None,
            extra_filter_value: None,
        }
    }

    /// Restart from page 1 (condition change / refresh), port of
    /// Python `SearchPagination.reset` (round 30 correction): page
    /// counters, both LastIndex accumulators and the sqi carry-over
    /// return to their initial values, mirroring the app's
    /// resetPageNumber + resetSqiControlFieldsJson.
    /// `extra_filter_value` is preserved on purpose: the app's
    /// refresh never clears it — `updateOtherParams` replaces it
    /// with the server-returned form after each refresh-type
    /// response and that form is carried into the next
    /// condition-changed send. Use a fresh state for a genuinely new
    /// search context.
    pub fn reset(&mut self) {
        self.page_number = 1;
        self.relate_page_number = 1;
        self.result_list_last_index = 0;
        self.relate_result_list_last_index = 0;
        self.sqi_control_fields_json = None;
        // extra_filter_value intentionally preserved (see docstring).
    }

    /// Fields for the next request; `firstRequest` is present with a
    /// null value, which `with_fields` turns into key removal.
    pub fn request_fields(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("pageNumber".into(), json!(self.page_number));
        out.insert(
            "relateResultPageNumber".into(),
            json!(self.relate_page_number),
        );
        out.insert(
            "resultListLastIndex".into(),
            json!(self.result_list_last_index),
        );
        out.insert(
            "relateResultListLastIndex".into(),
            json!(self.relate_result_list_last_index),
        );
        out.insert("firstRequest".into(), Value::Null);
        if let Some(sqi) = &self.sqi_control_fields_json {
            out.insert("sqiControlFieldsJson".into(), json!(sqi));
        }
        if let Some(efv) = &self.extra_filter_value {
            out.insert("extraFilterValue".into(), json!(efv));
        }
        out
    }

    /// Consume one parsed search page (the normalized Value produced by
    /// `parse_search_response`); returns true when another page should
    /// be requested.
    pub fn advance(&mut self, parsed_page: &Value) -> bool {
        if let Some(info) = parsed_page.get("result_info") {
            if let Some(sqi) = info.get("sqiControlFields") {
                if !sqi.is_null() {
                    self.sqi_control_fields_json = Some(match sqi {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                }
            }
        }
        if let Some(raw) = parsed_page.get("raw_data") {
            if let Some(efv) = raw.get("extraFilterValue") {
                if !efv.is_null() {
                    self.extra_filter_value = Some(match efv {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    });
                }
            }
        }
        if parsed_page.get("has_next_page").and_then(|v| v.as_bool()) != Some(true) {
            return false;
        }
        let items_len = parsed_page
            .get("items")
            .and_then(|v| v.as_array())
            .map(|a| a.len() as i64)
            .unwrap_or(0);
        let relate_len = parsed_page
            .get("relate_items")
            .and_then(|v| v.as_array())
            .map(|a| a.len() as i64)
            .unwrap_or(0);
        self.result_list_last_index += items_len;
        self.relate_result_list_last_index += relate_len;
        if items_len > 0 {
            self.page_number += 1;
        }
        if relate_len > 0 {
            self.relate_page_number += 1;
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Recommend / home-feed / pageNo pagination states (pagination.py)
// ---------------------------------------------------------------------------

/// `recommend_accumulate` state (RecommendViewModel), port of Python
/// `RecommendPagination`: refresh resets page_num/current_size to 0;
/// load-more increments page_num and current_size accumulates the
/// returned card count. `history_item_ids` are the already displayed
/// item ids, regenerated for every request.
#[derive(Debug, Clone)]
pub struct RecommendPagination {
    pub page_num: i64,
    pub page_size: i64,
    pub current_size: i64,
    pub history_item_ids: Vec<String>,
}

impl Default for RecommendPagination {
    fn default() -> Self {
        Self {
            page_num: 0,
            page_size: 20,
            current_size: 0,
            history_item_ids: Vec::new(),
        }
    }
}

impl RecommendPagination {
    pub fn new() -> Self {
        Self::default()
    }

    /// Refresh semantics: page_num/current_size restart at 0 and the
    /// displayed-item history is dropped. `page_size` is
    /// configuration and is preserved.
    pub fn reset(&mut self) {
        self.page_num = 0;
        self.current_size = 0;
        self.history_item_ids.clear();
    }

    /// Fields for the next request (`historyItemIds` is always
    /// assigned by fromFeedsModel — `""` when nothing shown).
    pub fn request_fields(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("pageNum".into(), json!(self.page_num));
        out.insert("pageSize".into(), json!(self.page_size));
        out.insert("currentSize".into(), json!(self.current_size));
        out.insert(
            "historyItemIds".into(),
            json!(self.history_item_ids.join(",")),
        );
        out
    }

    /// Consume one parsed recommend page (the normalized Value from
    /// `parse_recommend_response`: `cards[].item.item_id`,
    /// `has_more`); returns true when another page should be
    /// requested. Mirrors the Python state exactly: current_size and
    /// history accumulate even on the final page; page_num advances
    /// only when `has_more` is true.
    pub fn advance(&mut self, parsed_page: &Value) -> bool {
        let cards = parsed_page
            .get("cards")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        self.current_size += cards.len() as i64;
        for card in &cards {
            if let Some(id) = card.pointer("/item/item_id").and_then(|v| match v {
                Value::String(s) => Some(s.clone()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            }) {
                self.history_item_ids.push(id);
            }
        }
        if parsed_page.get("has_more").and_then(|v| v.as_bool()) != Some(true) {
            return false;
        }
        self.page_num += 1;
        true
    }

    /// Apply this state to recommend options (the caller keeps its
    /// req_params / tab fields; only the accumulate fields move).
    pub fn apply_to(&self, opts: &mut RecommendOptions) {
        opts.page_num = self.page_num;
        opts.page_size = self.page_size;
        opts.current_size = self.current_size;
        opts.history_item_ids = self.history_item_ids.clone();
    }
}

/// Request-data keys controlled by RecommendPagination.
pub const RECOMMEND_PAGING_FIELDS: &[&str] =
    &["pageNum", "pageSize", "currentSize", "historyItemIds"];

/// `home_recycle_carryover` state (HomeRecommendReq.updatePageNumber),
/// port of Python `HomePagination`: page_number advances only when
/// the previous sections were non-empty; last_response_count
/// accumulates feedsCount; client_recycle_params is copied verbatim
/// from the previous response.
#[derive(Debug, Clone, Default)]
pub struct HomePagination {
    pub page_number: i64,
    pub last_response_count: i64,
    pub client_recycle_params: Option<String>,
}

impl HomePagination {
    pub fn new() -> Self {
        Self {
            page_number: 1,
            last_response_count: 0,
            client_recycle_params: None,
        }
    }

    pub fn reset(&mut self) {
        self.page_number = 1;
        self.last_response_count = 0;
        self.client_recycle_params = None;
    }

    /// Fields for the next request; `needCustomsUrlParams` is present
    /// with a null value, which `with_fields` turns into key removal
    /// (HomeRecommendReq.updatePageNumber nulls it when advancing;
    /// requestMtop only re-adds it for pageNumber == 1, which an
    /// advanced state never is).
    pub fn request_fields(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("pageNumber".into(), json!(self.page_number));
        out.insert("lastResponseCount".into(), json!(self.last_response_count));
        out.insert("needCustomsUrlParams".into(), Value::Null);
        if let Some(crp) = &self.client_recycle_params {
            out.insert("clientRecycleParams".into(), json!(crp));
        }
        out
    }

    /// Consume one parsed home-feed page (the normalized Value from
    /// `parse_home_feed_response`: `sections`, `feeds_count`,
    /// `next_page`, `client_recycle_params`); returns true when
    /// another page should be requested. The counters accumulate
    /// whenever the previous sections were non-empty; next_page only
    /// governs whether the caller continues.
    pub fn advance(&mut self, parsed_page: &Value) -> bool {
        let sections_len = parsed_page
            .get("sections")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let feeds = parsed_page
            .get("feeds_count")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if let Some(crp) = parsed_page
            .get("client_recycle_params")
            .and_then(|v| v.as_str())
        {
            self.client_recycle_params = Some(crp.to_string());
        }
        if sections_len > 0 {
            self.page_number += 1;
            self.last_response_count += feeds;
        }
        parsed_page.get("next_page").and_then(|v| v.as_bool()) == Some(true)
    }

    /// Apply this state to home-feed options.
    pub fn apply_to(&self, opts: &mut HomeFeedOptions) {
        opts.page_number = self.page_number;
        opts.last_response_count = self.last_response_count;
        opts.client_recycle_params = self.client_recycle_params.clone();
    }
}

/// Request-data keys controlled by HomePagination.
pub const HOME_PAGING_FIELDS: &[&str] = &[
    "pageNumber",
    "lastResponseCount",
    "clientRecycleParams",
    "needCustomsUrlParams",
];

/// `flag_nextpage_pageno` state, port of Python `PageNoPagination`:
/// pageNo (int) advances while nextPage is true. Used by
/// search.spusimilar.itemlist 1.0 (round 16): the request carries
/// pageNo / pageSize (initial values 1 / 10 confirmed in the Dart
/// constructor) and the response carries a nextPage bool.
#[derive(Debug, Clone)]
pub struct PageNoPagination {
    pub page_no: i64,
    pub page_size: i64,
}

impl Default for PageNoPagination {
    fn default() -> Self {
        Self {
            page_no: 1,
            page_size: 10,
        }
    }
}

impl PageNoPagination {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn reset(&mut self) {
        self.page_no = 1;
    }

    pub fn request_fields(&self) -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("pageNo".into(), json!(self.page_no));
        out.insert("pageSize".into(), json!(self.page_size));
        out
    }

    /// Consume one parsed page carrying `next_page`; returns true
    /// when another page should be requested.
    pub fn advance(&mut self, parsed_page: &Value) -> bool {
        if parsed_page.get("next_page").and_then(|v| v.as_bool()) != Some(true) {
            return false;
        }
        self.page_no += 1;
        true
    }
}

/// Request-data keys controlled by PageNoPagination.
pub const PAGE_NO_PAGING_FIELDS: &[&str] = &["pageNo", "pageSize"];

/// Request-data keys controlled by SearchPagination (client.py's
/// condition-fingerprint exclusion set).
pub const SEARCH_PAGING_FIELDS: &[&str] = &[
    "pageNumber",
    "relateResultPageNumber",
    "firstRequest",
    "sqiControlFieldsJson",
    "extraFilterValue",
    "resultListLastIndex",
    "relateResultListLastIndex",
];

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from goofish-client/tests/test_requests.py
    #[test]
    fn search_defaults_and_omission() {
        let plan = build_search_request(Some("中古 自転車"), &SearchOptions::default()).unwrap();
        assert_eq!(plan.api_name, "mtop.taobao.idlemtopsearch.search");
        assert_eq!(plan.version, "1.0");
        let d = &plan.data;
        assert_eq!(d["keyword"], json!("中古 自転車"));
        assert_eq!(d["pageNumber"], json!(1));
        assert_eq!(d["rowsPerPage"], json!(10));
        assert_eq!(d["page"], json!(1));
        assert_eq!(d["bizFrom"], json!("home"));
        assert_eq!(d["supportFlexFilter"], json!(true));
        assert_eq!(d["smartUIFilter"], json!(true));
        let ab: Value = serde_json::from_str(d["abConfigs"].as_str().unwrap()).unwrap();
        assert_eq!(
            ab,
            json!({"isSearchResultNative": "true", "isEnterResultNative": "true"})
        );
        for key in [
            "gps",
            "latitude",
            "longitude",
            "fishAdCode",
            "userPositionJson",
        ] {
            assert!(d.get(key).is_none(), "{key} must be omitted");
        }
    }

    #[test]
    fn search_null_fields_omitted() {
        let plan = build_search_request(None, &SearchOptions::default()).unwrap();
        assert!(plan.data.get("keyword").is_none());
        assert_eq!(plan.data["firstRequest"], json!(true));
        assert!(plan.data.get("propValueStr").is_none());
        assert_eq!(
            plan.data["extraFilterValue"],
            json!("{\"divisionList\":[{}]}")
        );
    }

    #[test]
    fn search_first_request_tristate() {
        let mut opts = SearchOptions {
            first_request: None,
            ..Default::default()
        };
        let plan = build_search_request(Some("x"), &opts).unwrap();
        assert!(plan.data.get("firstRequest").is_none());
        opts.first_request = Some(false);
        let plan = build_search_request(Some("x"), &opts).unwrap();
        assert_eq!(plan.data["firstRequest"], json!(false));
    }

    #[test]
    fn search_with_filters_and_from_filter_flag() {
        let filters = SearchFilters {
            start_price: Some("5".into()),
            ..Default::default()
        };
        let opts = SearchOptions {
            filters: Some(filters),
            from_filter: true,
            ..Default::default()
        };
        let plan = build_search_request(Some("x"), &opts).unwrap();
        assert_eq!(plan.data["startPrice"], json!("5"));
        assert_eq!(plan.data["fromFilter"], json!(true));
        assert_eq!(plan.data["fromCombo"], json!("Filter"));
    }

    #[test]
    fn search_filters_prop_value_str_and_region() {
        let filters = SearchFilters {
            prop_selections: vec![
                ("1".into(), vec!["2".into(), "2".into(), "3".into()]),
                ("9".into(), vec![]),
            ],
            price_range_value: Some("100,500".into()),
            sort_field: Some("create".into()),
            sort_value: Some("desc".into()),
            province: Some("浙江".into()),
            city: Some("杭州".into()),
            ..Default::default()
        };
        let fields = filters.to_search_fields().unwrap();
        assert_eq!(
            fields["propValueStr"],
            json!("{\"searchFilter\":\"1:2,3;priceRange:100,500\"}")
        );
        assert_eq!(fields["sortField"], json!("create"));
        // extraFilterValue key order: area, city, province
        assert_eq!(
            fields["extraFilterValue"],
            json!("{\"divisionList\":[{\"city\":\"杭州\",\"province\":\"浙江\"}]}")
        );
    }

    #[test]
    fn search_filters_validation() {
        // lone sort half
        let filters = SearchFilters {
            sort_field: Some("create".into()),
            ..Default::default()
        };
        assert!(filters.to_search_fields().is_err());
        // bad price range forms
        assert!(validate_price_range("100-200").is_err());
        assert!(validate_price_range(",").is_err());
        assert_eq!(validate_price_range(",500").unwrap(), ",500");
        assert_eq!(validate_price_range("100,").unwrap(), "100,");
        // page validation
        let opts = SearchOptions {
            page_number: 0,
            ..Default::default()
        };
        assert!(build_search_request(Some("x"), &opts).is_err());
    }

    #[test]
    fn prop_value_str_round_trip() {
        let (sel, pr) =
            parse_prop_value_str("{\"searchFilter\":\"1:2,3;priceRange:100,500\"}").unwrap();
        assert_eq!(
            sel,
            vec![("1".to_string(), vec!["2".to_string(), "3".to_string()])]
        );
        assert_eq!(pr.as_deref(), Some("100,500"));
        assert_eq!(build_prop_value_str(&[], None).unwrap(), None);
    }

    #[test]
    fn search_extra_fields_passthrough_verbatim() {
        let mut extra = Map::new();
        extra.insert("gps".into(), json!("30.1,120.2"));
        let opts = SearchOptions {
            extra_fields: Some(extra),
            ..Default::default()
        };
        let plan = build_search_request(Some("x"), &opts).unwrap();
        assert_eq!(plan.data["gps"], json!("30.1,120.2"));
    }

    #[test]
    fn search_data_json_is_valid_json() {
        let filters = SearchFilters {
            prop_selections: vec![("1".into(), vec!["2".into()])],
            ..Default::default()
        };
        let opts = SearchOptions {
            filters: Some(filters),
            ..Default::default()
        };
        let plan = build_search_request(Some("キーワード"), &opts).unwrap();
        let parsed: Value = serde_json::from_str(&plan.data_json()).unwrap();
        assert_eq!(parsed["keyword"], json!("キーワード"));
        assert!(parsed["propValueStr"].is_string());
    }

    #[test]
    fn suggest_request() {
        let plan = build_suggest_request("あ", None, 0, None).unwrap();
        assert_eq!(
            plan.data,
            json!({"inputWords": "あ", "type": 0, "bucketId": 0})
        );
        assert_eq!(plan.version, "4.0");
    }

    #[test]
    fn category_phases() {
        assert_eq!(build_category_request(None).unwrap().data, json!({}));
        assert_eq!(
            build_category_request(Some("手机")).unwrap().data,
            json!({"name": "手机"})
        );
    }

    #[test]
    fn detail_request_defaults() {
        let plan = build_item_detail_request("123456", &DetailOptions::default()).unwrap();
        let d = &plan.data;
        assert_eq!(d["itemId"], json!("123456"));
        assert_eq!(d["flowVersion"], json!("8.0"));
        assert_eq!(d["isOld"], json!(false));
        assert_eq!(d["isBundleGroup"], json!(false));
        assert_eq!(d["hitNativeDetail"], json!(true));
        assert!(d.get("city").is_none());
        assert!(build_item_detail_request("abc", &DetailOptions::default()).is_err());
        assert!(build_item_detail_request("", &DetailOptions::default()).is_err());
    }

    #[test]
    fn detail_super_link_quirk() {
        let mut opts = DetailOptions {
            super_link_item_ids: Some("1,2".into()),
            ..Default::default()
        };
        let plan = build_item_detail_request("123", &opts).unwrap();
        assert!(plan.data.get("superLinkItemIds").is_none());
        opts.query_scene = Some("scene".into());
        let plan = build_item_detail_request("123", &opts).unwrap();
        assert_eq!(plan.data["superLinkItemIds"], json!("1,2"));
        opts.enter_type = Some("edit".into());
        assert!(build_item_detail_request("123", &opts).is_err());
    }

    #[test]
    fn comment_request() {
        let plan = build_comment_list_request("42", 10, None, None).unwrap();
        assert_eq!(plan.data, json!({"itemId": "42", "rowsPerPage": 10}));
    }

    #[test]
    fn fun_comment_request() {
        let plan =
            build_fun_comment_list_request("t1", "item", "ns", "s1", None, 10, 10, None).unwrap();
        assert_eq!(plan.data["targetId"], json!("t1"));
        assert_eq!(plan.data["pageSize"], json!(10));
        assert_eq!(plan.version, "1.0");
        assert!(plan.data.get("beginCursor").is_none());
    }

    #[test]
    fn rate_request_page_number_is_string() {
        let plan = build_rate_list_request("42", "777", "1").unwrap();
        assert_eq!(plan.data["pageNumber"], json!("1"));
        assert!(plan.data["pageNumber"].is_string());
        assert_eq!(plan.data["rowsPerPage"], json!(20));
    }

    #[test]
    fn recommend_request_paging_fields() {
        let mut req_params = Map::new();
        req_params.insert("itemId".into(), json!("1"));
        req_params.insert("tabId".into(), json!("t"));
        let opts = RecommendOptions {
            req_params: Some(req_params),
            history_item_ids: vec!["a".into(), "b".into()],
            ..RecommendOptions::new()
        };
        let plan = build_recommend_request(&opts).unwrap();
        assert_eq!(plan.data["pageNum"], json!(0));
        assert_eq!(plan.data["pageSize"], json!(20));
        assert_eq!(plan.data["currentSize"], json!(0));
        assert_eq!(plan.data["historyItemIds"], json!("a,b"));
        assert_eq!(plan.data["itemId"], json!("1"));
        assert_eq!(plan.data["isMiddle"], json!(false));
    }

    #[test]
    fn home_feed_request_carryover_omitted_on_refresh() {
        assert!(build_home_feed_request(&HomeFeedOptions::default())
            .unwrap()
            .data
            .get("clientRecycleParams")
            .is_none());
        let opts = HomeFeedOptions {
            page_number: 2,
            last_response_count: 30,
            client_recycle_params: Some("RAW-TOKEN".into()),
            ..Default::default()
        };
        let plan = build_home_feed_request(&opts).unwrap();
        assert_eq!(plan.data["clientRecycleParams"], json!("RAW-TOKEN"));
        assert!(plan.data.get("needCustomsUrlParams").is_none());
        let p1 = build_home_feed_request(&HomeFeedOptions::default()).unwrap();
        assert_eq!(p1.data["needCustomsUrlParams"], json!(true));
        assert_eq!(p1.data["spmPrefix"], json!(HOME_SPM_PREFIX));
        assert_eq!(p1.data["data"], json!("{}"));
    }

    #[test]
    fn city_feed_request_string_paging_and_location_omission() {
        let opts = CityFeedOptions {
            version: "1.0".into(),
            data_source_id: "ALL_CITY_FEEDS".into(),
            tab_name: None,
            page_number: "1".into(),
            last_response_count: "0".into(),
            keywords: None,
            latitude: None,
            longitude: None,
            feeds_api: false,
            extra_fields: None,
        };
        let plan = build_city_feed_request(&opts).unwrap();
        let d = &plan.data;
        assert_eq!(d["pageNumber"], json!("1"));
        assert!(d["pageNumber"].is_string());
        assert_eq!(d["lastResponseCount"], json!("0"));
        assert_eq!(d["name"], json!("SAME_CITY"));
        assert_eq!(d["showAd"], json!("1"));
        assert!(d.get("latitude").is_none() && d.get("longitude").is_none());
        let mut opts2 = opts.clone();
        opts2.data_source_id = "X".into();
        opts2.latitude = Some(30.25);
        opts2.longitude = Some(120.16);
        opts2.feeds_api = true;
        let plan2 = build_city_feed_request(&opts2).unwrap();
        assert_eq!(plan2.data["latitude"], json!(30.25));
        assert_eq!(plan2.api_name, "mtop.taobao.idle.local.home.feeds");
        assert_eq!(plan2.version, "1.0");
    }

    #[test]
    fn city_nearby_and_spusimilar() {
        let plan = build_city_nearby_request(Some("杭州"), None, None, None).unwrap();
        assert_eq!(plan.data, json!({"city": "杭州"}));
        let plan = build_spusimilar_request("42", None, 1, 10).unwrap();
        assert_eq!(
            plan.data,
            json!({"itemId": "42", "pageNo": 1, "pageSize": 10})
        );
    }

    #[test]
    fn fishpond_builders() {
        let plan = build_fishpond_list_request(1, 20, false, None, None).unwrap();
        assert_eq!(plan.data["type"], json!(1));
        assert_eq!(plan.data["asac"], json!(FISHPOND_ASAC));
        assert_eq!(plan.data["nearby"], json!(false));
        assert!(build_fishpond_list_request(1, 20, true, None, None).is_err());
        let info = build_fishpond_info_request(88001234, None).unwrap();
        assert_eq!(info.data["fishPoolId"], json!(88001234));
        assert_eq!(info.data["type"], json!(4));
        let comments = build_fishpond_comments_request(88001234, 1, None, None, None).unwrap();
        assert_eq!(comments.data["poolId"], json!(88001234));
        assert!(comments.data.get("fishPoolId").is_none());
        assert!(build_fishpond_info_request(0, None).is_err());
    }

    #[test]
    fn video_play_builder() {
        let plan = build_video_play_request(99, 0, None).unwrap();
        assert_eq!(plan.data, json!({"itemid": 0, "videoid": 99}));
        assert!(build_video_play_request(0, 0, None).is_err());
        assert!(build_video_play_request(1, -1, None).is_err());
        let card = json!({"item_id": "55", "video_id": "77", "has_video": true});
        let plan = build_video_play_request_from_feed_card(&card, None, None).unwrap();
        assert_eq!(plan.data["videoid"], json!(77));
        assert!(build_video_play_request_from_feed_card(&card, Some(56), None).is_err());
        let novideo = json!({"item_id": "55", "has_video": false});
        assert!(build_video_play_request_from_feed_card(&novideo, None, None).is_err());
    }

    #[test]
    fn user_headinfo_request_shape() {
        // Full form: all three typed fields, myUserId stays numeric.
        let plan = build_user_headinfo_request(Some("2211445566"), None, Some(123)).unwrap();
        assert_eq!(plan.api_name, "mtop.taobao.idle.user.headinfo.get");
        assert_eq!(plan.version, "4.0");
        assert_eq!(plan.data, json!({"userId": "2211445566", "myUserId": 123}));
        // nick is the alternative selector; None fields are omitted
        // and appKey never enters the data map (sending-layer field).
        let plan = build_user_headinfo_request(None, Some("示例闲鱼号"), None).unwrap();
        assert_eq!(plan.data, json!({"nick": "示例闲鱼号"}));
        assert!(plan.data.get("appKey").is_none());
        // All-None is the empty map (no invented defaults).
        let plan = build_user_headinfo_request(None, None, None).unwrap();
        assert_eq!(plan.data, json!({}));
    }

    #[test]
    fn seller_items_request_shape() {
        let plan = build_seller_items_request(1, 20).unwrap();
        assert_eq!(plan.api_name, "mtop.taobao.idle.user.publish.items");
        assert_eq!(plan.version, "6.0");
        // Exactly the confirmed consumer's two fields — crucially no
        // target-user / seller field is invented.
        assert_eq!(plan.data, json!({"pageNumber": 1, "pageSize": 20}));
        assert!(build_seller_items_request(0, 20).is_err());
        assert!(build_seller_items_request(1, 0).is_err());
    }

    #[test]
    fn search_pagination_reset_preserves_extra_filter_value() {
        // Round 30: reset() restarts counters and clears the sqi
        // carry-over, but never clears the server extraFilterValue.
        let mut state = SearchPagination::new();
        state.page_number = 3;
        state.result_list_last_index = 40;
        state.sqi_control_fields_json = Some("SQI".into());
        state.extra_filter_value = Some("EFV".into());
        state.reset();
        assert_eq!(state.page_number, 1);
        assert_eq!(state.relate_page_number, 1);
        assert_eq!(state.result_list_last_index, 0);
        assert_eq!(state.sqi_control_fields_json, None);
        assert_eq!(state.extra_filter_value.as_deref(), Some("EFV"));
    }

    #[test]
    fn recommend_pagination_accumulates_like_python() {
        // Port of pagination.py RecommendPagination over parsed
        // recommend pages (cards[].item.item_id + has_more).
        let mut state = RecommendPagination::new();
        let fields = state.request_fields();
        assert_eq!(fields["pageNum"], json!(0));
        assert_eq!(fields["pageSize"], json!(20));
        assert_eq!(fields["currentSize"], json!(0));
        assert_eq!(fields["historyItemIds"], json!(""));
        let page1 = json!({
            "cards": [
                {"item": {"item_id": "11"}},
                {"item": {"item_id": "22"}},
                {"card_type": "ad"},
            ],
            "has_more": true,
        });
        assert!(state.advance(&page1));
        assert_eq!(state.page_num, 1);
        assert_eq!(state.current_size, 3);
        assert_eq!(state.history_item_ids, vec!["11", "22"]);
        // Final page: counters still accumulate, page_num stays.
        let page2 = json!({"cards": [{"item": {"item_id": "33"}}], "has_more": false});
        assert!(!state.advance(&page2));
        assert_eq!(state.page_num, 1);
        assert_eq!(state.current_size, 4);
        assert_eq!(state.history_item_ids, vec!["11", "22", "33"]);
        // Refresh resets to 0 and drops history; page_size is kept.
        state.reset();
        assert_eq!(state.page_num, 0);
        assert_eq!(state.current_size, 0);
        assert!(state.history_item_ids.is_empty());
        assert_eq!(state.page_size, 20);
        // apply_to moves exactly the accumulate fields.
        let mut opts = RecommendOptions::new();
        state.history_item_ids = vec!["7".into()];
        state.apply_to(&mut opts);
        assert_eq!(opts.history_item_ids, vec!["7"]);
        let plan = build_recommend_request(&opts).unwrap();
        assert_eq!(plan.data["historyItemIds"], json!("7"));
    }

    #[test]
    fn home_pagination_recycle_carryover_like_python() {
        // Port of pagination.py HomePagination over parsed home
        // pages (sections / feeds_count / next_page /
        // client_recycle_params). Mirrors the measured round-21/22
        // transition: counts accumulate, the recycle token is copied
        // verbatim, needCustomsUrlParams is nulled on advance.
        let mut state = HomePagination::new();
        let page1 = json!({
            "sections": [{}, {}, {}],
            "feeds_count": 3,
            "next_page": true,
            "client_recycle_params": "RAW-TOKEN",
        });
        assert!(state.advance(&page1));
        assert_eq!(state.page_number, 2);
        assert_eq!(state.last_response_count, 3);
        assert_eq!(state.client_recycle_params.as_deref(), Some("RAW-TOKEN"));
        let fields = state.request_fields();
        assert_eq!(fields["pageNumber"], json!(2));
        assert_eq!(fields["clientRecycleParams"], json!("RAW-TOKEN"));
        assert_eq!(fields["needCustomsUrlParams"], Value::Null);
        // Empty final page: no counter movement, continuation stops,
        // but the response's recycle token is still taken in.
        let page2 = json!({
            "sections": [],
            "feeds_count": 0,
            "next_page": false,
            "client_recycle_params": "RAW-TOKEN-2",
        });
        assert!(!state.advance(&page2));
        assert_eq!(state.page_number, 2);
        assert_eq!(state.last_response_count, 3);
        assert_eq!(state.client_recycle_params.as_deref(), Some("RAW-TOKEN-2"));
        // apply_to + builder: the advanced send drops
        // needCustomsUrlParams exactly as updatePageNumber nulls it.
        let mut opts = HomeFeedOptions::default();
        state.apply_to(&mut opts);
        let plan = build_home_feed_request(&opts).unwrap();
        assert_eq!(plan.data["pageNumber"], json!(2));
        assert_eq!(plan.data["lastResponseCount"], json!(3));
        assert_eq!(plan.data["clientRecycleParams"], json!("RAW-TOKEN-2"));
        assert!(plan.data.get("needCustomsUrlParams").is_none());
        state.reset();
        assert_eq!(state.page_number, 1);
        assert_eq!(state.client_recycle_params, None);
    }

    #[test]
    fn page_no_pagination_advances_on_next_page() {
        let mut state = PageNoPagination::new();
        assert_eq!(state.request_fields()["pageNo"], json!(1));
        assert_eq!(state.request_fields()["pageSize"], json!(10));
        assert!(state.advance(&json!({"next_page": true})));
        assert_eq!(state.page_no, 2);
        assert!(!state.advance(&json!({"next_page": false})));
        assert_eq!(state.page_no, 2);
        state.reset();
        assert_eq!(state.page_no, 1);
    }

    #[test]
    fn with_fields_returns_copy() {
        let plan = build_search_request(Some("x"), &SearchOptions::default()).unwrap();
        let mut overrides = Map::new();
        overrides.insert("pageNumber".into(), json!(2));
        let plan2 = plan.with_fields(&overrides);
        assert_eq!(plan.data["pageNumber"], json!(1));
        assert_eq!(plan2.data["pageNumber"], json!(2));
        // null override removes the key
        let mut rm = Map::new();
        rm.insert("firstRequest".into(), Value::Null);
        let plan3 = plan.with_fields(&rm);
        assert!(plan3.data.get("firstRequest").is_none());
    }
}
