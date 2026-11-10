//! CPV (attribute-filter) navigator construction — a port of
//! `goofish_client/cpv.py` (round 34).
//!
//! Models the three `clientModifiedCpvNavigatorJson` producers
//! confirmed in the APK (classes11 `com.taobao.idlefish.search_implement`):
//!
//! 1. **FilterBar panel route** (`SearchResultModel.
//!    updateClientModifiedCpvNavigatorJson`): the checked `PvTerm`s of
//!    the panel's `FlexFilter.Element` list are collected via
//!    `ElementData.copyChecked()` and put as `tabList` into a base map
//!    taken from the last response's
//!    `resultInfo.sqiControlFields.cpvNavigatorDo` (when the response
//!    carries none, the previously retained map — initially empty —
//!    is used). `fromClient` is the **String** `"true"` on this route.
//! 2. **SelectCpvTool one-shot route**: a string saved by the Kun/H5
//!    side and consumed exactly once by the refresh funnel; the
//!    consumed value overwrites the request bean's field (which then
//!    persists). `parse_select_cpv` builds the same shape from a
//!    router string (`pid,pname,vid,vname;...`). On this route
//!    `fromClient` is **Boolean** `true` — the type difference
//!    between routes is an app fact, reproduced here, not normalised.
//! 3. **Kun quick-filter route**: the caller-supplied string is
//!    assigned to the bean unconditionally (null clears it), then the
//!    funnel runs, so a pending saved selection (route 2) still wins
//!    for that send. (Modelled in `search_session`, not here.)
//!
//! The functions here are pure: they never mutate their inputs.
//! Serialisation follows the fastjson conventions: null fields are
//! omitted, primitive bean fields (`int` / `boolean`) are always
//! emitted, and fields declared `@JSONField(serialize = false)`
//! (`ElementData.exContent`, `PvTerm.selecting`) never appear. The
//! measured wire stringifies scalars (`"checked":"false"`,
//! `"cardType":"0"`); the bean parse coerces them back, which is what
//! the `wire_*` helpers reproduce.

use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// fastjson's coercion into a primitive boolean bean field. Accepts
/// real booleans, the stringified wire forms (`"true"` / `"false"`,
/// case-insensitive as in `Boolean.parseBoolean`) and numbers;
/// anything else (including null) is false. A plain truthiness check
/// would read the measured string `"false"` as true — the round-31
/// bug class this helper exists to avoid.
pub fn wire_bool(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s.eq_ignore_ascii_case("true"),
        Some(Value::Number(n)) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        _ => false,
    }
}

/// fastjson's coercion into a primitive int bean field (0 default).
pub fn wire_int(value: Option<&Value>) -> i64 {
    match value {
        Some(Value::Bool(b)) => *b as i64,
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(s)) => s.parse::<i64>().unwrap_or(0),
        _ => 0,
    }
}

/// fastjson's coercion into a String bean field (null stays null).
pub fn wire_str(value: Option<&Value>) -> Option<String> {
    match value {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) => Some(other.to_string()),
    }
}

fn insert_wire_str(out: &mut Map<String, Value>, raw: &Value, key: &str) {
    if let Some(v) = wire_str(raw.get(key)) {
        out.insert(key.to_string(), Value::String(v));
    }
}

/// `SearchResultResp.TrackParams {arg1, args, page}` (Strings / Map).
fn parse_track_params(raw: Option<&Value>) -> Option<Value> {
    let raw = raw.filter(|v| v.is_object())?;
    let mut out = Map::new();
    insert_wire_str(&mut out, raw, "arg1");
    insert_wire_str(&mut out, raw, "page");
    if let Some(args) = raw.get("args").filter(|a| a.is_object()) {
        let mut args_out = Map::new();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                if let Some(s) = wire_str(Some(v)) {
                    args_out.insert(k.clone(), Value::String(s));
                }
            }
        }
        out.insert("args".into(), Value::Object(args_out));
    }
    Some(Value::Object(out))
}

/// `FlexFilter.Value` bean (fastjson form, nulls omitted).
fn parse_value(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let mut out = Map::new();
    for key in [
        "displayShortName",
        "icon",
        "prefixIcon",
        "propId",
        "rightIconUrl",
        "valueId",
        "valueName",
    ] {
        insert_wire_str(&mut out, raw, key);
    }
    // Primitive booleans: always emitted.
    out.insert("loading".into(), json!(wire_bool(raw.get("loading"))));
    out.insert("selected".into(), json!(wire_bool(raw.get("selected"))));
    if let Some(track) = parse_track_params(raw.get("trackDO")) {
        out.insert("trackDO".into(), track);
    }
    if let Some(pic) = raw.get("valuePic").filter(|p| p.is_object()) {
        let mut pic_out = Map::new();
        for key in ["picHeight", "picUrl", "picWidth"] {
            insert_wire_str(&mut pic_out, pic, key);
        }
        out.insert("valuePic".into(), Value::Object(pic_out));
    }
    Some(Value::Object(out))
}

/// `FlexFilter.PvTerm` bean (fastjson form, nulls omitted). Unknown
/// raw keys (the measured terms carry e.g. `order`) are dropped,
/// exactly as the bean parse drops them; `selecting` is
/// `@JSONField(serialize = false)` and never appears. `request` keeps
/// only the bean's three String fields (idleCateId / pid / vid) — the
/// measured `tbCateId` / `keyword` keys are dropped. `trackParams` is
/// a JSONObject on the bean and passes through.
fn parse_pv_term(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let mut out = Map::new();
    // Primitive boolean: always emitted.
    out.insert("checked".into(), json!(wire_bool(raw.get("checked"))));
    for key in ["pid", "pname", "vid", "vname"] {
        insert_wire_str(&mut out, raw, key);
    }
    if let Some(children) = raw.get("pvTermList").and_then(|c| c.as_array()) {
        let parsed: Vec<Value> = children.iter().filter_map(parse_pv_term).collect();
        out.insert("pvTermList".into(), Value::Array(parsed));
    }
    if let Some(request) = raw.get("request").filter(|r| r.is_object()) {
        let mut req_out = Map::new();
        for key in ["idleCateId", "pid", "vid"] {
            insert_wire_str(&mut req_out, request, key);
        }
        out.insert("request".into(), Value::Object(req_out));
    }
    if let Some(track) = raw.get("trackParams").filter(|t| t.is_object()) {
        out.insert("trackParams".into(), track.clone());
    }
    Some(Value::Object(out))
}

/// `FlexFilter.ElementData` bean (fastjson form, nulls omitted).
/// `exContent` is `@JSONField(serialize = false)` and never appears.
/// The primitive ints (cardType / initIndex / order) are always
/// emitted.
fn parse_element_data(raw: &Value) -> Option<Value> {
    if !raw.is_object() {
        return None;
    }
    let mut out = Map::new();
    out.insert("cardType".into(), json!(wire_int(raw.get("cardType"))));
    out.insert("initIndex".into(), json!(wire_int(raw.get("initIndex"))));
    out.insert("order".into(), json!(wire_int(raw.get("order"))));
    for key in [
        "mtop",
        "pid",
        "pname",
        "prefixIcon",
        "selectedDropDownIcon",
        "selectedDropUpIcon",
        "selectedIcon",
        "showPlace",
        "unSelectedIcon",
        "unSelectedName",
        "version",
    ] {
        insert_wire_str(&mut out, raw, key);
    }
    if let Some(terms) = raw.get("pvTermList").and_then(|t| t.as_array()) {
        let parsed: Vec<Value> = terms.iter().filter_map(parse_pv_term).collect();
        out.insert("pvTermList".into(), Value::Array(parsed));
    }
    if let Some(values) = raw.get("valueList").and_then(|v| v.as_array()) {
        let parsed: Vec<Value> = values.iter().filter_map(parse_value).collect();
        out.insert("valueList".into(), Value::Array(parsed));
    }
    if let Some(track_do) = parse_track_params(raw.get("trackDO")) {
        out.insert("trackDO".into(), track_do);
    }
    if let Some(track) = raw.get("trackParams").filter(|t| t.is_object()) {
        out.insert("trackParams".into(), track.clone());
    }
    Some(Value::Object(out))
}

/// `ElementData.copyChecked()` over raw (wire-form) element data.
///
/// The app deep-copies the ElementData through a JSON round trip into
/// the bean, then keeps only the checked top-level PvTerms (nested
/// child lists are *not* filtered). `checked_keys`, when given, is
/// the caller's view state — a set of `(pid, vid)` pairs that
/// overrides the top-level terms' checked flags before filtering
/// (the panel's in-memory state in the app). Returns the
/// fastjson-shaped value, or None when `raw_data` is not an object.
pub fn copy_checked_element_data(
    raw_data: &Value,
    checked_keys: Option<&HashSet<(Option<String>, Option<String>)>>,
) -> Option<Value> {
    let mut parsed = parse_element_data(raw_data)?;
    if let Some(terms) = parsed.get_mut("pvTermList").and_then(|t| t.as_array_mut()) {
        if let Some(keys) = checked_keys {
            for term in terms.iter_mut() {
                let key = (wire_str(term.get("pid")), wire_str(term.get("vid")));
                term["checked"] = json!(keys.contains(&key));
            }
        }
        terms.retain(|t| wire_bool(t.get("checked")));
    }
    Some(parsed)
}

// ---------------------------------------------------------------------------
// Route 1: panel construction
// ---------------------------------------------------------------------------

/// Route 1's `JSON.toJSONString(base)` after the two puts.
///
/// `base_map` is the retained base (the last response's
/// cpvNavigatorDo inner map, or the previously retained / initially
/// empty map); it is deep-copied, never mutated. `fromClient` is the
/// String `"true"` on this route (`String.valueOf(true)`).
pub fn build_panel_navigator_json(
    base_map: Option<&Map<String, Value>>,
    checked_elements: &[Value],
) -> String {
    let mut out: Map<String, Value> = base_map.cloned().unwrap_or_default();
    out.insert("tabList".into(), Value::Array(checked_elements.to_vec()));
    out.insert("fromClient".into(), json!("true"));
    Value::Object(out).to_string()
}

// ---------------------------------------------------------------------------
// Route 2: SelectCpvTool generation (router string / Kun array forms)
// ---------------------------------------------------------------------------

/// One tabList entry as parseSelectCpv / gotoMainTab build it: a
/// checked parent PvTerm whose single child carries the value.
fn select_term(
    pid: Option<&str>,
    pname: Option<&str>,
    vid: Option<&str>,
    vname: Option<&str>,
) -> Value {
    let mut parent = Map::new();
    parent.insert("checked".into(), json!(true));
    let mut child = Map::new();
    child.insert("checked".into(), json!(true));
    if let Some(pid) = pid {
        parent.insert("pid".into(), json!(pid));
        child.insert("pid".into(), json!(pid));
    }
    if let Some(pname) = pname {
        parent.insert("pname".into(), json!(pname));
        child.insert("pname".into(), json!(pname));
    }
    if let Some(vid) = vid {
        child.insert("vid".into(), json!(vid));
    }
    if let Some(vname) = vname {
        child.insert("vname".into(), json!(vname));
    }
    parent.insert(
        "pvTermList".into(),
        Value::Array(vec![Value::Object(child)]),
    );
    Value::Object(parent)
}

fn select_json(terms: Vec<Value>) -> String {
    // fromClient is Boolean TRUE on this route (SelectCpvTool.java:45)
    // — deliberately a different type from route 1's String "true".
    json!({"tabList": terms, "fromClient": true}).to_string()
}

/// `SelectCpvTool.parseSelectCpv`: router-string form.
///
/// `spec` is `pid,pname,vid,vname` groups joined by `;` (missing
/// trailing fields become null and are omitted). Returns None for an
/// empty spec — the only caller (`SearchResultReq.injectRouterParams`)
/// guards with `TextUtils.isEmpty` first, and the Java guard inside
/// parseSelectCpv itself is a dead condition, so no defined Java
/// behaviour exists for the empty string.
pub fn parse_select_cpv(spec: &str) -> Option<String> {
    if spec.is_empty() {
        return None;
    }
    let terms: Vec<Value> = spec
        .split(';')
        .map(|group| {
            let fields: Vec<&str> = group.split(',').collect();
            select_term(
                fields.first().copied(),
                fields.get(1).copied(),
                fields.get(2).copied(),
                fields.get(3).copied(),
            )
        })
        .collect();
    Some(select_json(terms))
}

/// The `WVFishSearchApiPlugin.gotoMainTab` form: an iterable of
/// `{pid, pname, vid, vname}` objects -> the same saved-JSON shape as
/// [`parse_select_cpv`] (Boolean `fromClient`).
pub fn build_cpv_json_from_selections(selections: &[Value]) -> String {
    let terms: Vec<Value> = selections
        .iter()
        .map(|item| {
            let get = |k: &str| wire_str(item.get(k));
            select_term(
                get("pid").as_deref(),
                get("pname").as_deref(),
                get("vid").as_deref(),
                get("vname").as_deref(),
            )
        })
        .collect();
    select_json(terms)
}

/// `SelectCpvTool`'s static slot, scoped to one owner.
///
/// The app keeps the saved JSON in a process-wide static; this port
/// scopes the slot to a `SearchSession` instead, so two sessions can
/// never consume each other's saved selection (the one-shot semantics
/// are unchanged: [`SavedCpvSelection::take`] returns the value and
/// clears the slot, exactly like
/// `getClientModifiedCpvNavigatorJson`).
#[derive(Debug, Clone, Default)]
pub struct SavedCpvSelection {
    value: Option<String>,
}

impl SavedCpvSelection {
    pub fn new() -> Self {
        Self::default()
    }

    /// `saveClientModifiedCpvNavigatorJson` (verbatim store).
    pub fn save(&mut self, value: Option<String>) {
        self.value = value;
    }

    /// `hasSavedSelectCpvList`: `!TextUtils.isEmpty(saved)`.
    pub fn has_saved(&self) -> bool {
        self.value
            .as_deref()
            .map(|v| !v.is_empty())
            .unwrap_or(false)
    }

    /// `getClientModifiedCpvNavigatorJson`: read-and-clear.
    pub fn take(&mut self) -> Option<String> {
        self.value.take()
    }
}

// ---------------------------------------------------------------------------
// Response-side extraction
// ---------------------------------------------------------------------------

/// All `flexFilter.data[].element[]` objects of a search response's
/// raw `data` object, flattened in delivered order (the FilterBar
/// adapter's element list). Non-object entries are skipped, mirroring
/// the bean parse. The returned values clone the caller's structures.
pub fn flex_filter_elements(raw_data: Option<&Value>) -> Vec<Value> {
    let mut elements = Vec::new();
    let Some(raw) = raw_data.filter(|v| v.is_object()) else {
        return elements;
    };
    let Some(flex) = raw.get("flexFilter").filter(|v| v.is_object()) else {
        return elements;
    };
    let Some(groups) = flex.get("data").and_then(|d| d.as_array()) else {
        return elements;
    };
    for group in groups {
        if !group.is_object() {
            continue;
        }
        if let Some(list) = group.get("element").and_then(|e| e.as_array()) {
            for element in list {
                if element.is_object() {
                    elements.push(element.clone());
                }
            }
        }
    }
    elements
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from goofish-client/tests/test_round34_cpv.py (unit part).
    #[test]
    fn parse_select_cpv_structure_and_types() {
        let out: Value =
            serde_json::from_str(&parse_select_cpv("20000,品牌,81155,佳能/Canon").unwrap())
                .unwrap();
        // Boolean true on this route — not the String "true".
        assert_eq!(out["fromClient"], json!(true));
        assert!(out["fromClient"].is_boolean());
        let term = &out["tabList"][0];
        assert_eq!(term["checked"], json!(true));
        assert_eq!(term["pid"], json!("20000"));
        assert_eq!(term["pname"], json!("品牌"));
        assert_eq!(term["pvTermList"][0]["vid"], json!("81155"));
        assert_eq!(term["pvTermList"][0]["vname"], json!("佳能/Canon"));
        assert!(parse_select_cpv("").is_none());
    }

    #[test]
    fn build_cpv_json_from_selections_matches_router_form() {
        let from_maps = build_cpv_json_from_selections(&[json!({
            "pid": "20000", "pname": "品牌", "vid": "81155", "vname": "佳能/Canon"
        })]);
        assert_eq!(
            from_maps,
            parse_select_cpv("20000,品牌,81155,佳能/Canon").unwrap()
        );
    }

    #[test]
    fn saved_cpv_selection_one_shot_semantics() {
        let mut slot = SavedCpvSelection::new();
        assert!(!slot.has_saved());
        slot.save(Some("{\"tabList\":[]}".to_string()));
        assert!(slot.has_saved());
        assert_eq!(slot.take().as_deref(), Some("{\"tabList\":[]}"));
        assert!(!slot.has_saved());
        assert_eq!(slot.take(), None);
        // An empty string does not count as saved (TextUtils.isEmpty).
        slot.save(Some(String::new()));
        assert!(!slot.has_saved());
    }

    #[test]
    fn copy_checked_filters_and_bean_shapes() {
        // Measured wire form: scalars stringified, unknown keys present.
        let data = json!({
            "cardType": "0", "order": "3", "pid": "20000", "pname": "品牌",
            "exContent": {"secret": 1},
            "pvTermList": [
                {"checked": "true", "pid": "20000", "vid": "1", "vname": "a",
                 "order": 9, "selecting": true,
                 "request": {"pid": "20000", "vid": "1", "tbCateId": "x"}},
                {"checked": "false", "pid": "20000", "vid": "2", "vname": "b"},
            ],
        });
        let copied = copy_checked_element_data(&data, None).unwrap();
        // Primitive ints re-emitted as numbers; serialize=false dropped.
        assert_eq!(copied["cardType"], json!(0));
        assert_eq!(copied["order"], json!(3));
        assert!(copied.get("exContent").is_none());
        let terms = copied["pvTermList"].as_array().unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0]["vid"], json!("1"));
        assert_eq!(terms[0]["checked"], json!(true));
        assert!(terms[0].get("order").is_none());
        assert!(terms[0].get("selecting").is_none());
        // request keeps only the bean's three String fields.
        assert_eq!(terms[0]["request"], json!({"pid": "20000", "vid": "1"}));
        // Caller view state overrides the wire checked flags.
        let mut keys = HashSet::new();
        keys.insert((Some("20000".to_string()), Some("2".to_string())));
        let copied = copy_checked_element_data(&data, Some(&keys)).unwrap();
        let terms = copied["pvTermList"].as_array().unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0]["vid"], json!("2"));
    }

    #[test]
    fn panel_navigator_json_uses_string_from_client_and_base() {
        let mut base = Map::new();
        base.insert("serverKey".into(), json!("serverValue"));
        let out: Value = serde_json::from_str(&build_panel_navigator_json(
            Some(&base),
            &[json!({"pid": "20000"})],
        ))
        .unwrap();
        assert_eq!(out["fromClient"], json!("true"));
        assert!(out["fromClient"].is_string());
        assert_eq!(out["serverKey"], json!("serverValue"));
        assert_eq!(out["tabList"], json!([{"pid": "20000"}]));
        // The base map is not mutated.
        assert!(base.get("tabList").is_none());
    }

    #[test]
    fn wire_bool_reads_stringified_wire_forms() {
        assert!(wire_bool(Some(&json!(true))));
        assert!(wire_bool(Some(&json!("true"))));
        assert!(wire_bool(Some(&json!("TRUE"))));
        assert!(!wire_bool(Some(&json!("false"))));
        assert!(!wire_bool(Some(&json!(0))));
        assert!(wire_bool(Some(&json!(2))));
        assert!(!wire_bool(None));
        assert!(!wire_bool(Some(&Value::Null)));
    }
}
