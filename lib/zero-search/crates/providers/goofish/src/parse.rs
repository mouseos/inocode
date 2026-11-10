//! Response parsers — ports of the `goofish_parser` package
//! (mtop.py / search.py / cards.py / item.py / browse.py / explore.py /
//! similar.py). All parsers take a full MTOP envelope and return a
//! normalized JSON Value; a non-SUCCESS `ret` is an error, exactly as
//! the Python parsers raise MtopApiError.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};

use crate::util::*;

// ---------------------------------------------------------------------------
// mtop.py envelope
// ---------------------------------------------------------------------------

fn opt_str_coerce(value: &Value) -> Option<String> {
    // _opt_str: scalars stringified, non-scalars become None.
    as_str(value)
}

pub fn parse_mtop_envelope(response: &Value) -> Result<Value> {
    let obj = response
        .as_object()
        .ok_or_else(|| anyhow!("MTOP response must be a JSON object"))?;
    let ret_raw = obj.get("ret").cloned().unwrap_or(Value::Null);
    let ret: Vec<String> = match &ret_raw {
        Value::Array(items) => items
            .iter()
            .map(|r| match r {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Value::Null => Vec::new(),
        other => vec![match other {
            Value::String(s) => s.clone(),
            o => o.to_string(),
        }],
    };
    let ret_code = ret
        .first()
        .map(|r| r.split("::").next().unwrap_or("").to_string());
    let is_success = ret_code
        .as_deref()
        .map(|c| c.to_uppercase() == "SUCCESS")
        .unwrap_or(false);
    let data = obj.get("data");
    Ok(json!({
        "api": opt_str_coerce(obj.get("api").unwrap_or(&Value::Null)),
        "version": opt_str_coerce(obj.get("v").unwrap_or(&Value::Null)),
        "ret": ret,
        "ret_code": ret_code,
        "is_success": is_success,
        "data": if matches!(data, Some(Value::Object(_))) { data.cloned().unwrap() } else { Value::Null },
    }))
}

/// The ApiError the way RemoteMtopCallback builds it (code/message),
/// simplified to the parts callers surface.
pub fn envelope_error(envelope: &Value) -> anyhow::Error {
    let ret = envelope
        .get("ret")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if ret.is_empty() {
        return anyhow!("UNKNOWN_ERROR");
    }
    let primary = if ret.len() >= 2 { &ret[1] } else { &ret[0] };
    let primary_s = primary.as_str().unwrap_or("");
    let (mut code, suffix) = match primary_s.split_once("::") {
        Some((c, s)) => (c.to_string(), s.to_string()),
        None => (primary_s.to_string(), String::new()),
    };
    let parts: Vec<&str> = if suffix.is_empty() {
        Vec::new()
    } else {
        suffix.split("||").collect()
    };
    let mut message = parts.first().map(|s| s.to_string()).unwrap_or_default();
    let mut desc_part = parts.get(1).map(|s| s.to_string()).unwrap_or_default();
    if code.is_empty() || message.is_empty() || desc_part.is_empty() {
        let first = ret[0].as_str().unwrap_or("");
        let (code0, suffix0) = match first.split_once("::") {
            Some((c, s)) => (c.to_string(), s.to_string()),
            None => (first.to_string(), String::new()),
        };
        let parts0: Vec<&str> = if suffix0.is_empty() {
            Vec::new()
        } else {
            suffix0.split("||").collect()
        };
        if code.is_empty() {
            code = code0.to_string();
        }
        if message.is_empty() {
            if let Some(m) = parts0.first() {
                message = m.to_string();
            }
        }
        if desc_part.is_empty() && parts0.len() > 1 {
            desc_part = parts0[1].to_string();
        }
    }
    if desc_part.is_empty() {
        for entry in &ret {
            let s = entry.as_str().unwrap_or("");
            if let Some((_, suf)) = s.split_once("::") {
                let p: Vec<&str> = suf.split("||").collect();
                if p.len() > 1 {
                    desc_part = p[1].to_string();
                    break;
                }
            }
        }
    }
    if message.is_empty() {
        message = "UNKNOWN_ERROR".to_string();
    }
    if code.is_empty() {
        code = "UNKNOWN_ERROR".to_string();
    }
    if desc_part == "IDLE_ITEM_DELETED" || desc_part == "ITEM_NOT_FOUND" {
        code = format!("{code}|{desc_part}");
    }
    anyhow!("{code}::{message}")
}

fn data_or_raise(response: &Value) -> Result<(Value, Value)> {
    let envelope = parse_mtop_envelope(response)?;
    if envelope.get("is_success").and_then(|v| v.as_bool()) != Some(true) {
        return Err(envelope_error(&envelope));
    }
    let data = envelope
        .get("data")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    Ok((envelope, data))
}

fn api_version(envelope: &Value) -> (Value, Value) {
    (
        envelope.get("api").cloned().unwrap_or(Value::Null),
        envelope.get("version").cloned().unwrap_or(Value::Null),
    )
}

// ---------------------------------------------------------------------------
// cards.py: DxCard -> SearchItem
// ---------------------------------------------------------------------------

pub fn is_ad_card(main: &Value) -> bool {
    let click = match main.get("clickParam") {
        Some(c) if c.is_object() => c,
        _ => return false,
    };
    let arg1 = click.get("arg1").and_then(|v| v.as_str());
    let biz_type = click
        .get("args")
        .and_then(|a| a.get("biz_type"))
        .and_then(|v| v.as_str());
    match (arg1, biz_type) {
        (Some(a), Some(b)) => {
            !a.is_empty() && a.to_lowercase() == "item" && b.to_lowercase() == "ad"
        }
        _ => false,
    }
}

fn parse_detail_param_images(detail_params: &Value) -> Vec<Value> {
    let raw = match detail_params.get("imageInfos") {
        Some(v) => v,
        None => return Vec::new(),
    };
    let parsed: Vec<Value> = match raw {
        Value::String(s) => match serde_json::from_str::<Value>(s) {
            Ok(Value::Array(a)) => a,
            _ => return Vec::new(),
        },
        Value::Array(a) => a.clone(),
        _ => return Vec::new(),
    };
    parsed
        .iter()
        .filter(|e| e.is_object())
        .map(|element| {
            let video_id = match element.get("videoId") {
                Some(Value::Bool(_)) | None => None,
                Some(Value::Number(n)) => Some(n.to_string()),
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            let is_video = element.get("videoCover").and_then(|v| v.as_bool()) == Some(true)
                || matches!(element.get("type"), Some(Value::Number(n)) if n.as_i64() == Some(10000))
                || matches!(element.get("type"), Some(Value::String(s)) if s == "10000")
                || video_id.is_some();
            build_obj(vec![
                ("url", opt_str(as_str(element.get("url").unwrap_or(&Value::Null)))),
                ("media_type", element.get("type").cloned()),
                ("width", element.get("widthSize").cloned()),
                ("height", element.get("heightSize").cloned()),
                ("major", element.get("major").cloned()),
                (
                    "is_video_cover",
                    element.get("videoCover").and_then(|v| v.as_bool()).map(Value::Bool),
                ),
                ("video_id", opt_str(video_id)),
                ("is_video", Some(Value::Bool(is_video))),
            ])
        })
        .collect()
}

/// Convert one DxCard to the normalized search-item shape.
pub fn parse_card(card: &Value, section: Option<&str>, index_in_section: Option<i64>) -> Value {
    let mut out = Map::new();
    out.insert("section".into(), json!(section));
    out.insert("index_in_section".into(), json!(index_in_section));
    if !card.is_object() {
        return Value::Object(out);
    }
    if let Some(v) = as_str(card.get("type").unwrap_or(&Value::Null)) {
        out.insert("card_type".into(), json!(v));
    }
    if let Some(v) = as_str(card.get("style").unwrap_or(&Value::Null)) {
        out.insert("card_style".into(), json!(v));
    }
    // SearchItem.is_ad defaults to false even for cards without the
    // item.main subtree (e.g. banner cards kept in the result list).
    out.insert("is_ad".into(), json!(false));
    let data = match card.get("data") {
        Some(d) if d.is_object() => d,
        _ => return Value::Object(out),
    };
    let template_key = match data.get("templateKey") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => "template".to_string(),
    };
    if let Some(template) = data.get(&template_key) {
        if template.is_object() {
            if let Some(v) = as_str(template.get("name").unwrap_or(&Value::Null)) {
                out.insert("template_name".into(), json!(v));
            }
            if let Some(v) = template.get("version") {
                out.insert("template_version".into(), v.clone());
            }
        }
    }
    let main = match dig(data, &["item", "main"]) {
        Some(m) if m.is_object() => m.clone(),
        _ => return Value::Object(out),
    };
    if let Some(v) = as_str(main.get("targetUrl").unwrap_or(&Value::Null)) {
        out.insert("target_url".into(), json!(v));
    }
    let click = main
        .get("clickParam")
        .filter(|c| c.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let args = click
        .get("args")
        .filter(|a| a.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let ex = main
        .get("exContent")
        .filter(|e| e.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let detail_params = ex.get("detailParams").filter(|d| d.is_object()).cloned();
    let dp = detail_params.clone().unwrap_or(json!({}));
    out.insert("is_ad".into(), json!(is_ad_card(&main)));
    if let Some(v) = as_str(click.get("arg1").unwrap_or(&Value::Null)) {
        out.insert("click_arg1".into(), json!(v));
    }
    if let Some(v) = as_str(args.get("biz_type").unwrap_or(&Value::Null)) {
        out.insert("biz_type".into(), json!(v));
    }
    if let Some(v) = as_str(args.get("index").unwrap_or(&Value::Null)) {
        out.insert("show_index".into(), json!(v));
    }
    if let Some(v) = as_str(args.get("cCatId").unwrap_or(&Value::Null)) {
        out.insert("category_id".into(), json!(v));
    }
    if let Some(v) = as_str(args.get("p_city").unwrap_or(&Value::Null)) {
        out.insert("p_city".into(), json!(v));
    }
    if let Some(v) = as_str(args.get("keyword").unwrap_or(&Value::Null)) {
        out.insert("keyword".into(), json!(v));
    }
    if let Some(dp) = &detail_params {
        out.insert("detail_params".into(), dp.clone());
        out.insert(
            "image_infos".into(),
            Value::Array(parse_detail_param_images(dp)),
        );
    }
    let item_id = as_str(dp.get("itemId").unwrap_or(&Value::Null))
        .or_else(|| as_str(ex.get("itemId").unwrap_or(&Value::Null)))
        .or_else(|| as_str(args.get("item_id").unwrap_or(&Value::Null)));
    if let Some(v) = item_id {
        out.insert("item_id".into(), json!(v));
    }
    let title = as_str(ex.get("title").unwrap_or(&Value::Null))
        .or_else(|| as_str(dp.get("title").unwrap_or(&Value::Null)));
    if let Some(v) = title {
        out.insert("title".into(), json!(v));
    }
    if let Some(v) = as_str(ex.get("richTitle").unwrap_or(&Value::Null)) {
        out.insert("rich_title".into(), json!(v));
    }
    if let Some(v) = ex.get("price") {
        if !v.is_null() {
            out.insert("price_text".into(), v.clone());
            // normalized `price`: the plain-string form when present
            if let Some(s) = as_str(v) {
                if v.is_string() {
                    out.insert("price".into(), json!(s));
                }
            }
        }
    }
    if let Some(v) = as_str(dp.get("soldPrice").unwrap_or(&Value::Null)) {
        out.insert("sold_price".into(), json!(v));
    }
    if let Some(v) = ex.get("oriPrice") {
        if !v.is_null() {
            out.insert("ori_price".into(), v.clone());
        }
    }
    if let Some(v) = ex.get("prefixPrice") {
        if !v.is_null() {
            out.insert("prefix_price".into(), v.clone());
        }
    }
    if let Some(v) = as_str(ex.get("picUrl").unwrap_or(&Value::Null)) {
        out.insert("pic_url".into(), json!(v));
    }
    if let Some(v) = ex.get("picWidth") {
        if !v.is_null() {
            out.insert("pic_width".into(), v.clone());
        }
    }
    if let Some(v) = ex.get("picHeight") {
        if !v.is_null() {
            out.insert("pic_height".into(), v.clone());
        }
    }
    if let Some(v) = as_str(ex.get("userAvatarUrl").unwrap_or(&Value::Null)) {
        out.insert("user_avatar_url".into(), json!(v));
    }
    if let Some(v) = as_str(ex.get("userNickName").unwrap_or(&Value::Null)) {
        out.insert("user_nick".into(), json!(v));
        out.insert("user_nick_name".into(), json!(v));
    }
    if let Some(v) = as_str(ex.get("want").unwrap_or(&Value::Null)) {
        out.insert("want".into(), json!(v));
        out.insert("want_text".into(), json!(v));
    }
    if let Some(v) = as_str(ex.get("area").unwrap_or(&Value::Null)) {
        out.insert("area".into(), json!(v));
    }
    if let Some(v) = ex.get("isAliMaMaAD") {
        if !v.is_null() {
            out.insert("is_alimama_ad_raw".into(), v.clone());
        }
    }
    Value::Object(out)
}

pub fn parse_card_list(cards: &Value, section: &str) -> Vec<Value> {
    as_list(cards)
        .iter()
        .enumerate()
        .map(|(i, card)| parse_card(card, Some(section), Some(i as i64)))
        .collect()
}

// ---------------------------------------------------------------------------
// search.py
// ---------------------------------------------------------------------------

pub fn parse_search_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let result_info = data.get("resultInfo").filter(|r| r.is_object()).cloned();
    let has_next = result_info
        .as_ref()
        .and_then(|ri| ri.get("hasNextPage"))
        .and_then(as_bool_pub);
    let hitnum = data.get("hitnum").and_then(as_int_strict_pub);
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("hitnum", opt_i64(hitnum)),
        ("has_next_page", opt_bool(has_next)),
        (
            "items",
            Some(Value::Array(parse_card_list(
                data.get("resultList").unwrap_or(&Value::Null),
                "resultList",
            ))),
        ),
        (
            "prefix_cards",
            Some(Value::Array(parse_card_list(
                data.get("resultPrefixBar").unwrap_or(&Value::Null),
                "resultPrefixBar",
            ))),
        ),
        (
            "relate_items",
            Some(Value::Array(parse_card_list(
                data.get("relateResultList").unwrap_or(&Value::Null),
                "relateResultList",
            ))),
        ),
        (
            "relate_prefix_cards",
            Some(Value::Array(parse_card_list(
                data.get("relateResultPrefixBar").unwrap_or(&Value::Null),
                "relateResultPrefixBar",
            ))),
        ),
        ("top_list", data.get("topList").cloned()),
        ("result_info", result_info),
        ("raw_data", Some(data)),
    ]))
}

fn as_bool_pub(v: &Value) -> Option<bool> {
    as_bool(v)
}
fn as_int_strict_pub(v: &Value) -> Option<i64> {
    as_int_strict(v)
}

// ---------------------------------------------------------------------------
// cards.py: feed item cards (DinamicX data.-root family)
// ---------------------------------------------------------------------------

pub const FEED_ITEM_TEMPLATES: [&str; 4] = [
    "fish_home_common_item_card_d3",
    "fish_home_tags_item_card_d5",
    "fish_home_advertise_card_d3",
    "fish_home_advertise_card_d4",
];
pub const FEED_AD_TEMPLATES: [&str; 2] =
    ["fish_home_advertise_card_d3", "fish_home_advertise_card_d4"];

fn rich_title_text(value: &Value) -> Option<String> {
    let arr = value.as_array()?;
    let mut parts = Vec::new();
    for element in arr {
        if !element.is_object() || element.get("type").and_then(|v| v.as_str()) != Some("Text") {
            continue;
        }
        if let Some(span_data) = element.get("data") {
            if let Some(text) = span_data.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    }
}

/// Build the FeedItemCard view for one feed section, or None when the
/// section's template is not a confirmed item-card template.
#[allow(clippy::if_same_then_else)]
pub fn parse_feed_item_card(index: i64, section: &Value) -> Option<Value> {
    if !section.is_object() {
        return None;
    }
    let template = section.get("template").filter(|t| t.is_object())?;
    let name = as_str(template.get("name")?)?;
    if !FEED_ITEM_TEMPLATES.contains(&name.as_str()) {
        return None;
    }
    let data = section
        .get("data")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let detail_params = data
        .get("detailParams")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let mut title = rich_title_text(data.get("richTitle").unwrap_or(&Value::Null));
    if title.is_none() {
        if let Some(summary) = data.get("titleSummary") {
            if summary.is_object() {
                title = as_str(summary.get("text").unwrap_or(&Value::Null));
            }
        }
    }
    if title.is_none() {
        title = as_str(detail_params.get("title").unwrap_or(&Value::Null));
    }
    let click = data
        .get("clickParam")
        .filter(|c| c.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let click_args = click
        .get("args")
        .filter(|a| a.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let item_id = as_str(data.get("itemId").unwrap_or(&Value::Null))
        .or_else(|| as_str(detail_params.get("itemId").unwrap_or(&Value::Null)))
        .or_else(|| as_str(click_args.get("itemId").unwrap_or(&Value::Null)))
        .or_else(|| as_str(click_args.get("item_id").unwrap_or(&Value::Null)));
    let price_info = data
        .get("priceInfo")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let main_pic = data
        .get("mainPicInfo")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let top_hot = data
        .get("topHotPoint")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let hot_point = data
        .get("hotPoint")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let user = data
        .get("user")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let video_info = data
        .get("videoInfo")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let mut video_id = as_str(video_info.get("videoId").unwrap_or(&Value::Null));
    let main_pic_video_id = as_str(main_pic.get("videoId").unwrap_or(&Value::Null));
    if let (Some(a), Some(b)) = (&video_id, &main_pic_video_id) {
        if a != b {
            video_id = None;
        }
    }
    let is_video_flags = [
        detail_params.get("isVideo").cloned(),
        data.get("isVideo").cloned(),
    ];
    let has_video = if video_info.is_object()
        && !video_info.as_object().map(|m| m.is_empty()).unwrap_or(true)
    {
        Some(true)
    } else if is_video_flags
        .iter()
        .flatten()
        .any(|f| matches!(f, Value::String(s) if s.to_lowercase() == "true"))
    {
        Some(true)
    } else if is_video_flags
        .iter()
        .flatten()
        .any(|f| matches!(f, Value::String(s) if s.to_lowercase() == "false"))
    {
        Some(false)
    } else {
        None
    };
    let video_url_present =
        matches!(video_info.get("videoUrl"), Some(Value::String(s)) if !s.is_empty());
    Some(build_obj(vec![
        ("section_index", Some(json!(index))),
        ("template_name", Some(json!(name))),
        ("template_version", template.get("version").cloned()),
        (
            "column_type",
            opt_str(as_str(template.get("columnType").unwrap_or(&Value::Null))),
        ),
        (
            "is_ad",
            Some(json!(FEED_AD_TEMPLATES.contains(&name.as_str()))),
        ),
        ("item_id", opt_str(item_id)),
        ("title", opt_str(title)),
        (
            "price_text",
            opt_str(as_str(price_info.get("price").unwrap_or(&Value::Null))),
        ),
        (
            "price",
            opt_str(as_str(price_info.get("price").unwrap_or(&Value::Null))),
        ),
        (
            "pic_url",
            opt_str(as_str(main_pic.get("url").unwrap_or(&Value::Null))),
        ),
        ("pic_width", main_pic.get("widthSize").cloned()),
        ("pic_height", main_pic.get("heightSize").cloned()),
        (
            "area_text",
            opt_str(as_str(top_hot.get("areaText").unwrap_or(&Value::Null))),
        ),
        (
            "area",
            opt_str(as_str(top_hot.get("areaText").unwrap_or(&Value::Null))),
        ),
        (
            "distance_text",
            opt_str(as_str(top_hot.get("distanceText").unwrap_or(&Value::Null))),
        ),
        (
            "hot_point_text",
            opt_str(as_str(hot_point.get("text").unwrap_or(&Value::Null))),
        ),
        (
            "user_nick",
            opt_str(as_str(user.get("userNick").unwrap_or(&Value::Null))),
        ),
        (
            "user_avatar_url",
            opt_str(as_str(user.get("avatar").unwrap_or(&Value::Null))),
        ),
        (
            "target_url",
            opt_str(as_str(data.get("targetUrl").unwrap_or(&Value::Null))),
        ),
        ("video_id", opt_str(video_id)),
        ("has_video", opt_bool(has_video)),
        ("video_url_present", Some(json!(video_url_present))),
        (
            "video_cover_url",
            opt_str(as_str(video_info.get("url").unwrap_or(&Value::Null))),
        ),
        ("video_cover_width", video_info.get("widthSize").cloned()),
        ("video_cover_height", video_info.get("heightSize").cloned()),
        (
            "detail_params",
            if data
                .get("detailParams")
                .map(|d| d.is_object())
                .unwrap_or(false)
            {
                data.get("detailParams").cloned()
            } else {
                None
            },
        ),
    ]))
}

// ---------------------------------------------------------------------------
// item.py: item detail (FCI)
// ---------------------------------------------------------------------------

fn s_or_empty(value: Option<&Value>) -> String {
    value.and_then(as_str).unwrap_or_default()
}

fn non_empty(value: Option<&Value>) -> Option<String> {
    as_str(value?).filter(|t| !t.is_empty())
}

fn parse_json_string(value: Option<&Value>) -> Option<Value> {
    let s = value?.as_str()?;
    if s.is_empty() {
        return None;
    }
    let parsed: Value = serde_json::from_str(s).ok()?;
    if parsed.is_object() {
        Some(parsed)
    } else {
        None
    }
}

fn components_of(section: &Value) -> Vec<Value> {
    section
        .get("components")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default()
}

fn iter_component_data(sections: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    for section in sections {
        for component in components_of(section) {
            if let Some(cdata) = component.get("data") {
                if cdata.is_object() {
                    out.push(cdata.clone());
                }
            }
        }
    }
    out
}

fn extract_images(sections: &[Value]) -> Vec<Value> {
    let mut images = Vec::new();
    for section in sections {
        for component in components_of(section) {
            if component.get("render").and_then(|v| v.as_str())
                != Some("Basic_Native_spn_image_group")
            {
                continue;
            }
            let data = match component.get("data") {
                Some(d) if d.is_object() => d,
                _ => continue,
            };
            let media = match data.get("mediaInfos").and_then(|m| m.as_array()) {
                Some(m) => m,
                None => continue,
            };
            for entry in media {
                if !entry.is_object() {
                    continue;
                }
                let media_type = entry.get("type").and_then(as_int_measured);
                let video_id = as_str(entry.get("videoId").unwrap_or(&Value::Null));
                let is_video = video_id.is_some() || !matches!(media_type, None | Some(0));
                images.push(build_obj(vec![
                    (
                        "url",
                        opt_str(as_str(entry.get("url").unwrap_or(&Value::Null))),
                    ),
                    (
                        "cover_url",
                        opt_str(as_str(entry.get("coverUrl").unwrap_or(&Value::Null))),
                    ),
                    ("media_type", opt_i64(media_type)),
                    ("video_id", opt_str(video_id)),
                    ("width", entry.get("width").cloned()),
                    ("height", entry.get("height").cloned()),
                    ("is_video", Some(json!(is_video))),
                ]));
            }
        }
    }
    images
}

pub fn parse_item_response(detail_json: &Value) -> Result<Value> {
    if !detail_json.is_object() {
        return Err(anyhow!("detail payload must be a JSON object"));
    }
    let mut data = detail_json.clone();
    let has_flow = data.get("flowData").is_some();
    if !has_flow {
        if let Some(inner) = data.get("data") {
            if inner.is_object() && inner.get("flowData").is_some() {
                let envelope = parse_mtop_envelope(&data)?;
                if envelope.get("is_success").and_then(|v| v.as_bool()) != Some(true) {
                    return Err(envelope_error(&envelope));
                }
                data = inner.clone();
            } else if data.get("ret").is_some() {
                let envelope = parse_mtop_envelope(&data)?;
                if envelope.get("is_success").and_then(|v| v.as_bool()) != Some(true) {
                    return Err(envelope_error(&envelope));
                }
            }
        } else if data.get("ret").is_some() {
            let envelope = parse_mtop_envelope(&data)?;
            if envelope.get("is_success").and_then(|v| v.as_bool()) != Some(true) {
                return Err(envelope_error(&envelope));
            }
        }
    }

    // FCI defaults
    let mut item_id = String::new();
    let mut title = String::new();
    let mut desc = String::new();
    let mut price = String::new();
    let mut cpv_text = String::new();
    let mut cpv_attributes: Vec<Value> = Vec::new();
    let mut is_free_shipping = false;
    let mut fav_count: i64 = 0;
    let mut want_count: i64 = 0;
    let mut view_count: i64 = 0;
    let mut channel_cat_id = String::new();
    let mut seller_nick = String::new();
    let mut seller_id = String::new();
    let mut seller_city = String::new();
    let mut seller_identity_tag = String::new();
    let mut seller_desc = String::new();
    let mut sold_count: i64 = 0;

    let flow = data.get("flowData").filter(|f| f.is_object()).cloned();
    let flow = match flow {
        Some(f) => f,
        None => {
            return Ok(detail_output(
                &data,
                item_id,
                title,
                desc,
                price,
                cpv_text,
                cpv_attributes,
                is_free_shipping,
                fav_count,
                want_count,
                view_count,
                channel_cat_id,
                seller_nick,
                seller_id,
                seller_city,
                seller_identity_tag,
                seller_desc,
                sold_count,
                Vec::new(),
                Vec::new(),
                None,
                None,
                None,
            ))
        }
    };
    let sections: Vec<Value> = flow
        .get("body")
        .and_then(|b| b.get("sections"))
        .and_then(|s| s.as_array())
        .cloned()
        .unwrap_or_default();
    if sections.is_empty() {
        return Ok(detail_output(
            &data,
            item_id,
            title,
            desc,
            price,
            cpv_text,
            cpv_attributes,
            is_free_shipping,
            fav_count,
            want_count,
            view_count,
            channel_cat_id,
            seller_nick,
            seller_id,
            seller_city,
            seller_identity_tag,
            seller_desc,
            sold_count,
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
        ));
    }
    let images = extract_images(&sections);
    let components: Vec<Value> = {
        let mut summaries = Vec::new();
        for (idx, section) in sections.iter().enumerate() {
            for component in components_of(section) {
                if component.is_object() {
                    summaries.push(json!({
                        "render": as_str(component.get("render").unwrap_or(&Value::Null)),
                        "key": as_str(component.get("key").unwrap_or(&Value::Null)),
                        "section_index": idx,
                    }));
                }
            }
        }
        summaries
    };
    let price_info = extract_price_info(&sections);
    let description = extract_description(&sections);
    let seller_card = extract_seller_card(&sections);

    let components0 = components_of(&sections[0]);
    let track_params = flow
        .get("trackParams")
        .filter(|t| t.is_object())
        .cloned()
        .unwrap_or(json!({}));

    for component in &components0 {
        if !component.is_object() {
            continue;
        }
        let cdata = match component.get("data") {
            Some(d) if d.is_object() => d.clone(),
            _ => continue,
        };
        if item_id.is_empty() && cdata.get("itemId").is_some() {
            item_id = s_or_empty(cdata.get("itemId"));
        }
        if cdata.get("desc").is_some() {
            desc = s_or_empty(cdata.get("desc"));
        }
        let req_params = cdata
            .get("reqParams")
            .filter(|r| r.is_object())
            .cloned()
            .unwrap_or(json!({}));
        if let Some(t) = non_empty(req_params.get("title")) {
            title = t;
        }
        if title.is_empty() {
            title = desc.clone();
        }
        if cdata.get("price").is_some() {
            price = s_or_empty(cdata.get("price"));
        }
        if let Some(cpv_labels) = cdata.get("cpvLabels").and_then(|v| v.as_array()) {
            let mut parts = Vec::new();
            let mut attrs = Vec::new();
            for entry in cpv_labels {
                if !entry.is_object() {
                    continue;
                }
                let name = non_empty(entry.get("propertyName"));
                let value = non_empty(entry.get("valueName"));
                if let (Some(n), Some(v)) = (name, value) {
                    parts.push(format!("{n}:{v}"));
                    attrs.push(json!({"name": n, "value": v}));
                }
            }
            cpv_text = parts.join(",");
            cpv_attributes = attrs;
        }
        if let Some(reduce_list) = cdata.get("reduceInfoList").and_then(|v| v.as_array()) {
            for entry in reduce_list {
                if entry.is_object() {
                    let text = s_or_empty(entry.get("text"));
                    if text.contains('包') && text.contains('邮') {
                        // contains "包邮"
                        if text.contains("包邮") {
                            is_free_shipping = true;
                            break;
                        }
                    }
                }
            }
        }
        if let Some(cnt) = cdata.get("collectCnt") {
            let cnt_s = as_str(cnt);
            if let Some(cnt_s) = cnt_s {
                if !cnt_s.is_empty() {
                    if let Ok(n) = cnt_s.parse::<i64>() {
                        fav_count = n;
                    }
                }
            }
        }
        if let Some(extra_list) = cdata.get("extraInfoList").and_then(|v| v.as_array()) {
            for entry in extra_list {
                let text = match as_str(entry) {
                    Some(t) => t,
                    None => continue,
                };
                if text.is_empty() {
                    continue;
                }
                if text.contains("想要") {
                    want_count = extract_first_int(&text);
                }
                if text.contains("浏览") {
                    view_count = extract_first_int(&text);
                }
            }
        }
        if track_params.get("channelCatId").is_some() {
            if let Some(cat) = non_empty(track_params.get("channelCatId")) {
                channel_cat_id = cat;
            }
        }
        if channel_cat_id.is_empty() {
            if let Some(cat) = non_empty(req_params.get("mainItemLeafCateId")) {
                channel_cat_id = cat;
            }
        }
        if let Some(user_info) = cdata.get("userInfo") {
            if user_info.is_object() {
                if user_info.get("nick").is_some() {
                    if let Some(nick) = non_empty(user_info.get("nick")) {
                        seller_nick = nick;
                    }
                }
                if user_info.get("userId").is_some() {
                    if let Some(uid) = non_empty(user_info.get("userId")) {
                        seller_id = uid;
                    }
                }
            }
        }
        if cdata.get("city").is_some() {
            if let Some(city) = non_empty(cdata.get("city")) {
                seller_city = city;
            }
        }
        if let Some(tags) = cdata.get("identityTitleTags").and_then(|v| v.as_array()) {
            if let Some(first) = tags.first() {
                if first.is_object() && first.get("text").is_some() {
                    if let Some(tag) = non_empty(first.get("text")) {
                        seller_identity_tag = tag;
                    }
                }
            }
        }
        if cdata.get("creditInfoDesc").is_some() {
            if let Some(credit) = non_empty(cdata.get("creditInfoDesc")) {
                sold_count = extract_first_int(&credit);
                seller_desc = credit;
            }
        }
    }

    // flowData.context.userData fallbacks
    if let Some(context) = flow.get("context") {
        if context.is_object() && context.get("userData").is_some() {
            if let Some(user_data) = parse_json_string(context.get("userData")) {
                if seller_nick.is_empty() && user_data.get("displayName").is_some() {
                    if let Some(nick) = non_empty(user_data.get("displayName")) {
                        seller_nick = nick;
                    }
                }
                if user_data.get("introduction").is_some() {
                    if let Some(intro) = non_empty(user_data.get("introduction")) {
                        sold_count = extract_first_int(&intro);
                        seller_desc = intro;
                    }
                }
                if seller_identity_tag.is_empty() && user_data.get("tagInfo").is_some() {
                    if let Some(tag_info) = parse_json_string(user_data.get("tagInfo")) {
                        if tag_info.get("tagText").is_some() {
                            if let Some(tag) = non_empty(tag_info.get("tagText")) {
                                seller_identity_tag = tag;
                            }
                        }
                    }
                }
            }
        }
    }

    if seller_city.is_empty() {
        for component in &components0 {
            if !component.is_object() {
                continue;
            }
            let cdata = match component.get("data") {
                Some(d) if d.is_object() => d,
                _ => continue,
            };
            if let Some(aoi) = cdata.get("aoiInfo").and_then(|v| v.as_array()) {
                if aoi.len() > 1 {
                    if let Some(city) = non_empty(Some(&aoi[1])) {
                        seller_city = city;
                        break;
                    }
                }
            }
        }
    }

    Ok(detail_output(
        &data,
        item_id,
        title,
        desc,
        price,
        cpv_text,
        cpv_attributes,
        is_free_shipping,
        fav_count,
        want_count,
        view_count,
        channel_cat_id,
        seller_nick,
        seller_id,
        seller_city,
        seller_identity_tag,
        seller_desc,
        sold_count,
        images,
        components,
        price_info,
        description,
        seller_card,
    ))
}

#[allow(clippy::too_many_arguments)]
fn detail_output(
    raw_data: &Value,
    item_id: String,
    title: String,
    desc: String,
    price: String,
    cpv_text: String,
    cpv_attributes: Vec<Value>,
    is_free_shipping: bool,
    fav_count: i64,
    want_count: i64,
    view_count: i64,
    channel_cat_id: String,
    seller_nick: String,
    seller_id: String,
    seller_city: String,
    seller_identity_tag: String,
    seller_desc: String,
    sold_count: i64,
    images: Vec<Value>,
    components: Vec<Value>,
    price_info: Option<Value>,
    description: Option<Value>,
    seller_card: Option<Value>,
) -> Value {
    let desc_text = description
        .as_ref()
        .and_then(|d| d.get("desc"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    json!({
        "item_id": item_id,
        "title": title,
        "desc": if !desc.is_empty() { desc } else { desc_text },
        "price": price.clone(),
        "cpv_text": cpv_text,
        "cpv_attributes": cpv_attributes,
        "is_free_shipping": is_free_shipping,
        "fav_count": fav_count,
        "want_count": want_count,
        "view_count": view_count,
        "channel_cat_id": channel_cat_id,
        "seller": {
            "nick": seller_nick,
            "seller_id": seller_id,
            "city": seller_city,
            "identity_tag": seller_identity_tag,
            "desc": seller_desc,
            "sold_count": sold_count,
        },
        "images": images,
        "components": components,
        "price_info": price_info,
        "description": description,
        "seller_card": seller_card,
        "raw_data": raw_data,
    })
}

fn extract_price_info(sections: &[Value]) -> Option<Value> {
    const PRICE_KEYS: [&str; 7] = [
        "price",
        "minPrice",
        "maxPrice",
        "unit",
        "priceSuffix",
        "extraInfoList",
        "reduceInfoList",
    ];
    for cdata in iter_component_data(sections) {
        let obj = cdata.as_object()?;
        if !PRICE_KEYS.iter().any(|k| obj.contains_key(*k)) {
            continue;
        }
        return Some(json!({
            "price": s_or_empty(cdata.get("price")),
            "min_price": s_or_empty(cdata.get("minPrice")),
            "max_price": s_or_empty(cdata.get("maxPrice")),
            "unit": s_or_empty(cdata.get("unit")),
            "price_suffix": s_or_empty(cdata.get("priceSuffix")),
            "extra_info_list": cdata.get("extraInfoList").filter(|v| v.is_array()).cloned(),
            "reduce_info_list": cdata.get("reduceInfoList").filter(|v| v.is_array()).cloned(),
            "custom_text": s_or_empty(cdata.get("customText")),
            "price_end_text": s_or_empty(cdata.get("priceEndText")),
            "quality_url": s_or_empty(cdata.get("qualityUrl")),
            "group_buy_info": cdata.get("groupBuyInfo").filter(|v| v.is_object()).cloned(),
            "idle_promotion_info": cdata.get("idlePromotionInfo").filter(|v| v.is_object()).cloned(),
            "price_with_note": cdata.get("priceWithNote").filter(|v| v.is_object()).cloned(),
            "promotion_query_params": cdata.get("promotionQueryParams").filter(|v| v.is_object()).cloned(),
        }));
    }
    None
}

fn extract_description(sections: &[Value]) -> Option<Value> {
    for cdata in iter_component_data(sections) {
        let obj = cdata.as_object()?;
        if !["desc", "richTextDesc", "aiDesc"]
            .iter()
            .any(|k| obj.contains_key(*k))
        {
            continue;
        }
        return Some(json!({
            "desc": s_or_empty(cdata.get("desc")),
            "ai_desc": s_or_empty(cdata.get("aiDesc")),
            "rich_text_desc": s_or_empty(cdata.get("richTextDesc")),
            "seller_id": s_or_empty(cdata.get("sellerId")),
            "highlights": cdata.get("highlights").filter(|v| v.is_array()).cloned(),
        }));
    }
    None
}

fn extract_seller_card(sections: &[Value]) -> Option<Value> {
    for cdata in iter_component_data(sections) {
        let obj = cdata.as_object()?;
        if !obj.contains_key("nick") {
            continue;
        }
        if ![
            "creditInfoDesc",
            "sellerId",
            "identityTitleTags",
            "avatarUrl",
        ]
        .iter()
        .any(|k| obj.contains_key(*k))
        {
            continue;
        }
        return Some(json!({
            "nick": s_or_empty(cdata.get("nick")),
            "seller_id": s_or_empty(cdata.get("sellerId")),
            "city": s_or_empty(cdata.get("city")),
            "credit_info_desc": s_or_empty(cdata.get("creditInfoDesc")),
            "avatar_url": s_or_empty(cdata.get("avatarUrl")),
            "last_coming_str": s_or_empty(cdata.get("lastComingStr")),
            "identity_title_tags": cdata.get("identityTitleTags").filter(|v| v.is_array()).cloned(),
        }));
    }
    None
}

// ---------------------------------------------------------------------------
// browse.py parsers
// ---------------------------------------------------------------------------

pub fn parse_category_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let categories: Vec<Value> = as_list(data.get("cat2List").unwrap_or(&Value::Null))
        .iter()
        .filter(|e| e.is_object())
        .map(|entry| {
            build_obj(vec![
                (
                    "cat_id",
                    opt_str(as_str(entry.get("catId").unwrap_or(&Value::Null))),
                ),
                (
                    "name",
                    opt_str(as_str(entry.get("name").unwrap_or(&Value::Null))),
                ),
                (
                    "track_params",
                    entry.get("trackParams").filter(|t| t.is_object()).cloned(),
                ),
            ])
        })
        .collect();
    let cards: Vec<Value> = as_list(data.get("cardList").unwrap_or(&Value::Null))
        .iter()
        .filter(|c| c.is_object())
        .map(|card| {
            build_obj(vec![(
                "card_type",
                opt_str(as_str(card.get("cardType").unwrap_or(&Value::Null))),
            )])
        })
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("categories", Some(Value::Array(categories))),
        ("cards", Some(Value::Array(cards))),
        (
            "card_data_map",
            data.get("cardDataMap").filter(|m| m.is_object()).cloned(),
        ),
    ]))
}

fn parse_recommend_card(card: &Value) -> Value {
    let item = card.get("cardData").filter(|d| d.is_object()).map(|card_data| {
        let wrapper = json!({"type": card.get("cardType").cloned().unwrap_or(Value::Null), "data": card_data});
        parse_card(&wrapper, Some("recommendList"), None)
    });
    build_obj(vec![
        (
            "card_type",
            opt_str(as_str(card.get("cardType").unwrap_or(&Value::Null))),
        ),
        ("item", item),
        (
            "idle_ads_task_id",
            opt_str(as_str(card.get("idleAdsTaskId").unwrap_or(&Value::Null))),
        ),
        (
            "mocked",
            opt_str(as_str(card.get("mocked").unwrap_or(&Value::Null))),
        ),
        (
            "more_topic_id",
            opt_str(as_str(card.get("moreTopicId").unwrap_or(&Value::Null))),
        ),
        (
            "render_times",
            opt_i64(card.get("renderTimes").and_then(as_int_measured)),
        ),
    ])
}

pub fn parse_recommend_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let cards: Vec<Value> = as_list(data.get("cardList").unwrap_or(&Value::Null))
        .iter()
        .filter(|c| c.is_object())
        .map(parse_recommend_card)
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("cards", Some(Value::Array(cards))),
        ("has_more", opt_bool(data.get("hasMore").and_then(as_bool))),
        (
            "server_time",
            opt_str(as_str(data.get("serverTime").unwrap_or(&Value::Null))),
        ),
        (
            "need_decrypt_keys",
            data.get("needDecryptKeys")
                .filter(|k| k.is_array())
                .cloned(),
        ),
        (
            "wind_card",
            data.get("windCard").filter(|w| w.is_object()).cloned(),
        ),
    ]))
}

/// PublicComment with nested replies, built bottom-up over an arena
/// (the Python parser is iterative for the same reason: reply nesting
/// comes from the server payload and must not recurse unboundedly).
fn parse_public_comment(root: &Value) -> Value {
    // Arena collection (pre-order).
    let mut nodes: Vec<Value> = Vec::new();
    let mut children: Vec<Vec<usize>> = Vec::new();
    // Stable node identity: record parent->child indices directly.
    nodes.push(root.clone());
    children.push(Vec::new());
    let mut work: Vec<usize> = vec![0];
    while let Some(idx) = work.pop() {
        let node = nodes[idx].clone();
        if let Some(replies) = node.get("reply").and_then(|r| r.as_array()) {
            for reply in replies {
                if reply.is_object() {
                    let child_idx = nodes.len();
                    nodes.push(reply.clone());
                    children.push(Vec::new());
                    children[idx].push(child_idx);
                    work.push(child_idx);
                }
            }
        }
    }
    let mut built: Vec<Option<Value>> = vec![None; nodes.len()];
    for idx in (0..nodes.len()).rev() {
        let replies: Vec<Value> = children[idx]
            .iter()
            .map(|c| built[*c].clone().unwrap_or(json!({})))
            .collect();
        built[idx] = Some(build_public_comment(&nodes[idx], replies));
    }
    built[0].clone().unwrap_or(json!({}))
}

fn build_public_comment(obj: &Value, replies: Vec<Value>) -> Value {
    let g = |k: &str| opt_str(as_str(obj.get(k).unwrap_or(&Value::Null)));
    build_obj(vec![
        ("comment_id", g("commentId")),
        ("content", g("content")),
        ("reporter_id", g("reporterId")),
        ("reporter_nick", g("reporterNick")),
        ("portrait_url", g("portraitUrl")),
        ("gmt_create", g("gmtCreate")),
        ("report_time", g("reportTime")),
        ("ip_region_address", g("ipRegionAddress")),
        ("level", g("level")),
        ("parent_comment_id", g("parentCommentId")),
        ("parent_commenter_id", g("parentCommenterId")),
        ("parent_commenter_nick", g("parentCommenterNick")),
        ("be_replier_id", g("beReplierId")),
        ("be_replier_nick", g("beReplierNick")),
        ("reply_comment_id", g("replyCommentId")),
        (
            "pict_urls",
            obj.get("pictCdnUrlList").filter(|p| p.is_array()).cloned(),
        ),
        ("seller_id", g("sellerId")),
        ("seller_nick", g("sellerNick")),
        ("rate_id", g("rateId")),
        ("feedback", g("feedback")),
        ("rater_user_nick", g("raterUserNick")),
        ("rater_head_img", g("raterHeadImg")),
        ("rated_uid", g("ratedUid")),
        ("biz_type", g("bizType")),
        ("status", g("status")),
        ("ip_address", g("ipAddress")),
        ("item_id", g("itemId")),
        ("biz_code", g("bizCode")),
        ("rate_status", g("rateStatus")),
        (
            "trade_info",
            obj.get("tradeInfo").filter(|t| t.is_object()).cloned(),
        ),
        ("replies", Some(Value::Array(replies))),
    ])
}

pub fn parse_comment_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let comments: Vec<Value> = as_list(data.get("items").unwrap_or(&Value::Null))
        .iter()
        .filter(|o| o.is_object())
        .map(parse_public_comment)
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("comments", Some(Value::Array(comments))),
        (
            "total_count",
            opt_str(as_str(data.get("totalCount").unwrap_or(&Value::Null))),
        ),
        (
            "total_rate_count",
            opt_str(as_str(data.get("totalRateCount").unwrap_or(&Value::Null))),
        ),
        (
            "next_page",
            opt_bool(data.get("nextPage").and_then(as_bool)),
        ),
        (
            "list_type",
            opt_str(as_str(data.get("type").unwrap_or(&Value::Null))),
        ),
        (
            "rate_labels",
            data.get("rateLabels").filter(|l| l.is_array()).cloned(),
        ),
    ]))
}

pub fn parse_rate_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let cards: Vec<Value> = as_list(data.get("cardList").unwrap_or(&Value::Null))
        .iter()
        .filter(|c| c.is_object())
        .map(|card| {
            let comment = card
                .get("cardData")
                .filter(|d| d.is_object())
                .map(parse_public_comment);
            build_obj(vec![
                (
                    "card_type",
                    opt_str(as_str(card.get("cardType").unwrap_or(&Value::Null))),
                ),
                ("comment", comment),
            ])
        })
        .collect();
    // displayable: cardType "1040" with a comment carrying non-empty feedback
    let displayable = cards
        .iter()
        .filter(|c| {
            c.get("card_type").and_then(|v| v.as_str()) == Some("1040")
                && c.get("comment")
                    .and_then(|cm| cm.get("feedback"))
                    .and_then(|f| f.as_str())
                    .map(|f| !f.is_empty())
                    .unwrap_or(false)
        })
        .count();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("cards", Some(Value::Array(cards))),
        ("displayable_count", Some(json!(displayable))),
        (
            "next_page",
            opt_bool(data.get("nextPage").and_then(as_bool)),
        ),
        (
            "total_rate_count",
            opt_str(as_str(data.get("totalRateCount").unwrap_or(&Value::Null))),
        ),
        (
            "total_count",
            opt_str(as_str(data.get("totalCount").unwrap_or(&Value::Null))),
        ),
    ]))
}

pub fn parse_seller_items_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let cards: Vec<Value> = as_list(data.get("cardList").unwrap_or(&Value::Null))
        .iter()
        .filter(|c| c.is_object())
        .map(|card| {
            build_obj(vec![(
                "card_type",
                opt_str(as_str(card.get("cardType").unwrap_or(&Value::Null))),
            )])
        })
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("cards", Some(Value::Array(cards))),
        (
            "next_page",
            opt_bool(data.get("nextPage").and_then(as_bool)),
        ),
        (
            "total_count",
            opt_i64(data.get("totalCount").and_then(as_int_measured)),
        ),
    ]))
}

pub fn parse_home_feed_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let raw_sections = as_list(data.get("sections").unwrap_or(&Value::Null));
    let sections: Vec<Value> = raw_sections
        .iter()
        .filter(|s| s.is_object())
        .map(|section| {
            build_obj(vec![(
                "section_type",
                opt_str(as_str(section.get("sectionType").unwrap_or(&Value::Null))),
            )])
        })
        .collect();
    let item_cards: Vec<Value> = raw_sections
        .iter()
        .enumerate()
        .filter_map(|(i, s)| parse_feed_item_card(i as i64, s))
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("sections", Some(Value::Array(sections))),
        (
            "feeds_count",
            opt_i64(data.get("feedsCount").and_then(as_int_measured)),
        ),
        (
            "next_page",
            opt_bool(data.get("nextPage").and_then(as_bool)),
        ),
        (
            "server_time",
            opt_str(as_str(data.get("serverTime").unwrap_or(&Value::Null))),
        ),
        (
            "client_recycle_params",
            opt_str(as_str(
                data.get("clientRecycleParams").unwrap_or(&Value::Null),
            )),
        ),
        (
            "home_top_list",
            data.get("homeTopList").filter(|t| t.is_array()).cloned(),
        ),
        (
            "config_info",
            data.get("configInfo").filter(|c| c.is_object()).cloned(),
        ),
        ("item_cards", Some(Value::Array(item_cards))),
    ]))
}

fn suggest_entry(kind: &str, obj: &Value) -> Value {
    let g = |k: &str| opt_str(as_str(obj.get(k).unwrap_or(&Value::Null)));
    build_obj(vec![
        ("kind", Some(json!(kind))),
        ("suggest", g("suggest")),
        ("rn", g("rn")),
        ("desc", g("desc")),
        ("user_id", g("userId")),
        ("user_nick", g("userNick")),
        ("avatar", g("avatar")),
        ("title", g("title")),
        ("price", g("price")),
        ("redirect", g("redirect")),
        ("ai_seek_suggest", g("aiSeekSuggest")),
    ])
}

pub fn parse_suggest_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let mut dynamic_entries: Vec<Value> = Vec::new();
    for entry in as_list(data.get("dynamicList").unwrap_or(&Value::Null)) {
        if !entry.is_object() {
            continue;
        }
        let kind = as_str(entry.get("type").unwrap_or(&Value::Null))
            .unwrap_or_else(|| "unknown".to_string());
        match entry.get("data") {
            Some(payload) if payload.is_object() => {
                dynamic_entries.push(suggest_entry(&kind, payload));
            }
            _ => {
                dynamic_entries.push(build_obj(vec![("kind", Some(json!(kind)))]));
            }
        }
    }
    let static_list = |key: &str, kind: &str| -> Vec<Value> {
        as_list(data.get(key).unwrap_or(&Value::Null))
            .iter()
            .filter(|o| o.is_object())
            .map(|o| suggest_entry(kind, o))
            .collect()
    };
    let pic_text = static_list("picTextEntryInfoVOList", "picText");
    let users = static_list("users", "user");
    let items = static_list("items", "item");
    let entries = if !dynamic_entries.is_empty() {
        dynamic_entries.clone()
    } else {
        let mut all = pic_text.clone();
        all.extend(users.clone());
        all.extend(items.clone());
        all
    };
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("entries", Some(Value::Array(entries))),
        ("dynamic_entries", Some(Value::Array(dynamic_entries))),
        ("pic_text_entries", Some(Value::Array(pic_text))),
        ("user_entries", Some(Value::Array(users))),
        ("item_entries", Some(Value::Array(items))),
        (
            "bucket_id",
            opt_i64(data.get("bucketId").and_then(as_int_measured)),
        ),
        (
            "show_static_user_search",
            opt_bool(data.get("showStaticUserSearch").and_then(as_bool)),
        ),
        (
            "enable_new_style_updated",
            opt_bool(data.get("enableNewStyleUpdated").and_then(as_bool)),
        ),
    ]))
}

pub fn parse_user_headinfo_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let g = |k: &str| opt_str(as_str(data.get(k).unwrap_or(&Value::Null)));
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("user_id", g("userId")),
        ("id", g("id")),
        ("nick", g("nick")),
        ("idle_nick", g("idleNick")),
        ("user_nick", g("userNick")),
        ("avatar", g("avatar")),
        ("background_image_url", g("backgroundImageUrl")),
        ("background_image", g("backGroundImage")),
        ("city", g("city")),
        ("address", g("address")),
        ("signature", g("signature")),
        ("summary", g("summary")),
        ("gender", g("gender")),
        ("constellation", g("constellation")),
        (
            "info_percent",
            opt_i64(data.get("infoPercent").and_then(as_int_strict)),
        ),
        ("idle_selling_num", g("idleSellingNum")),
        ("idle_sold_num", g("idleSoldNum")),
        ("idle_buy_num", g("idleBuyNum")),
        ("idle_fav_num", g("idleFavNum")),
        ("attention_num", g("myAttentionNum")),
        ("fans_num", g("myFansNum")),
        ("favor_num", g("myFavorNum")),
        ("super_favor_num", g("superFavorNum")),
        ("trade_count", g("tradeCount")),
        ("trade_income_content", g("tradeIncomeContent")),
        ("share_url", g("shareUrl")),
    ]))
}

pub fn parse_video_play_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        (
            "play_url",
            opt_str(as_str(data.get("playUrl").unwrap_or(&Value::Null))),
        ),
        (
            "ld320p_url",
            opt_str(as_str(data.get("ld320pUrl").unwrap_or(&Value::Null))),
        ),
        (
            "sd480p_url",
            opt_str(as_str(data.get("sd480pUrl").unwrap_or(&Value::Null))),
        ),
        (
            "mts_videoid",
            opt_i64(data.get("mtsVideoid").and_then(as_int_strict)),
        ),
    ]))
}

// ---------------------------------------------------------------------------
// explore.py parsers
// ---------------------------------------------------------------------------

pub fn parse_city_feed_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let mut next_page = data.get("nextPage").and_then(as_bool);
    if next_page.is_none() {
        next_page = data.get("hasNext").and_then(as_bool);
    }
    let raw_sections = as_list(data.get("sections").unwrap_or(&Value::Null));
    let item_cards: Vec<Value> = raw_sections
        .iter()
        .enumerate()
        .filter_map(|(i, s)| {
            if s.is_object() {
                parse_feed_item_card(i as i64, s)
            } else {
                None
            }
        })
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("card_list", data.get("cardList").cloned()),
        ("headers", data.get("headers").cloned()),
        ("sections", data.get("sections").cloned()),
        (
            "feeds_count",
            opt_i64(data.get("feedsCount").and_then(as_int_lenient)),
        ),
        ("next_page", opt_bool(next_page)),
        ("extra_param", data.get("extraParam").cloned()),
        ("item_cards", Some(Value::Array(item_cards))),
    ]))
}

pub fn parse_city_nearby_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let items: Vec<Value> = as_list(data.get("nearByItemInfos").unwrap_or(&Value::Null))
        .iter()
        .filter(|r| r.is_object())
        .map(|raw| {
            build_obj(vec![
                ("item_id", raw.get("itemId").cloned()),
                ("pic_link", raw.get("picLink").cloned()),
                ("distance_desc", raw.get("distanceDesc").cloned()),
                ("pre_desc", raw.get("preDesc").cloned()),
                ("cat_level4_name", raw.get("catLevel4Name").cloned()),
                ("target_url", raw.get("targetUrl").cloned()),
                ("click_param", raw.get("clickParam").cloned()),
            ])
        })
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("items", Some(Value::Array(items))),
    ]))
}

fn pond_summary(raw: &Value) -> Value {
    build_obj(vec![
        ("pond_id", opt_i64(raw.get("id").and_then(as_int_lenient))),
        ("pool_name", raw.get("poolName").cloned()),
        ("pic_url", raw.get("picUrl").cloned()),
        ("icon_url", raw.get("iconUrl").cloned()),
        ("item_num", raw.get("itemNum").cloned()),
        ("user_cnt_str", raw.get("userCntStr").cloned()),
        ("city", raw.get("city").cloned()),
        ("prov", raw.get("prov").cloned()),
        ("area", raw.get("area").cloned()),
        ("distance_km", raw.get("distanceKm").cloned()),
        ("distance_string", raw.get("distanceString").cloned()),
        ("pool_type", raw.get("poolType").cloned()),
        ("pool_status", raw.get("poolStatus").cloned()),
        (
            "pool_dimension",
            opt_i64(raw.get("poolDimension").and_then(as_int_lenient)),
        ),
        (
            "admin_user_id",
            opt_i64(raw.get("adminUserId").and_then(as_int_lenient)),
        ),
        ("admin_user_nick", raw.get("adminUserNick").cloned()),
        (
            "is_allow_publish",
            opt_bool(raw.get("isAllowPublish").and_then(as_bool)),
        ),
    ])
}

pub fn parse_fishpond_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let items: Vec<Value> = as_list(data.get("items").unwrap_or(&Value::Null))
        .iter()
        .filter(|r| r.is_object())
        .map(pond_summary)
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("items", Some(Value::Array(items))),
        (
            "next_page",
            opt_bool(data.get("nextPage").and_then(as_bool)),
        ),
        (
            "total_count",
            opt_i64(data.get("totalCount").and_then(as_int_lenient)),
        ),
        ("item_urls", data.get("itemUrls").cloned()),
    ]))
}

pub fn parse_fishpond_info_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let pool = data
        .get("defaultFishPool")
        .filter(|p| p.is_object())
        .cloned()
        .unwrap_or(json!({}));
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("pond_id", opt_i64(pool.get("id").and_then(as_int_lenient))),
        ("pool_name", pool.get("poolName").cloned()),
        ("pic_url", pool.get("picUrl").cloned()),
        ("icon_url", pool.get("iconUrl").cloned()),
        ("item_num", pool.get("itemNum").cloned()),
        ("follower_num", pool.get("followerNum").cloned()),
        ("city", pool.get("city").cloned()),
        ("prov", pool.get("prov").cloned()),
        ("area", pool.get("area").cloned()),
        ("distance_km", pool.get("distanceKm").cloned()),
        ("distance_string", pool.get("distanceString").cloned()),
        ("pool_type", pool.get("poolType").cloned()),
        ("pool_status", pool.get("poolStatus").cloned()),
        ("level", opt_i64(pool.get("level").and_then(as_int_lenient))),
        (
            "admin_user_id",
            opt_i64(pool.get("adminUserId").and_then(as_int_lenient)),
        ),
        ("admin_user_nick", pool.get("adminUserNick").cloned()),
        (
            "is_allow_publish",
            opt_bool(pool.get("isAllowPublish").and_then(as_bool)),
        ),
        ("topic_title_list", pool.get("topicTitleList").cloned()),
        ("announcements", pool.get("announcementList").cloned()),
        ("server_time", data.get("serverTime").cloned()),
    ]))
}

fn pond_comment(raw: &Value) -> Value {
    build_obj(vec![
        ("comment_id", raw.get("commentId").cloned()),
        ("content", raw.get("content").cloned()),
        ("img_url", raw.get("imgUrl").cloned()),
        ("img_size", raw.get("imgSize").cloned()),
        (
            "long_report_time",
            opt_i64(raw.get("longReportTime").and_then(as_int_lenient)),
        ),
        ("pool_id", raw.get("poolId").cloned()),
        ("position", raw.get("position").cloned()),
        ("reporter_id", raw.get("reporterId").cloned()),
        ("reporter_nick", raw.get("reporterNick").cloned()),
        ("sub_content", raw.get("subContent").cloned()),
        ("sub_user_id", raw.get("subUserId").cloned()),
        ("sub_user_nick", raw.get("subUserNick").cloned()),
        ("type", opt_i64(raw.get("type").and_then(as_int_lenient))),
    ])
}

pub fn parse_fishpond_comments_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let items: Vec<Value> = as_list(data.get("items").unwrap_or(&Value::Null))
        .iter()
        .filter(|r| r.is_object())
        .map(pond_comment)
        .collect();
    let sys_comments: Vec<Value> = as_list(data.get("sysCommentList").unwrap_or(&Value::Null))
        .iter()
        .filter(|r| r.is_object())
        .map(pond_comment)
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("items", Some(Value::Array(items))),
        ("sys_comments", Some(Value::Array(sys_comments))),
        (
            "next_page",
            opt_bool(data.get("nextPage").and_then(as_bool)),
        ),
        (
            "pool_id",
            opt_i64(data.get("poolId").and_then(as_int_lenient)),
        ),
        ("pool_name", data.get("poolName").cloned()),
        ("server_time", data.get("serverTime").cloned()),
        (
            "total_count",
            opt_i64(data.get("totalCount").and_then(as_int_lenient)),
        ),
    ]))
}

pub fn parse_fun_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let need_refresh = response
        .get("data")
        .and_then(|d| d.get("data"))
        .filter(|d| d.is_object())
        .and_then(|inner| inner.get("needRefresh"))
        .and_then(as_bool);
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("need_refresh", opt_bool(need_refresh)),
        ("flow_data", data.get("flowData").cloned()),
    ]))
}

fn fun_comment(root: &Value) -> Value {
    // Iterative post-order build, as in the Python parser.
    let mut nodes: Vec<Value> = vec![root.clone()];
    let mut children: Vec<Vec<usize>> = vec![Vec::new()];
    let mut work: Vec<usize> = vec![0];
    while let Some(idx) = work.pop() {
        let node = nodes[idx].clone();
        for reply in as_list(node.get("reply").unwrap_or(&Value::Null)) {
            if reply.is_object() {
                let child_idx = nodes.len();
                nodes.push(reply.clone());
                children.push(Vec::new());
                children[idx].push(child_idx);
                work.push(child_idx);
            }
        }
    }
    let mut built: Vec<Option<Value>> = vec![None; nodes.len()];
    for idx in (0..nodes.len()).rev() {
        let replies: Vec<Value> = children[idx]
            .iter()
            .map(|c| built[*c].clone().unwrap_or(json!({})))
            .collect();
        built[idx] = Some(build_fun_comment(&nodes[idx], replies));
    }
    built[0].clone().unwrap_or(json!({}))
}

fn build_fun_comment(raw: &Value, replies: Vec<Value>) -> Value {
    build_obj(vec![
        (
            "comment_id",
            opt_i64(raw.get("commentId").and_then(as_int_lenient)),
        ),
        ("content", raw.get("content").cloned()),
        ("reporter_id", raw.get("reporterId").cloned()),
        ("reporter_nick", raw.get("reporterNick").cloned()),
        ("portrait_url", raw.get("portraitUrl").cloned()),
        ("report_time_str", raw.get("reportTimeStr").cloned()),
        ("ip_region_address", raw.get("ipRegionAddress").cloned()),
        ("item_id", raw.get("itemId").cloned()),
        (
            "like_count",
            opt_i64(raw.get("likeCount").and_then(as_int_lenient)),
        ),
        ("has_reply", opt_bool(raw.get("hasReply").and_then(as_bool))),
        ("top", opt_bool(raw.get("top").and_then(as_bool))),
        ("hot", opt_bool(raw.get("hot").and_then(as_bool))),
        ("cursor", raw.get("cursor").cloned()),
        (
            "parent_comment_id",
            opt_i64(raw.get("parentCommentId").and_then(as_int_lenient)),
        ),
        ("images", raw.get("images").cloned()),
        ("replies", Some(Value::Array(replies))),
        (
            "reply_total_count",
            opt_i64(raw.get("replyTotalCount").and_then(as_int_lenient)),
        ),
    ])
}

pub fn parse_fun_comment_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let items: Vec<Value> = as_list(data.get("items").unwrap_or(&Value::Null))
        .iter()
        .filter(|r| r.is_object())
        .map(fun_comment)
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("items", Some(Value::Array(items))),
        ("total", opt_i64(data.get("total").and_then(as_int_lenient))),
        ("next", opt_bool(data.get("next").and_then(as_bool))),
        ("last_cursor", data.get("lastCursor").cloned()),
        (
            "comment_cnt",
            opt_i64(data.get("commentCnt").and_then(as_int_lenient)),
        ),
        ("item_id", data.get("itemId").cloned()),
        ("seller_id", data.get("sellerId").cloned()),
        ("target_id", data.get("targetId").cloned()),
        ("target_type", data.get("targetType").cloned()),
        ("target_user_id", data.get("targetUserId").cloned()),
    ]))
}

fn filter_values(raw_values: &Value) -> Vec<Value> {
    as_list(raw_values)
        .iter()
        .filter(|r| r.is_object())
        .map(|raw| {
            build_obj(vec![
                ("value_id", raw.get("valueId").cloned()),
                ("value_name", raw.get("valueName").cloned()),
                ("selected", opt_bool(raw.get("selected").and_then(as_bool))),
                ("prop_id", raw.get("propId").cloned()),
            ])
        })
        .collect()
}

pub fn parse_search_filter_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let mut groups: Vec<Value> = Vec::new();
    let mut protocol: Option<Value> = None;
    if let Some(flex) = data.get("flexFilter") {
        if flex.is_object() {
            protocol = flex.get("protocol").cloned();
            for raw_group in as_list(flex.get("data").unwrap_or(&Value::Null)) {
                if !raw_group.is_object() {
                    continue;
                }
                let elements: Vec<Value> =
                    as_list(raw_group.get("element").unwrap_or(&Value::Null))
                        .iter()
                        .filter(|e| e.is_object())
                        .map(|raw_el| {
                            let el_data = raw_el
                                .get("data")
                                .filter(|d| d.is_object())
                                .cloned()
                                .unwrap_or(json!({}));
                            build_obj(vec![
                                ("filter_type", raw_el.get("filterType").cloned()),
                                (
                                    "selected",
                                    opt_bool(raw_el.get("selected").and_then(as_bool)),
                                ),
                                ("pid", el_data.get("pid").cloned()),
                                ("pname", el_data.get("pname").cloned()),
                                (
                                    "values",
                                    Some(Value::Array(filter_values(
                                        el_data.get("valueList").unwrap_or(&Value::Null),
                                    ))),
                                ),
                            ])
                        })
                        .collect();
                groups.push(build_obj(vec![
                    ("name", raw_group.get("name").cloned()),
                    ("elements", Some(Value::Array(elements))),
                ]));
            }
        }
    }
    let mut panel_cards: Vec<Value> = Vec::new();
    if let Some(panel) = data.get("smartUiFilterPanelDO") {
        if panel.is_object() {
            for raw_card in as_list(panel.get("filterPanelDOList").unwrap_or(&Value::Null)) {
                if !raw_card.is_object() {
                    continue;
                }
                let card_data = raw_card
                    .get("cardData")
                    .filter(|d| d.is_object())
                    .cloned()
                    .unwrap_or(json!({}));
                panel_cards.push(build_obj(vec![
                    ("card_type", raw_card.get("cardType").cloned()),
                    ("prop_id", card_data.get("propId").cloned()),
                    ("prop_name", card_data.get("propName").cloned()),
                    ("price_range", card_data.get("priceRange").cloned()),
                    ("placeholder", card_data.get("placeholder").cloned()),
                    ("req", opt_bool(card_data.get("req").and_then(as_bool))),
                    (
                        "values",
                        Some(Value::Array(filter_values(
                            card_data.get("value").unwrap_or(&Value::Null),
                        ))),
                    ),
                ]));
            }
        }
    }
    let use_filter_sheet = data
        .get("filterSheetDO")
        .filter(|s| s.is_object())
        .and_then(|sheet| sheet.get("useFilterSheet"))
        .and_then(as_bool);
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("groups", Some(Value::Array(groups))),
        ("protocol", protocol),
        ("panel_cards", Some(Value::Array(panel_cards))),
        ("use_filter_sheet", opt_bool(use_filter_sheet)),
    ]))
}

// ---------------------------------------------------------------------------
// similar.py
// ---------------------------------------------------------------------------

fn parse_similar_item(obj: &Value) -> Value {
    let item_id = match obj.get("itemId") {
        Some(v) if !v.is_null() => Some(v.clone()),
        _ => obj.get("id").cloned(),
    };
    build_obj(vec![
        ("item_id", item_id),
        ("title", obj.get("title").cloned()),
        ("price", obj.get("price").cloned()),
        ("original_price", obj.get("originalPrice").cloned()),
        ("price_unit", obj.get("priceUnit").cloned()),
        ("price_text", obj.get("priceText").cloned()),
        ("pic_url", obj.get("picUrl").cloned()),
        ("ori_pic_url", obj.get("oriPicUrl").cloned()),
        ("image_urls", obj.get("imageUrls").cloned()),
        ("category_id", obj.get("categoryId").cloned()),
        ("category_name", obj.get("categoryName").cloned()),
        ("user_id", obj.get("userId").cloned()),
        ("user_nick", obj.get("userNick").cloned()),
        ("province", obj.get("province").cloned()),
        ("city", obj.get("city").cloned()),
        ("area", obj.get("area").cloned()),
        ("want_num", obj.get("wantNum").cloned()),
        ("sold", opt_bool(obj.get("sold").and_then(as_bool))),
        ("online", opt_bool(obj.get("online").and_then(as_bool))),
        ("free_ship", opt_bool(obj.get("freeShip").and_then(as_bool))),
        ("can_buy", opt_bool(obj.get("canBuy").and_then(as_bool))),
        ("video_id", obj.get("videoId").cloned()),
        ("video_cover_url", obj.get("videoCoverUrl").cloned()),
        (
            "user_info",
            obj.get("userInfo").filter(|u| u.is_object()).cloned(),
        ),
    ])
}

pub fn parse_spusimilar_response(response: &Value) -> Result<Value> {
    let (envelope, data) = data_or_raise(response)?;
    let (api, version) = api_version(&envelope);
    let cards: Vec<Value> = as_list(data.get("cardList").unwrap_or(&Value::Null))
        .iter()
        .filter(|c| c.is_object())
        .map(|card| {
            let items: Vec<Value> = card
                .get("cardData")
                .filter(|d| d.is_object())
                .map(|card_data| {
                    as_list(card_data.get("items").unwrap_or(&Value::Null))
                        .iter()
                        .filter(|e| e.is_object())
                        .map(parse_similar_item)
                        .collect()
                })
                .unwrap_or_default();
            build_obj(vec![
                ("card_type", card.get("cardType").cloned()),
                ("items", Some(Value::Array(items))),
            ])
        })
        .collect();
    Ok(build_obj(vec![
        ("api", Some(api)),
        ("version", Some(version)),
        ("cards", Some(Value::Array(cards))),
        (
            "next_page",
            opt_bool(data.get("nextPage").and_then(as_bool)),
        ),
        ("server_time", data.get("serverTime").cloned()),
        (
            "need_decrypt_keys",
            data.get("needDecryptKeys")
                .filter(|k| k.is_array())
                .cloned(),
        ),
    ]))
}

/// Dispatch by API name — the analogue of the registry `parser_function`
/// mapping in `goofish_client.api_registry.parser_callable`.
pub fn parse_for_api(api_name: &str, response: &Value) -> Result<Value> {
    match api_name {
        "mtop.taobao.idlemtopsearch.search" => parse_search_response(response),
        "mtop.taobao.idlemtopsearch.search.suggest" => parse_suggest_response(response),
        "mtop.taobao.idle.category.guide" => parse_category_response(response),
        "mtop.taobao.idle.local.home" | "mtop.taobao.idle.local.home.feeds" => {
            parse_city_feed_response(response)
        }
        "mtop.taobao.idle.local.near.by.corner.info" => parse_city_nearby_response(response),
        "com.taobao.idle.fishpool.list" => parse_fishpond_response(response),
        "mtop.taobao.idle.fishpool.id" => parse_fishpond_info_response(response),
        "com.taobao.idle.fishpool.comment.list" => parse_fishpond_comments_response(response),
        "mtop.taobao.idle.awesome.detail.unit" => parse_item_response(response),
        "mtop.taobao.idle.comment.list" => parse_comment_response(response),
        "mtop.taobao.idle.rate.list" => parse_rate_response(response),
        "mtop.taobao.idle.item.recommend.list" => parse_recommend_response(response),
        "mtop.taobao.idle.fun.item.detail" => parse_fun_response(response),
        "mtop.taobao.idle.fun.general.comment.list" => parse_fun_comment_response(response),
        "mtop.taobao.idle.user.publish.items" => parse_seller_items_response(response),
        "mtop.taobao.idlehome.home.nextfresh" => parse_home_feed_response(response),
        "com.taobao.idle.video.play" => parse_video_play_response(response),
        "mtop.taobao.idle.user.headinfo.get" => parse_user_headinfo_response(response),
        "mtop.taobao.idle.search.spusimilar.itemlist" => parse_spusimilar_response(response),
        "mtop.taobao.idle.item.search.filter" => parse_search_filter_response(response),
        _ => Err(anyhow!("no parser registered for {api_name}")),
    }
}

/// Whether `parse_for_api` has a parser for this API (the client checks
/// this before sending, like the Python client's parser lookup).
pub fn parser_supported(api_name: &str) -> bool {
    matches!(
        api_name,
        "mtop.taobao.idlemtopsearch.search"
            | "mtop.taobao.idlemtopsearch.search.suggest"
            | "mtop.taobao.idle.category.guide"
            | "mtop.taobao.idle.local.home"
            | "mtop.taobao.idle.local.home.feeds"
            | "mtop.taobao.idle.local.near.by.corner.info"
            | "com.taobao.idle.fishpool.list"
            | "mtop.taobao.idle.fishpool.id"
            | "com.taobao.idle.fishpool.comment.list"
            | "mtop.taobao.idle.awesome.detail.unit"
            | "mtop.taobao.idle.comment.list"
            | "mtop.taobao.idle.rate.list"
            | "mtop.taobao.idle.item.recommend.list"
            | "mtop.taobao.idle.fun.item.detail"
            | "mtop.taobao.idle.fun.general.comment.list"
            | "mtop.taobao.idle.user.publish.items"
            | "mtop.taobao.idlehome.home.nextfresh"
            | "com.taobao.idle.video.play"
            | "mtop.taobao.idle.user.headinfo.get"
            | "mtop.taobao.idle.search.spusimilar.itemlist"
            | "mtop.taobao.idle.item.search.filter"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), name);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        serde_json::from_str(&text).unwrap()
    }

    // Ported from goofish-parser/tests/test_search.py
    #[test]
    fn search_parses_items_and_meta() {
        let resp = parse_search_response(&fixture("search_response.json")).unwrap();
        assert_eq!(resp["api"], json!("mtop.taobao.idlemtopsearch.search"));
        assert_eq!(resp["hitnum"], json!(2));
        assert_eq!(resp["has_next_page"], json!(true));
        assert_eq!(resp["items"].as_array().unwrap().len(), 3);
        assert_eq!(resp["prefix_cards"], json!([]));
        assert_eq!(resp["relate_items"], json!([]));
        assert_eq!(resp["top_list"], json!([]));
    }

    #[test]
    fn search_item_extraction() {
        let resp = parse_search_response(&fixture("search_response.json")).unwrap();
        let item = &resp["items"][0];
        assert_eq!(item["item_id"], json!("1001"));
        assert_eq!(item["title"], json!("示例商品标题"));
        assert_eq!(item["rich_title"], json!("示例商品标题"));
        assert_eq!(item["price_text"], json!("1280"));
        assert_eq!(item["sold_price"], json!("1280"));
        assert_eq!(item["ori_price"], json!("1999"));
        assert_eq!(item["area"], json!("上海"));
        assert_eq!(item["p_city"], json!("上海"));
        assert_eq!(item["category_id"], json!("50012345"));
        assert_eq!(item["show_index"], json!("0"));
        assert_eq!(item["keyword"], json!("示例关键词"));
        assert_eq!(item["want_text"], json!("12人想要"));
        assert_eq!(item["user_nick_name"], json!("示例卖家甲"));
        assert_eq!(item["target_url"], json!("fleamarket://item?id=1001"));
        assert_eq!(item["section"], json!("resultList"));
        assert_eq!(item["index_in_section"], json!(0));
        assert_eq!(
            item["pic_url"],
            json!("https://example.invalid/pic1001.jpg")
        );
        assert_eq!(item["pic_width"], json!(800));
        assert_eq!(item["pic_height"], json!(800));
        assert_eq!(item["template_name"], json!("idlefish_search_card_item"));
        assert_eq!(item["template_version"], json!(20));
        assert_eq!(item["card_type"], json!("dx"));
        assert_eq!(item["card_style"], json!("waterfall"));
    }

    #[test]
    fn search_ad_detection() {
        let payload = fixture("search_response.json");
        let resp = parse_search_response(&payload).unwrap();
        assert_eq!(resp["items"][0]["is_ad"], json!(false));
        assert_eq!(resp["items"][1]["is_ad"], json!(true));
        assert_eq!(resp["items"][1]["biz_type"], json!("ad"));
        assert_eq!(resp["items"][1]["is_alimama_ad_raw"], json!(true));
        // case-insensitive, empty arg1 -> not ad
        let mut payload = payload;
        let card = &mut payload["data"]["resultList"][1];
        card["data"]["item"]["main"]["clickParam"]["arg1"] = json!("ITEM");
        card["data"]["item"]["main"]["clickParam"]["args"]["biz_type"] = json!("AD");
        let resp = parse_search_response(&payload).unwrap();
        assert_eq!(resp["items"][1]["is_ad"], json!(true));
        payload["data"]["resultList"][1]["data"]["item"]["main"]["clickParam"]["arg1"] = json!("");
        let resp = parse_search_response(&payload).unwrap();
        assert_eq!(resp["items"][1]["is_ad"], json!(false));
        // unknown card kept, fields absent
        let resp = parse_search_response(&fixture("search_response.json")).unwrap();
        let unknown = &resp["items"][2];
        assert!(unknown.get("item_id").is_none());
        assert!(unknown.get("title").is_none());
        assert_eq!(unknown["is_ad"], json!(false));
        assert_eq!(unknown["card_type"], json!("unknown_banner"));
    }

    #[test]
    fn search_empty_and_null_fields() {
        let payload = json!({
            "api": "mtop.taobao.idlemtopsearch.search", "v": "1.0",
            "ret": ["SUCCESS::调用成功"],
            "data": {"resultList": [], "resultInfo": {"hasNextPage": false}},
        });
        let resp = parse_search_response(&payload).unwrap();
        assert_eq!(resp["items"], json!([]));
        assert_eq!(resp["has_next_page"], json!(false));
        assert!(resp.get("hitnum").is_none());
        let payload = json!({
            "api": "mtop.taobao.idlemtopsearch.search", "v": "1.0",
            "ret": ["SUCCESS::调用成功"],
            "data": {
                "resultList": [
                    {"type": "dx", "style": null,
                     "data": {"item": {"main": {"exContent": null, "clickParam": null, "targetUrl": null}}}},
                    {"type": "dx", "data": null},
                ],
                "resultInfo": null,
            },
        });
        let resp = parse_search_response(&payload).unwrap();
        assert_eq!(resp["items"].as_array().unwrap().len(), 2);
        assert!(resp["items"][0].get("item_id").is_none());
        assert!(resp.get("has_next_page").is_none());
    }

    #[test]
    fn search_error_envelope_raises() {
        let result = parse_search_response(&fixture("search_error.json"));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("FAIL_BIZ_ITEM_ERROR"), "{msg}");
    }

    #[test]
    fn envelope_basics() {
        let env =
            parse_mtop_envelope(&json!({"api": "a", "v": 1.0, "ret": ["SUCCESS::ok"], "data": {}}))
                .unwrap();
        assert_eq!(env["is_success"], json!(true));
        assert_eq!(env["ret_code"], json!("SUCCESS"));
        let env = parse_mtop_envelope(&json!({"ret": []})).unwrap();
        assert_eq!(env["is_success"], json!(false));
        assert!(parse_mtop_envelope(&json!([])).is_err());
    }

    // Ported from goofish-parser/tests/test_item.py
    #[test]
    fn detail_core_fields() {
        let detail = parse_item_response(&fixture("item_detail.json")).unwrap();
        assert_eq!(detail["item_id"], json!("1001"));
        assert_eq!(detail["title"], json!("示例商品标题"));
        assert_eq!(detail["desc"], json!("示例商品描述"));
        assert_eq!(detail["price"], json!("1280"));
        assert_eq!(detail["channel_cat_id"], json!("50012345"));
        let pairs: Vec<(String, String)> = detail["cpv_attributes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["name"].as_str().unwrap().to_string(),
                    a["value"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("成色".into(), "9成新".into()),
                ("品牌".into(), "示例品牌".into())
            ]
        );
        assert_eq!(detail["cpv_text"], json!("成色:9成新,品牌:示例品牌"));
        assert_eq!(detail["is_free_shipping"], json!(true));
        assert_eq!(detail["fav_count"], json!(45));
        assert_eq!(detail["want_count"], json!(12));
        assert_eq!(detail["view_count"], json!(345));
    }

    #[test]
    fn detail_seller_and_images() {
        let detail = parse_item_response(&fixture("item_detail.json")).unwrap();
        let seller = &detail["seller"];
        assert_eq!(seller["nick"], json!("示例卖家甲"));
        assert_eq!(seller["seller_id"], json!("2002"));
        assert_eq!(seller["city"], json!("上海"));
        assert_eq!(seller["identity_tag"], json!("实人认证"));
        assert_eq!(seller["desc"], json!("卖出3件宝贝，信用良好"));
        assert_eq!(seller["sold_count"], json!(3));
        let images = detail["images"].as_array().unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0]["url"], json!("https://example.invalid/img1.jpg"));
        assert_eq!(images[0]["is_video"], json!(false));
        assert_eq!(images[0]["width"], json!(800.0));
        assert_eq!(images[1]["is_video"], json!(true));
        assert_eq!(
            images[1]["cover_url"],
            json!("https://example.invalid/cover1.jpg")
        );
        let renders: Vec<String> = detail["components"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["render"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            renders,
            vec![
                "Basic_Native_spn_app_bar",
                "Basic_Native_spn_image_group",
                "Basic_Native_spn_dynamic_dx"
            ]
        );
    }

    #[test]
    fn detail_userdata_fallbacks() {
        let user_data = serde_json::to_string(&json!({
            "displayName": "回退昵称",
            "introduction": "已卖出7件",
            "tagInfo": serde_json::to_string(&json!({"tagText": "回退标签"})).unwrap(),
        }))
        .unwrap();
        let payload = json!({
            "flowData": {
                "body": {"sections": [{"components": [
                    {"render": "x", "key": "k",
                     "data": {"itemId": "9", "aoiInfo": ["上海市", "浦东新区"]}},
                ]}]},
                "context": {"userData": user_data},
            }
        });
        let detail = parse_item_response(&payload).unwrap();
        assert_eq!(detail["seller"]["nick"], json!("回退昵称"));
        assert_eq!(detail["seller"]["desc"], json!("已卖出7件"));
        assert_eq!(detail["seller"]["sold_count"], json!(7));
        assert_eq!(detail["seller"]["identity_tag"], json!("回退标签"));
        assert_eq!(detail["seller"]["city"], json!("浦东新区"));
    }

    #[test]
    fn detail_defaults_and_envelope_unwrap() {
        let detail = parse_item_response(&json!({"somethingElse": {}})).unwrap();
        assert_eq!(detail["item_id"], json!(""));
        assert_eq!(detail["seller"]["nick"], json!(""));
        assert_eq!(detail["images"], json!([]));
        let payload = json!({
            "flowData": {"body": {"sections": [{"components": [
                {"render": "x", "data": {"desc": "只有描述"}},
            ]}]}},
        });
        let detail = parse_item_response(&payload).unwrap();
        assert_eq!(detail["title"], json!("只有描述"));
        let wrapped = json!({
            "api": "mtop.taobao.idle.awesome.detail.unit", "v": "1.0",
            "ret": ["SUCCESS::调用成功"], "data": fixture("item_detail.json"),
        });
        let detail = parse_item_response(&wrapped).unwrap();
        assert_eq!(detail["item_id"], json!("1001"));
        // measured detail fixture parses and yields images
        let detail = parse_item_response(&fixture("detail_media_round19.json")).unwrap();
        assert!(!detail["images"].as_array().unwrap().is_empty());
    }

    // Ported fixture expectations from goofish-parser/tests/test_round7.py
    #[test]
    fn browse_fixtures() {
        let bundle = fixture("browse_round7.json");
        let guide = parse_category_response(&bundle["category_guide"]).unwrap();
        assert_eq!(guide["api"], json!("mtop.taobao.idle.category.guide"));
        assert_eq!(guide["version"], json!("2.1"));
        let cat_ids: Vec<String> = guide["categories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["cat_id"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(cat_ids.len(), 3);
        assert_eq!(guide["categories"][0]["name"], json!("手机"));
        assert_eq!(
            guide["categories"][0]["track_params"],
            json!({"spm": "a2170.7897998.1"})
        );
        let card_types: Vec<String> = guide["cards"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["card_type"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(card_types, vec!["CategoryCard", "BannerCard"]);
        assert_eq!(guide["card_data_map"], json!({"c1": {"title": "示例分区"}}));

        let feed = parse_recommend_response(&bundle["recommend"]).unwrap();
        assert_eq!(feed["has_more"], json!(true));
        assert_eq!(feed["server_time"], json!("1700000000000"));
        assert_eq!(feed["need_decrypt_keys"], json!(["userId"]));
        let first = &feed["cards"][0];
        assert_eq!(first["card_type"], json!("item"));
        assert_eq!(first["item"]["item_id"], json!("2001"));
        assert_eq!(first["item"]["title"], json!("示例相关商品"));
        assert_eq!(first["item"]["sold_price"], json!("256"));
        assert_eq!(first["item"]["area"], json!("深圳"));

        let result = parse_comment_response(&bundle["comments"]).unwrap();
        assert_eq!(result["total_count"], json!("2"));
        assert_eq!(result["total_rate_count"], json!("1"));
        assert_eq!(result["next_page"], json!(false));
        let first = &result["comments"][0];
        assert_eq!(first["comment_id"], json!("c100"));
        assert_eq!(first["content"], json!("示例评论内容"));
        assert_eq!(first["reporter_id"], json!("30001"));
        assert_eq!(first["seller_id"], json!("40001"));
        assert_eq!(
            first["pict_urls"],
            json!(["https://example.invalid/c100-1.jpg"])
        );
        assert_eq!(first["replies"].as_array().unwrap().len(), 1);
        assert_eq!(first["replies"][0]["comment_id"], json!("c101"));
        assert_eq!(first["replies"][0]["parent_comment_id"], json!("c100"));

        let rates = parse_rate_response(&bundle["rates"]).unwrap();
        assert_eq!(rates["next_page"], json!(true));
        assert_eq!(rates["total_rate_count"], json!("3"));
        assert_eq!(rates["cards"].as_array().unwrap().len(), 2);
        assert_eq!(rates["displayable_count"], json!(1));
        let comment = &rates["cards"][0]["comment"];
        assert_eq!(comment["feedback"], json!("示例评价文本"));
        assert_eq!(comment["rate_id"], json!("r100"));
        assert_eq!(comment["rated_uid"], json!("40001"));

        let items = parse_seller_items_response(&bundle["seller_items"]).unwrap();
        assert_eq!(items["next_page"], json!(true));
        assert_eq!(items["total_count"], json!(21));

        let home = parse_home_feed_response(&bundle["home_feed"]).unwrap();
        assert_eq!(home["feeds_count"], json!(10));
        assert_eq!(home["next_page"], json!(true));
        assert_eq!(
            home["client_recycle_params"],
            json!("recycle-token-example")
        );

        let suggest = parse_suggest_response(&bundle["suggest_dynamic"]).unwrap();
        assert_eq!(suggest["bucket_id"], json!(7));
        let kinds: Vec<String> = suggest["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, vec!["item", "user", "picText", "dx"]);
        assert_eq!(suggest["entries"][0]["suggest"], json!("iphone 15"));
        let suggest_static = parse_suggest_response(&bundle["suggest_static"]).unwrap();
        let kinds: Vec<String> = suggest_static["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["kind"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(kinds, vec!["picText", "user", "item"]);
    }

    // Ported fixture expectations from goofish-parser/tests/test_round8.py
    #[test]
    fn explore_fixtures() {
        let bundle = fixture("explore_round8.json");
        let feed = parse_city_feed_response(&bundle["city_feed"]).unwrap();
        assert_eq!(feed["api"], json!("mtop.taobao.idle.local.home.feeds"));
        assert_eq!(feed["version"], json!("3.0"));
        assert_eq!(feed["feeds_count"], json!(40));
        assert_eq!(feed["next_page"], json!(true));
        assert_eq!(
            feed["extra_param"],
            json!({"sessionId": "synthetic-session"})
        );
        let last = parse_city_feed_response(&bundle["city_feed_last"]).unwrap();
        assert_eq!(last["next_page"], json!(false));
        assert_eq!(last["feeds_count"], json!(0));

        let nearby = parse_city_nearby_response(&bundle["city_nearby"]).unwrap();
        let item = &nearby["items"][0];
        assert_eq!(item["item_id"], json!("700000000002"));
        assert_eq!(item["distance_desc"], json!("500m"));
        assert_eq!(item["cat_level4_name"], json!("手机"));

        let ponds = parse_fishpond_response(&bundle["fishpond_list"]).unwrap();
        assert_eq!(ponds["api"], json!("com.taobao.idle.fishpool.list"));
        assert_eq!(ponds["next_page"], json!(true));
        assert_eq!(ponds["total_count"], json!(57));
        let first = &ponds["items"][0];
        assert_eq!(first["pond_id"], json!(88001234));
        assert_eq!(first["pool_name"], json!("杭州数码闲置"));
        assert_eq!(first["item_num"], json!("1234"));
        assert_eq!(first["distance_km"], json!("1.5"));
        assert_eq!(first["admin_user_id"], json!(99001122));
        assert_eq!(first["is_allow_publish"], json!(true));

        let info = parse_fishpond_info_response(&bundle["fishpond_info"]).unwrap();
        assert_eq!(info["pond_id"], json!(88001234));
        assert_eq!(info["follower_num"], json!("321"));
        assert_eq!(info["level"], json!(3));
        assert_eq!(info["topic_title_list"], json!(["数码", "自提"]));
        assert_eq!(info["server_time"], json!("1700000000000"));

        let comments = parse_fishpond_comments_response(&bundle["fishpond_comments"]).unwrap();
        assert_eq!(comments["next_page"], json!(false));
        assert_eq!(comments["total_count"], json!(3));
        assert_eq!(comments["pool_id"], json!(88001234));
        let c0 = &comments["items"][0];
        assert_eq!(c0["pool_id"], json!("88001234"));
        assert_eq!(c0["comment_id"], json!("c100"));
        assert_eq!(c0["long_report_time"], json!(1700000000000i64));
        assert_eq!(c0["reporter_nick"], json!("买家甲"));

        let fun = parse_fun_response(&bundle["fun_detail"]).unwrap();
        assert_eq!(fun["api"], json!("mtop.taobao.idle.fun.item.detail"));
        assert_eq!(fun["need_refresh"], json!(true));

        let page = parse_fun_comment_response(&bundle["fun_comments"]).unwrap();
        assert_eq!(page["total"], json!(42));
        assert_eq!(page["next"], json!(true));
        assert_eq!(page["last_cursor"], json!("cur_2"));
        assert_eq!(page["comment_cnt"], json!(42));
        assert_eq!(page["seller_id"], json!("440011"));
        assert_eq!(page["items"][0]["comment_id"], json!(9001));
        assert_eq!(page["items"][0]["like_count"], json!(12));
        assert_eq!(page["items"][0]["hot"], json!(true));

        let filters = parse_search_filter_response(&bundle["search_filters"]).unwrap();
        assert!(filters["groups"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false));
    }

    #[test]
    fn spusimilar_fixture() {
        let result = parse_spusimilar_response(&fixture("spusimilar_round16.json")).unwrap();
        assert_eq!(
            result["api"],
            json!("mtop.taobao.idle.search.spusimilar.itemlist")
        );
        assert_eq!(result["cards"].as_array().unwrap().len(), 2);
        assert_eq!(result["next_page"], json!(true));
        assert_eq!(result["server_time"], json!("1700000000000"));
        let items = result["cards"][0]["items"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["item_id"], json!("800000001"));
        assert_eq!(items[0]["title"], json!("SYNTHETIC similar item one"));
        assert_eq!(items[0]["price"], json!("199.00"));
        assert_eq!(items[0]["original_price"], json!("259.00"));
        assert_eq!(items[0]["sold"], json!(false));
        assert_eq!(items[0]["online"], json!(true));
        assert_eq!(items[1]["item_id"], json!("800000002"));
        assert_eq!(items[1]["price"], json!(88.5));
        assert_eq!(result["cards"][1]["items"], json!([]));
    }

    #[test]
    fn video_play_and_headinfo_fixtures() {
        let bundle = fixture("video_play_round10.json");
        let full = parse_video_play_response(&bundle["video_play_full"]).unwrap();
        assert!(full.get("play_url").is_some());
        let bundle = fixture("headinfo_round9.json");
        let full = parse_user_headinfo_response(&bundle["headinfo_full"]).unwrap();
        assert!(full.get("user_id").is_some() || full.get("nick").is_some());
    }

    #[test]
    fn measured_feed_fixture_cards() {
        let feed = parse_city_feed_response(&fixture("feed_city_media_round25.json")).unwrap();
        let cards = feed["item_cards"].as_array().unwrap();
        assert_eq!(cards.len(), 3);
        assert!(cards.iter().all(|c| c.get("item_id").is_some()));
        assert_eq!(
            cards.iter().filter(|c| c["is_ad"] == json!(true)).count(),
            1
        );
    }
}
