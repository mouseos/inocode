//! A stateful main-search session reproducing the app's request
//! bean — a port of `goofish_client/search_session.py` (rounds
//! 30/34/35), consuming the normalized values produced by
//! `parse_search_response` (`items`, `relate_items`, `has_next_page`,
//! `result_info`, `raw_data`) where the Python session duck-types the
//! parser models (`items`, `result_info_raw`, `raw_data`,
//! `has_next_page`).
//!
//! The one-shot builders in [`crate::requests`] reproduce a single
//! SearchResultReq *shape* (an initial, router-shaped search). The
//! app itself never rebuilds the bean: one SearchResultReq lives for
//! the whole SearchResultActivity and every UI operation mutates it
//! and re-sends through the shared refresh funnel
//! (`SearchResultPresenter.refreshFeeds` -> `SearchResultModel.refresh`
//! -> `requestMtop`), with response callbacks rewriting parts of the
//! bean for the next send.
//!
//! [`SearchSession`] models that bean for the main search API:
//!
//! - [`SearchSession::initial_plan`] is the router-entry send
//!   (firstRequest=true, fromFilter as routed — false by default).
//! - The condition-change methods ([`SearchSession::apply_sort`],
//!   [`SearchSession::release_sort`], [`SearchSession::apply_filters`],
//!   [`SearchSession::set_panel_price_range`]) return the funnel send
//!   for the new conditions: fromFilter=true, fromCombo filled with
//!   "Filter", page/sqi/LastIndex state reset, firstRequest omitted
//!   once the first successful non-empty response has been accepted,
//!   and the server-returned extraFilterValue / forceUseTppRepair
//!   carried over from the last refresh-type response.
//! - [`SearchSession::next_page_plan`] is the loadMore send (paging
//!   fields from [`SearchPagination`]).
//! - [`SearchSession::accept_response`] plays the response callbacks:
//!   refresh-type responses run the updateOtherParams effects (server
//!   extraFilterValue, forceUseTppRepair, one-shot consumption); the
//!   initial (refreshAll) response additionally rebuilds the filter
//!   selections from the returned smartUiFilterPanelDO and applies
//!   the follow-new-arrival sort overwrite.
//!
//! Round 34 state: `singleControl` (the model value starts unset, so
//! the first send omits the key; every successful non-empty
//! refresh-type response replaces it with `resultInfo.singleControl`)
//! and the CPV navigator (see [`crate::cpv`]): the FilterBar panel
//! construction over the last response's cpvNavigatorDo base
//! ([`SearchSession::select_cpv`] / [`SearchSession::deselect_cpv`] /
//! [`SearchSession::apply_cpv_selection`]), the SelectCpvTool saved
//! one-shot ([`SearchSession::save_cpv_selection`], consumed by the
//! next funnel send), the router `selectedCpvList` injection (the
//! `selected_cpv_list` constructor argument), and the Kun quick-filter
//! unconditional assignment
//! ([`SearchSession::apply_kun_quick_filter`]). The saved slot is
//! scoped to the session, not process-global like the app's static.
//!
//! Round 35 state: the FilterBar quick-filter toggles
//! ([`SearchSession::toggle_quick_filter`]) — the status-button
//! elements of the flexFilter group named `filter`; toggling flips
//! the entry in the `quickFilter` value set of the current
//! propValueStr with TransformUtil semantics — and the singleControl
//! display toggle ([`SearchSession::set_single_control`] /
//! [`SearchSession::toggle_single_control`]), which produces **no
//! send**: the flipped value rides on the next send like any other
//! bean state. The app only *offers* that button when the server
//! delivers the element; no measured response has delivered one, so
//! in-app availability is server-gated (a constraint, not modelled
//! as a gate on the setter).
//!
//! Deliberately not modelled (the one-shot builders plus
//! `extra_fields` remain the route for these): runtime location
//! stamping (latitude/longitude/gps/fishAdCode/userPositionJson), the
//! area panel's customGps/customDistance copy, the Kun panel-cache
//! assignment (`smartUiFilterPanelDOCache` is re-derived from
//! responses here), and router-supplied fromCombo values other than
//! the funnel's "Filter".
//!
//! Only successful parsed responses may be passed to
//! `accept_response`.

use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

use crate::cpv::{
    build_cpv_json_from_selections, build_panel_navigator_json, copy_checked_element_data,
    flex_filter_elements, parse_select_cpv, wire_bool, SavedCpvSelection,
};
use crate::requests::{
    build_prop_value_str, build_search_request, parse_prop_value_str, RequestPlan, SearchFilters,
    SearchOptions, SearchPagination,
};

type PropMap = Vec<(String, Vec<String>)>;

fn prop_map_get<'a>(map: &'a PropMap, key: &str) -> Option<&'a Vec<String>> {
    map.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn prop_map_set(map: &mut PropMap, key: &str, values: Vec<String>) {
    if let Some(existing) = map.iter_mut().find(|(k, _)| k == key) {
        existing.1 = values;
    } else {
        map.push((key.to_string(), values));
    }
}

fn prop_map_remove(map: &mut PropMap, key: &str) {
    map.retain(|(k, _)| k != key);
}

/// TransformUtil.transPropValueStr2Map, exactly (port of the Python
/// session helper): an ordered mapping of key -> value list parsed
/// from a propValueStr string. The `priceRange` value is *not* split
/// on commas — it is kept as a single value in place. An unparseable
/// or empty input yields an empty mapping, as the Java side's
/// exception path does.
fn parse_prop_map(text: Option<&str>) -> PropMap {
    let mut result: PropMap = Vec::new();
    let Some(text) = text.filter(|t| !t.is_empty()) else {
        return result;
    };
    let Ok(obj) = serde_json::from_str::<Value>(text) else {
        return result;
    };
    let Some(body) = obj.get("searchFilter").and_then(|v| v.as_str()) else {
        return result;
    };
    if body.is_empty() {
        return result;
    }
    for part in body.split(';') {
        let Some((key, rest)) = part.split_once(':') else {
            continue;
        };
        if key.is_empty() || rest.is_empty() {
            continue;
        }
        if key == "priceRange" {
            prop_map_set(&mut result, key, vec![rest.to_string()]);
        } else {
            prop_map_set(
                &mut result,
                key,
                rest.split(',').map(|s| s.to_string()).collect(),
            );
        }
    }
    result
}

/// TransformUtil.transMap2PropValueStr, exactly: keys whose value set
/// is empty are dropped, and when nothing remains the result is None
/// (the bean field is nulled).
fn serialize_prop_map(map: &PropMap) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for (key, values) in map {
        let kept: Vec<&String> = values.iter().filter(|v| !v.is_empty()).collect();
        if kept.is_empty() {
            continue;
        }
        parts.push(format!(
            "{}:{}",
            key,
            kept.iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(json!({"searchFilter": parts.join(";")}).to_string())
}

/// The FilterBar quick-toggle options of a search response's raw
/// data: the status-button elements (`SortStatusButton` /
/// `FilterStatusButton`) of the flexFilter group named `filter`
/// (when groups carry names, only that group is the FilterBar's; the
/// `sorter` group belongs to the SorterBar). Each option is the
/// element's first delivered value. Returns None when the response
/// carries no flexFilter structure at all (the caller then keeps the
/// previous options).
fn quick_filter_options_from_raw(raw: Option<&Value>) -> Option<Vec<Value>> {
    let raw = raw.filter(|v| v.is_object())?;
    let flex = raw.get("flexFilter").filter(|v| v.is_object())?;
    let groups = flex.get("data").and_then(|d| d.as_array())?;
    let named: Vec<&Value> = groups
        .iter()
        .filter(|g| g.is_object() && g.get("name").map(|n| !n.is_null()).unwrap_or(false))
        .collect();
    let use_groups: Vec<&Value> = if !named.is_empty() {
        named
            .into_iter()
            .filter(|g| g.get("name").and_then(|n| n.as_str()) == Some("filter"))
            .collect()
    } else {
        groups.iter().collect()
    };
    let mut options: Vec<Value> = Vec::new();
    let mut seen: HashSet<Value> = HashSet::new();
    for group in use_groups {
        let Some(elements) = group.get("element").and_then(|e| e.as_array()) else {
            continue;
        };
        for element in elements {
            if !element.is_object() {
                continue;
            }
            if !matches!(
                element.get("filterType").and_then(|t| t.as_str()),
                Some("SortStatusButton") | Some("FilterStatusButton")
            ) {
                continue;
            }
            let Some(data) = element.get("data").filter(|d| d.is_object()) else {
                continue;
            };
            let Some(values) = data.get("valueList").and_then(|v| v.as_array()) else {
                continue;
            };
            let Some(first) = values.first().filter(|f| f.is_object()) else {
                continue;
            };
            let Some(value_id) = first.get("valueId").filter(|v| !v.is_null()) else {
                continue;
            };
            if !seen.insert(value_id.clone()) {
                continue;
            }
            options.push(json!({
                "value_id": value_id,
                "value_name": first.get("valueName").cloned().unwrap_or(Value::Null),
            }));
        }
    }
    Some(options)
}

/// The measured wire form stringifies scalars: `"selected":"true"`.
fn selected(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true))) || value.and_then(|v| v.as_str()) == Some("true")
}

#[derive(Debug, Clone, PartialEq)]
enum PropOverride {
    Unset,
    Value(Option<String>),
}

/// The app-shaped stateful counterpart of `build_search_request`.
#[derive(Debug, Clone)]
pub struct SearchSession {
    keyword: Option<String>,
    filters: SearchFilters,
    pagination: SearchPagination,
    started: bool,
    force_use_tpp_repair: bool,
    funnel_used: bool,
    plan_built: bool,
    has_next: bool,
    generation: u64,
    single_control: Option<bool>,
    cpv_navigator_json: Option<String>,
    cpv_elements: Vec<Value>,
    cpv_checked: HashSet<(Option<String>, Option<String>)>,
    cpv_base_map: Map<String, Value>,
    last_cpv_navigator_do: Option<Map<String, Value>>,
    saved_cpv: SavedCpvSelection,
    prop_value_str_override: PropOverride,
    quick_filter_options: Vec<Value>,
}

impl SearchSession {
    pub fn new(keyword: Option<&str>, filters: Option<SearchFilters>) -> Self {
        Self::with_selected_cpv_list(keyword, filters, None)
    }

    pub fn with_selected_cpv_list(
        keyword: Option<&str>,
        filters: Option<SearchFilters>,
        selected_cpv_list: Option<&str>,
    ) -> Self {
        let mut session = Self {
            keyword: keyword.map(|s| s.to_string()),
            filters: filters.unwrap_or_default(),
            pagination: SearchPagination::new(),
            started: false,
            force_use_tpp_repair: false,
            funnel_used: false,
            plan_built: false,
            has_next: false,
            generation: 0,
            single_control: None,
            cpv_navigator_json: None,
            cpv_elements: Vec::new(),
            cpv_checked: HashSet::new(),
            cpv_base_map: Map::new(),
            last_cpv_navigator_do: None,
            saved_cpv: SavedCpvSelection::new(),
            prop_value_str_override: PropOverride::Unset,
            quick_filter_options: Vec::new(),
        };
        if let Some(spec) = selected_cpv_list.filter(|s| !s.is_empty()) {
            // SearchResultReq.injectRouterParams: a non-empty
            // selectedCpvList router parameter generates the field
            // via SelectCpvTool.parseSelectCpv when it is still null.
            session.cpv_navigator_json = parse_select_cpv(spec);
        }
        session
    }

    // -- read-only views ---------------------------------------------------

    pub fn keyword(&self) -> Option<&str> {
        self.keyword.as_deref()
    }

    pub fn filters(&self) -> SearchFilters {
        self.filters.clone()
    }

    /// True once a successful non-empty refresh-type response has
    /// been accepted (the app's one-shot fields are consumed then).
    pub fn started(&self) -> bool {
        self.started
    }

    /// Condition-change counter; a next-page plan belongs to the
    /// generation of the conditions plan it was derived from.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The D-13 carry-over value: None until the first successful
    /// refresh-type response has been accepted (the key is omitted
    /// while None), then the response's `resultInfo.singleControl`
    /// as a real boolean.
    pub fn single_control(&self) -> Option<bool> {
        self.single_control
    }

    /// The bean's current `clientModifiedCpvNavigatorJson` value
    /// (None while the key is omitted from sends).
    pub fn cpv_navigator_json(&self) -> Option<&str> {
        self.cpv_navigator_json.as_deref()
    }

    fn decorate_plan(&self, mut plan: RequestPlan) -> RequestPlan {
        // Apply the bean state the builders do not own: the round-34
        // singleControl / CPV-navigator carries and a Kun
        // propValueStr assignment in force. `with_fields` removes
        // keys whose value is null, so an explicit None below is an
        // omission.
        let mut overrides = Map::new();
        if let Some(sc) = self.single_control {
            overrides.insert("singleControl".into(), json!(sc));
        }
        if let Some(nav) = &self.cpv_navigator_json {
            overrides.insert("clientModifiedCpvNavigatorJson".into(), json!(nav));
        }
        if !overrides.is_empty() {
            plan = plan.with_fields(&overrides);
        }
        if let PropOverride::Value(v) = &self.prop_value_str_override {
            let mut ov = Map::new();
            ov.insert(
                "propValueStr".into(),
                v.clone().map(Value::String).unwrap_or(Value::Null),
            );
            plan = plan.with_fields(&ov);
        }
        plan
    }

    fn build_plan(&self, from_filter: bool, first_request: Option<bool>) -> Result<RequestPlan> {
        let opts = SearchOptions {
            filters: Some(self.filters.clone()),
            first_request,
            from_filter,
            extra_filter_value: self.pagination.extra_filter_value.clone(),
            ..SearchOptions::default()
        };
        build_search_request(self.keyword.as_deref(), &opts)
    }

    // -- first-page plans ----------------------------------------------------

    /// The router-entry send for the session's conditions.
    pub fn initial_plan(&mut self) -> Result<RequestPlan> {
        let mut plan = self.build_plan(false, Some(true))?;
        let mut ov = Map::new();
        ov.insert("forceUseTppRepair".into(), json!(self.force_use_tpp_repair));
        plan = plan.with_fields(&ov);
        self.plan_built = true;
        self.has_next = false;
        Ok(self.decorate_plan(plan))
    }

    fn condition_change_plan(&mut self) -> Result<RequestPlan> {
        // SelectCpvTool consumption happens in refreshFeeds /
        // refreshFilterFeeds before the send is built: a saved value
        // overwrites the bean's field (and then persists in it).
        if self.saved_cpv.has_saved() {
            self.cpv_navigator_json = self.saved_cpv.take();
        }
        // The funnel resets paging state; reset() preserves the
        // server-returned extraFilterValue (round 30 semantics).
        self.pagination.reset();
        self.generation += 1;
        // When the new conditions carry their own region, the builder
        // derives extraFilterValue from them (the area flow replaces
        // the field); otherwise the preserved server form is carried.
        let has_region = self.filters.province.is_some()
            || self.filters.city.is_some()
            || self.filters.area.is_some();
        let carried = if has_region {
            None
        } else {
            self.pagination.extra_filter_value.clone()
        };
        let opts = SearchOptions {
            filters: Some(self.filters.clone()),
            first_request: if self.started { None } else { Some(true) },
            from_filter: true,
            extra_filter_value: carried,
            ..SearchOptions::default()
        };
        let mut plan = build_search_request(self.keyword.as_deref(), &opts)?;
        let mut ov = Map::new();
        ov.insert("forceUseTppRepair".into(), json!(self.force_use_tpp_repair));
        plan = plan.with_fields(&ov);
        self.funnel_used = true;
        self.plan_built = true;
        self.has_next = false;
        Ok(self.decorate_plan(plan))
    }

    /// Select a server-delivered sort candidate (propId/valueId
    /// taken verbatim from the previous response's flexFilter).
    pub fn apply_sort(&mut self, sort_field: &str, sort_value: &str) -> Result<RequestPlan> {
        self.filters.sort_field = Some(sort_field.to_string());
        self.filters.sort_value = Some(sort_value.to_string());
        self.condition_change_plan()
    }

    /// Return to the default candidate: the app re-selects the first
    /// candidate, whose delivered pair is null/null, so the sort keys
    /// disappear from the send.
    pub fn release_sort(&mut self) -> Result<RequestPlan> {
        self.filters.sort_field = None;
        self.filters.sort_value = None;
        self.condition_change_plan()
    }

    /// Replace the whole condition set (quick-filter toggles, CPV
    /// selections, region, category-shaped router fields) and return
    /// the funnel send for it.
    pub fn apply_filters(&mut self, filters: SearchFilters) -> Result<RequestPlan> {
        self.filters = filters;
        // A new condition set supersedes any Kun propValueStr
        // assignment (the field is derived again).
        self.prop_value_str_override = PropOverride::Unset;
        self.condition_change_plan()
    }

    /// Set (or clear, with None) the in-app panel price band. The
    /// band travels as the propValueStr priceRange entry in
    /// "min,max" form; startPrice/endPrice are never written by the
    /// panel flow.
    pub fn set_panel_price_range(
        &mut self,
        price_range_value: Option<&str>,
    ) -> Result<RequestPlan> {
        self.filters.price_range_value = price_range_value.map(|s| s.to_string());
        // The panel flow regenerates propValueStr, superseding any
        // Kun assignment.
        self.prop_value_str_override = PropOverride::Unset;
        self.condition_change_plan()
    }

    // -- loadMore -------------------------------------------------------------

    /// The loadMore send for the current conditions, from the paging
    /// state left by the last accepted response. The base is rebuilt
    /// from the session's current conditions (not from the previously
    /// built plan), so response-driven rewrites — the panel rebuild
    /// and the follow-arrival sort overwrite — are reflected, as they
    /// are in the app's single bean.
    pub fn next_page_plan(&self) -> Result<RequestPlan> {
        if !self.plan_built {
            return Err(anyhow!(
                "no first-page plan yet: call initial_plan() or a condition-change method first"
            ));
        }
        if !self.has_next {
            return Err(anyhow!(
                "the last accepted response reported no next page (or no response has been accepted for the current conditions)"
            ));
        }
        let mut plan = self.build_plan(self.funnel_used, None)?;
        plan = plan.with_fields(&self.pagination.request_fields());
        let mut ov = Map::new();
        ov.insert("forceUseTppRepair".into(), json!(self.force_use_tpp_repair));
        plan = plan.with_fields(&ov);
        Ok(self.decorate_plan(plan))
    }

    // -- response callbacks ---------------------------------------------------

    /// Consume a parsed search response (the normalized Value from
    /// `parse_search_response`); returns true when another page may
    /// be requested for the current conditions.
    ///
    /// `load_more` marks a loadMore response (no updateOtherParams).
    /// `refresh_all` marks the initial send's response (additionally
    /// the panel rebuild and the follow-new-arrival sort overwrite).
    pub fn accept_response(
        &mut self,
        parsed_page: &Value,
        load_more: bool,
        refresh_all: bool,
    ) -> bool {
        let items_len = parsed_page
            .get("items")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        let non_empty = items_len > 0;
        // The model's `response` is replaced by every successful
        // response (loadMore included): the CPV candidate elements
        // and the cpvNavigatorDo base source follow it.
        self.update_cpv_response_state(parsed_page);
        // The FilterBar's element list follows the same replacement.
        self.update_quick_filter_response_state(parsed_page);
        if load_more {
            self.has_next = self.pagination.advance(parsed_page);
            return self.has_next;
        }
        if non_empty {
            // updateOtherParams (successful, non-empty refresh-type
            // responses only): the one-shot fields are consumed and
            // forceUseTppRepair is taken from resultInfo. The
            // extraFilterValue replacement itself happens inside
            // SearchPagination.advance (same source: the response's
            // top-level extraFilterValue).
            let info = parsed_page.get("result_info").filter(|v| v.is_object());
            // D-13 (round 34): the presenter callbacks take
            // singleControl in from resultInfo in the same region;
            // ResultInfo.singleControl is a primitive boolean bean
            // field (false when resultInfo or the key is absent), so
            // the model value is always a real boolean afterwards.
            // The stringified wire form ("false") goes through
            // wire_bool, never through truthiness.
            self.single_control = Some(wire_bool(info.and_then(|i| i.get("singleControl"))));
            if let Some(info) = info {
                if let Some(v) = info.get("forceUseTppRepair").filter(|v| !v.is_null()) {
                    // The measured wire form stringifies scalars
                    // ("forceUseTppRepair": "false"); a plain
                    // truthiness read would read "false" as true.
                    self.force_use_tpp_repair = selected(Some(v));
                }
            }
            self.started = true;
            if refresh_all {
                self.apply_refresh_all_effects(parsed_page);
            }
        }
        self.has_next = self.pagination.advance(parsed_page);
        self.has_next
    }

    fn apply_refresh_all_effects(&mut self, parsed_page: &Value) {
        // The panel rebuild regenerates propValueStr from the
        // returned panel definition, superseding any Kun assignment.
        self.prop_value_str_override = PropOverride::Unset;
        let Some(raw) = parsed_page.get("raw_data").filter(|v| v.is_object()) else {
            return;
        };
        // Follow new-arrival: the presenter overwrites the sort for
        // subsequent sends (no immediate re-send).
        if let Some(follow) = raw.get("userFollowButtonDO").filter(|v| v.is_object()) {
            if let Some(arrivals) = follow.get("newArrivalNums").and_then(|v| v.as_f64()) {
                if arrivals > 0.0 {
                    self.filters.sort_field = Some("create".to_string());
                    self.filters.sort_value = Some("desc".to_string());
                }
            }
        }
        // propValueStr rebuild from the returned panel definition:
        // selected values per card, with the 3085 price card carried
        // as its raw priceRange string. An absent/empty panel list
        // nulls the field.
        let Some(panel) = raw.get("smartUiFilterPanelDO").filter(|v| v.is_object()) else {
            return;
        };
        let Some(cards) = panel.get("filterPanelDOList").and_then(|v| v.as_array()) else {
            self.filters.prop_selections = Vec::new();
            self.filters.price_range_value = None;
            return;
        };
        if cards.is_empty() {
            self.filters.prop_selections = Vec::new();
            self.filters.price_range_value = None;
            return;
        }
        let mut selections: Vec<(String, Vec<String>)> = Vec::new();
        let mut price_range: Option<String> = None;
        for card in cards {
            if !card.is_object() {
                continue;
            }
            let Some(card_data) = card.get("cardData").filter(|v| v.is_object()) else {
                continue;
            };
            if card
                .get("cardType")
                .map(|t| t.to_string().trim_matches('"').to_string())
                == Some("3085".to_string())
            {
                price_range = card_data
                    .get("priceRange")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                continue;
            }
            let Some(prop_id) = card_data.get("propId") else {
                continue;
            };
            let prop_id = match prop_id {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let mut values: Vec<String> = Vec::new();
            if let Some(entries) = card_data.get("value").and_then(|v| v.as_array()) {
                for entry in entries {
                    if entry.is_object()
                        && selected(entry.get("selected"))
                        && entry.get("valueId").map(|v| !v.is_null()).unwrap_or(false)
                    {
                        let vid = match entry.get("valueId").unwrap() {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        values.push(vid);
                    }
                }
            }
            if !values.is_empty() {
                selections.push((prop_id, values));
            }
        }
        self.filters.prop_selections = selections;
        self.filters.price_range_value = price_range;
    }

    // -- CPV navigator (round 34) --------------------------------------------

    fn update_cpv_response_state(&mut self, parsed_page: &Value) {
        // Follow the model's `response` replacement: the CPV
        // candidate elements come from the newest response's
        // flexFilter and the panel-construction base source is its
        // resultInfo.sqiControlFields.cpvNavigatorDo (cloned; the
        // parsed response is never mutated). Terms the server itself
        // marks checked join the selection, as the panel would show
        // them checked.
        let info = parsed_page.get("result_info").filter(|v| v.is_object());
        let navigator_do = info
            .and_then(|i| i.get("sqiControlFields"))
            .filter(|v| v.is_object())
            .and_then(|sqi| sqi.get("cpvNavigatorDo"))
            .filter(|v| v.is_object())
            .and_then(|v| v.as_object().cloned());
        self.last_cpv_navigator_do = navigator_do;
        let raw = parsed_page.get("raw_data");
        let elements = flex_filter_elements(raw);
        if !elements.is_empty() {
            self.cpv_elements = elements;
            for element in &self.cpv_elements {
                let Some(data) = element.get("data").filter(|v| v.is_object()) else {
                    continue;
                };
                if let Some(terms) = data.get("pvTermList").and_then(|v| v.as_array()) {
                    for term in terms {
                        if term.is_object() && wire_bool(term.get("checked")) {
                            self.cpv_checked.insert((
                                crate::cpv::wire_str(term.get("pid")),
                                crate::cpv::wire_str(term.get("vid")),
                            ));
                        }
                    }
                }
            }
        }
    }

    /// The server-delivered CPV properties of the last accepted
    /// response: `[{pid, pname, values: [{vid, vname, checked}]}]`
    /// for every flexFilter element carrying a pvTermList. Empty
    /// until a response with CPV data has been accepted.
    pub fn cpv_properties(&self) -> Vec<Value> {
        let mut properties = Vec::new();
        for element in &self.cpv_elements {
            let Some(data) = element.get("data").filter(|v| v.is_object()) else {
                continue;
            };
            let Some(terms) = data.get("pvTermList").and_then(|v| v.as_array()) else {
                continue;
            };
            let mut values = Vec::new();
            for term in terms {
                if !term.is_object() {
                    continue;
                }
                let key = (
                    crate::cpv::wire_str(term.get("pid")),
                    crate::cpv::wire_str(term.get("vid")),
                );
                values.push(json!({
                    "vid": term.get("vid").cloned().unwrap_or(Value::Null),
                    "vname": term.get("vname").cloned().unwrap_or(Value::Null),
                    "checked": self.cpv_checked.contains(&key),
                }));
            }
            properties.push(json!({
                "pid": data.get("pid").cloned().unwrap_or(Value::Null),
                "pname": data.get("pname").cloned().unwrap_or(Value::Null),
                "values": values,
            }));
        }
        properties
    }

    /// The current CPV selection as `(pid, [vid, ...])` pairs, in
    /// candidate (server-delivered) order.
    pub fn cpv_selection(&self) -> Vec<(String, Vec<String>)> {
        let mut selection = Vec::new();
        for prop in self.cpv_properties() {
            let chosen: Vec<String> = prop["values"]
                .as_array()
                .map(|values| {
                    values
                        .iter()
                        .filter(|v| v.get("checked").and_then(|c| c.as_bool()) == Some(true))
                        .filter_map(|v| {
                            v.get("vid").map(|id| match id {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !chosen.is_empty() {
                if let Some(pid) = prop["pid"].as_str() {
                    selection.push((pid.to_string(), chosen));
                }
            }
        }
        selection
    }

    fn cpv_term_exists(&self, pid: &str, vid: &str) -> bool {
        self.cpv_elements.iter().any(|element| {
            let Some(data) = element.get("data").filter(|v| v.is_object()) else {
                return false;
            };
            data.get("pvTermList")
                .and_then(|v| v.as_array())
                .map(|terms| {
                    terms.iter().any(|term| {
                        term.is_object()
                            && crate::cpv::wire_str(term.get("pid")).as_deref() == Some(pid)
                            && crate::cpv::wire_str(term.get("vid")).as_deref() == Some(vid)
                    })
                })
                .unwrap_or(false)
        })
    }

    fn rebuild_cpv_navigator(&mut self) {
        // `updateClientModifiedCpvNavigatorJson` (route 1), verbatim:
        // rebuild the checked copies from the panel state; when any
        // remain, put them as tabList over the base (the last
        // response's cpvNavigatorDo map when it carries one, else the
        // retained map) with the String fromClient; when none remain,
        // clear the field only when the last response carried no
        // cpvNavigatorDo — with a base present the app leaves the
        // previous value in the bean (it stays on the wire).
        let mut checked_elements: Vec<Value> = Vec::new();
        for element in &self.cpv_elements {
            let Some(data) = element.get("data").filter(|v| v.is_object()) else {
                continue;
            };
            if let Some(copied) = copy_checked_element_data(data, Some(&self.cpv_checked)) {
                if copied
                    .get("pvTermList")
                    .and_then(|t| t.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false)
                {
                    checked_elements.push(copied);
                }
            }
        }
        if !checked_elements.is_empty() {
            if let Some(base) = &self.last_cpv_navigator_do {
                self.cpv_base_map = base.clone();
            }
            self.cpv_navigator_json = Some(build_panel_navigator_json(
                Some(&self.cpv_base_map),
                &checked_elements,
            ));
        } else if self.last_cpv_navigator_do.is_none() {
            self.cpv_navigator_json = None;
        }
        // else: base present and nothing checked — the app does not
        // touch the bean field (SearchResultModel.java:288-296).
    }

    /// Check one server-delivered CPV value and return the funnel
    /// send (`onCpvValueChanged` -> `refreshFeeds`). Selecting an
    /// already-selected value is a no-op state change that still
    /// rebuilds and returns the funnel send, as a panel confirm does.
    /// Values the last response did not deliver are refused — the
    /// panel cannot produce them.
    pub fn select_cpv(&mut self, pid: &str, vid: &str) -> Result<RequestPlan> {
        if !self.cpv_term_exists(pid, vid) {
            return Err(anyhow!(
                "CPV value (pid={pid:?}, vid={vid:?}) was not delivered by the last accepted response"
            ));
        }
        self.cpv_checked
            .insert((Some(pid.to_string()), Some(vid.to_string())));
        self.rebuild_cpv_navigator();
        self.condition_change_plan()
    }

    /// Uncheck one CPV value and return the funnel send. The
    /// empty-selection outcome follows route 1 exactly (see
    /// `rebuild_cpv_navigator`).
    pub fn deselect_cpv(&mut self, pid: &str, vid: &str) -> Result<RequestPlan> {
        if !self.cpv_term_exists(pid, vid) {
            return Err(anyhow!(
                "CPV value (pid={pid:?}, vid={vid:?}) was not delivered by the last accepted response"
            ));
        }
        self.cpv_checked
            .remove(&(Some(pid.to_string()), Some(vid.to_string())));
        self.rebuild_cpv_navigator();
        self.condition_change_plan()
    }

    /// Replace the whole CPV selection (one panel confirm with a
    /// complete state) and return the funnel send. Every pair must
    /// have been delivered by the last accepted response.
    pub fn apply_cpv_selection(&mut self, selection: &[(String, String)]) -> Result<RequestPlan> {
        for (pid, vid) in selection {
            if !self.cpv_term_exists(pid, vid) {
                return Err(anyhow!(
                    "CPV value (pid={pid:?}, vid={vid:?}) was not delivered by the last accepted response"
                ));
            }
        }
        self.cpv_checked = selection
            .iter()
            .map(|(p, v)| (Some(p.clone()), Some(v.clone())))
            .collect();
        self.rebuild_cpv_navigator();
        self.condition_change_plan()
    }

    /// Save a SelectCpvTool one-shot value (route 2). Accepts the
    /// saved JSON string verbatim, a router string
    /// (`pid,pname,vid,vname;...`, converted with
    /// [`parse_select_cpv`]), or an iterable of `{pid, pname, vid,
    /// vname}` objects (the gotoMainTab form). The next funnel send
    /// consumes it exactly once — it overwrites the bean's navigator
    /// value for that send and persists in the bean afterwards, like
    /// the app. Router-string input is told apart from saved JSON by
    /// its leading character (`{`).
    pub fn save_cpv_selection_json(&mut self, value: Option<String>) {
        let value = match value {
            Some(s) if !s.is_empty() && !s.trim_start().starts_with('{') => parse_select_cpv(&s),
            other => other,
        };
        self.saved_cpv.save(value);
    }

    /// Save a SelectCpvTool one-shot value from `{pid, pname, vid,
    /// vname}` objects (the gotoMainTab form).
    pub fn save_cpv_selection(&mut self, selections: &[Value]) {
        self.saved_cpv
            .save(Some(build_cpv_json_from_selections(selections)));
    }

    /// True while a saved one-shot value is pending consumption.
    pub fn has_saved_cpv_selection(&self) -> bool {
        self.saved_cpv.has_saved()
    }

    /// The Kun quick-filter confirm (`refreshByQuickFilter` ->
    /// `refreshFeeds`): both arguments are assigned to the bean
    /// unconditionally — None clears the field — and the funnel send
    /// is returned. A pending saved selection still wins for this
    /// send (the funnel's consumption runs after the assignment).
    /// The panel-cache and area-filter branches of the plugin call
    /// are outside this method (see the module docstring).
    pub fn apply_kun_quick_filter(
        &mut self,
        cpv_navigator_json: Option<String>,
        prop_value_str: Option<String>,
    ) -> Result<RequestPlan> {
        self.cpv_navigator_json = cpv_navigator_json;
        self.prop_value_str_override = PropOverride::Value(prop_value_str);
        self.condition_change_plan()
    }

    // -- quick-filter toggles and the singleControl toggle (round 35) --------

    fn update_quick_filter_response_state(&mut self, parsed_page: &Value) {
        // Follow the model's `response` replacement for the
        // FilterBar: the quick-toggle options come from the newest
        // response's flexFilter filter group. A response without a
        // flexFilter structure leaves the previous options in place.
        if let Some(options) = quick_filter_options_from_raw(parsed_page.get("raw_data")) {
            self.quick_filter_options = options;
        }
    }

    fn effective_prop_map(&self) -> Result<PropMap> {
        // The session's current propValueStr as an ordered map — the
        // Kun assignment when one is in force, else the map the
        // filters derive (selections in order, priceRange last, as
        // build_prop_value_str emits it).
        if let PropOverride::Value(v) = &self.prop_value_str_override {
            return Ok(parse_prop_map(v.as_deref()));
        }
        match build_prop_value_str(
            &self.filters.prop_selections,
            self.filters.price_range_value.as_deref(),
        )? {
            Some(text) => {
                let (selections, price_range) = parse_prop_value_str(&text)?;
                let mut map: PropMap = selections;
                if let Some(pr) = price_range {
                    prop_map_set(&mut map, "priceRange", vec![pr]);
                }
                Ok(map)
            }
            None => Ok(Vec::new()),
        }
    }

    /// The quick-filter toggles the last accepted response delivered
    /// in its FilterBar: `[{value_id, value_name, selected}]` in
    /// delivered order. Empty until a response with a flexFilter
    /// filter group has been accepted. (The panel-only quick values
    /// — e.g. `filterArea`, whose state follows the area flow — are
    /// not bar toggles and never appear here.)
    pub fn quick_filter_options(&self) -> Vec<Value> {
        let selected: HashSet<String> = self.quick_filter_selection().into_iter().collect();
        self.quick_filter_options
            .iter()
            .map(|option| {
                let vid = option
                    .get("value_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                json!({
                    "value_id": option.get("value_id").cloned().unwrap_or(Value::Null),
                    "value_name": option.get("value_name").cloned().unwrap_or(Value::Null),
                    "selected": selected.contains(&vid),
                })
            })
            .collect()
    }

    /// The currently selected quick-filter value ids, in
    /// propValueStr (insertion) order — the `quickFilter` value set
    /// of the session's effective propValueStr.
    pub fn quick_filter_selection(&self) -> Vec<String> {
        let map = self.effective_prop_map().unwrap_or_default();
        let mut selection: Vec<String> = Vec::new();
        if let Some(values) = prop_map_get(&map, "quickFilter") {
            for value in values {
                if !selection.contains(value) {
                    selection.push(value.clone());
                }
            }
        }
        selection
    }

    /// Flip one delivered quick-filter toggle and return the funnel
    /// send (the FilterBar status-button click: `onPropValueChanged`
    /// -> `setPropValueStr` -> `refreshFeeds`).
    ///
    /// The toggle edits the `quickFilter` value set of the session's
    /// current propValueStr with TransformUtil semantics: selecting
    /// appends the value id to the set (creating the key at the end
    /// of the map when absent), deselecting removes it, an emptied
    /// set drops the key, and an all-empty map nulls propValueStr.
    /// When a Kun propValueStr assignment is in force the edit
    /// applies to that string and stays in force as the edited
    /// assignment; otherwise the filters' selections are updated.
    /// Values the last response did not deliver as bar toggles are
    /// refused — the bar cannot produce them.
    pub fn toggle_quick_filter(&mut self, value_id: &str) -> Result<RequestPlan> {
        let delivered = self
            .quick_filter_options
            .iter()
            .any(|o| o.get("value_id").and_then(|v| v.as_str()) == Some(value_id));
        if !delivered {
            return Err(anyhow!(
                "quick-filter value {value_id:?} was not delivered as a FilterBar toggle by the last accepted response"
            ));
        }
        if let PropOverride::Value(current) = &self.prop_value_str_override {
            let mut map = parse_prop_map(current.as_deref());
            let mut values = prop_map_get(&map, "quickFilter")
                .cloned()
                .unwrap_or_default();
            if values.iter().any(|v| v == value_id) {
                values.retain(|v| v != value_id);
            } else {
                values.push(value_id.to_string());
            }
            if values.is_empty() {
                prop_map_remove(&mut map, "quickFilter");
            } else {
                prop_map_set(&mut map, "quickFilter", values);
            }
            self.prop_value_str_override = PropOverride::Value(serialize_prop_map(&map));
        } else {
            let mut selections = self.filters.prop_selections.clone();
            let mut values = prop_map_get(&selections, "quickFilter")
                .cloned()
                .unwrap_or_default();
            if values.iter().any(|v| v == value_id) {
                values.retain(|v| v != value_id);
            } else {
                values.push(value_id.to_string());
            }
            if values.is_empty() {
                prop_map_remove(&mut selections, "quickFilter");
            } else {
                prop_map_set(&mut selections, "quickFilter", values);
            }
            self.filters.prop_selections = selections;
        }
        self.condition_change_plan()
    }

    /// Set the singleControl display value directly (the sorter bar
    /// toggle's outcome state). No send is produced — in the app the
    /// toggle only re-renders the current feed; the value rides on
    /// the next plan this session builds, and the next successful
    /// refresh-type response overwrites it from resultInfo again.
    /// Returns the new value.
    pub fn set_single_control(&mut self, value: bool) -> bool {
        self.single_control = Some(value);
        value
    }

    /// Flip the singleControl display value, as the
    /// `Client_SingleControlButton` click does
    /// (`!Boolean.parseBoolean(String.valueOf(current))` — a
    /// not-yet-set value reads as false, so the first toggle turns
    /// it on). No send is produced; see `set_single_control`.
    /// Returns the new value.
    pub fn toggle_single_control(&mut self) -> bool {
        let current = self.single_control.unwrap_or(false);
        self.single_control = Some(!current);
        !current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic parsed pages in the exact shape parse_search_response
    // emits. Shapes/values mirror the measured round-20/32 payloads
    // (server extraFilterValue form, delivered quick toggles, CPV
    // brand property 20000 / 81155) — the same values the Python
    // round-30/34/35 tests pin.
    const SERVER_EFV: &str = "{\"hideExcludeMultiPlacesSellers\":\"true\",\"divisionList\":[{}]}";

    fn flex_filter() -> Value {
        json!({
            "data": [
                {"name": "sorter", "element": [
                    {"filterType": "SortStatusButton", "data": {"valueList": [
                        {"valueId": "sorterOnly", "valueName": "排序"}]}},
                ]},
                {"name": "filter", "element": [
                    {"filterType": "SortStatusButton", "data": {"valueList": [
                        {"valueId": "filterPersonal", "valueName": "个人闲置"}]}},
                    {"filterType": "FilterStatusButton", "data": {"valueList": [
                        {"valueId": "filterUserShopStatus", "valueName": "鱼小铺"}]}},
                    {"filterType": "CpvNavFilter", "data": {
                        "cardType": "0", "order": "1", "pid": "20000", "pname": "品牌",
                        "pvTermList": [
                            {"checked": "false", "pid": "20000", "pname": "品牌",
                             "vid": "21459", "vname": "卡西欧/Casio", "pvTermList": []},
                            {"checked": "false", "pid": "20000", "pname": "品牌",
                             "vid": "81155", "vname": "佳能/Canon", "pvTermList": []},
                        ],
                    }},
                ]},
            ]
        })
    }

    fn parsed_page(items: usize, has_next: bool) -> Value {
        let items: Vec<Value> = (0..items)
            .map(|i| json!({"item_id": format!("{}", 100 + i)}))
            .collect();
        json!({
            "items": items,
            "relate_items": [],
            "has_next_page": has_next,
            "result_info": {
                "hasNextPage": has_next,
                "singleControl": "false",
                "forceUseTppRepair": "false",
                "sqiControlFields": {"someKey": "someValue"},
            },
            "raw_data": {
                "extraFilterValue": SERVER_EFV,
                "flexFilter": flex_filter(),
                "smartUiFilterPanelDO": {"filterPanelDOList": []},
            },
        })
    }

    fn started_session() -> SearchSession {
        let mut session = SearchSession::new(Some("相机"), None);
        session.initial_plan().unwrap();
        assert!(session.accept_response(&parsed_page(10, true), false, true));
        assert!(session.started());
        session
    }

    #[test]
    fn session_initial_plan_is_router_shaped() {
        let mut session = SearchSession::new(Some("相机"), None);
        let plan = session.initial_plan().unwrap();
        assert_eq!(plan.data["firstRequest"], json!(true));
        assert_eq!(plan.data["fromFilter"], json!(false));
        assert!(plan.data.get("fromCombo").is_none());
        assert!(plan.data.get("singleControl").is_none());
        assert!(plan.data.get("clientModifiedCpvNavigatorJson").is_none());
        assert_eq!(plan.data["forceUseTppRepair"], json!(false));
    }

    #[test]
    fn session_sort_after_first_response_is_app_shaped() {
        // Python round-30 scenario A.
        let mut session = started_session();
        let plan = session.apply_sort("create", "desc").unwrap();
        let d = &plan.data;
        assert_eq!(d["sortField"], json!("create"));
        assert_eq!(d["sortValue"], json!("desc"));
        // The funnel accompaniments the one-shot builds lacked:
        assert_eq!(d["fromFilter"], json!(true));
        assert_eq!(d["fromCombo"], json!("Filter"));
        assert!(d.get("firstRequest").is_none()); // consumed
        assert!(d.get("sqiControlFieldsJson").is_none()); // reset
        assert_eq!(d["pageNumber"], json!(1));
        assert_eq!(d["resultListLastIndex"], json!(0));
        // The server-returned extraFilterValue form is carried.
        let efv: Value = serde_json::from_str(d["extraFilterValue"].as_str().unwrap()).unwrap();
        assert_eq!(
            efv,
            json!({"hideExcludeMultiPlacesSellers": "true", "divisionList": [{}]})
        );
        // D-13: singleControl arrived (stringified "false") and rides
        // on the funnel send as a real boolean.
        assert_eq!(d["singleControl"], json!(false));
    }

    #[test]
    fn session_next_page_carries_paging_state() {
        let session = started_session();
        let plan = session.next_page_plan().unwrap();
        assert_eq!(plan.data["pageNumber"], json!(2));
        assert_eq!(plan.data["resultListLastIndex"], json!(10));
        assert!(plan.data.get("firstRequest").is_none());
        assert_eq!(
            plan.data["sqiControlFieldsJson"],
            json!("{\"someKey\":\"someValue\"}")
        );
        // A fresh session cannot page.
        let fresh = SearchSession::new(Some("x"), None);
        assert!(fresh.next_page_plan().is_err());
    }

    #[test]
    fn session_panel_rebuild_nulls_unselected_prop_value_str() {
        // Python round-30: the measured panel has no selected values,
        // so after refreshAll the propValueStr state is empty even if
        // the caller's initial filters had selections.
        let filters = SearchFilters {
            prop_selections: vec![("quickFilter".into(), vec!["filterPersonal".into()])],
            ..Default::default()
        };
        let mut session = SearchSession::new(Some("相机"), Some(filters));
        session.initial_plan().unwrap();
        session.accept_response(&parsed_page(10, true), false, true);
        let plan = session.apply_sort("modify", "desc").unwrap();
        assert!(plan.data.get("propValueStr").is_none());
    }

    #[test]
    fn session_release_sort_drops_sort_keys() {
        let mut session = started_session();
        session.apply_sort("create", "desc").unwrap();
        let plan = session.release_sort().unwrap();
        assert!(plan.data.get("sortField").is_none());
        assert!(plan.data.get("sortValue").is_none());
        assert_eq!(plan.data["fromFilter"], json!(true));
    }

    #[test]
    fn session_cpv_select_builds_panel_navigator_json() {
        // Python round-34 scenario B: no cpvNavigatorDo base in the
        // response, so the base is the retained (empty) map and
        // fromClient is the String "true" (route 1).
        let mut session = started_session();
        assert!(session.cpv_navigator_json().is_none());
        let props = session.cpv_properties();
        assert_eq!(props.len(), 1);
        assert_eq!(props[0]["pid"], json!("20000"));
        let plan = session.select_cpv("20000", "81155").unwrap();
        let nav: Value = serde_json::from_str(
            plan.data["clientModifiedCpvNavigatorJson"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(nav["fromClient"], json!("true"));
        assert!(nav["fromClient"].is_string());
        let element = &nav["tabList"][0];
        assert_eq!(element["pid"], json!("20000"));
        let terms = element["pvTermList"].as_array().unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0]["vid"], json!("81155"));
        assert_eq!(terms[0]["checked"], json!(true));
        assert_eq!(
            session.cpv_selection(),
            vec![("20000".to_string(), vec!["81155".to_string()])]
        );
        // Unknown values are refused without a send.
        assert!(session.select_cpv("20000", "99999").is_err());
    }

    #[test]
    fn session_cpv_deselect_last_value_clears_field_without_base() {
        // Python round-34 scenario C (no-base branch).
        let mut session = started_session();
        session.select_cpv("20000", "81155").unwrap();
        let plan = session.deselect_cpv("20000", "81155").unwrap();
        assert!(plan.data.get("clientModifiedCpvNavigatorJson").is_none());
        assert!(session.cpv_navigator_json().is_none());
    }

    #[test]
    fn session_cpv_deselect_all_with_base_keeps_previous_value() {
        // Python round-34 scenario C (base branch, synthetic
        // cpvNavigatorDo — no measured payload carries one).
        let mut page = parsed_page(5, false);
        page["result_info"]["sqiControlFields"] =
            json!({"cpvNavigatorDo": {"serverKey": "serverValue"}});
        let mut session = SearchSession::new(Some("x"), None);
        session.initial_plan().unwrap();
        session.accept_response(&page, false, true);
        let plan = session.select_cpv("20000", "81155").unwrap();
        let nav: Value = serde_json::from_str(
            plan.data["clientModifiedCpvNavigatorJson"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(nav["serverKey"], json!("serverValue"));
        assert_eq!(nav["fromClient"], json!("true"));
        let kept = session.cpv_navigator_json().unwrap().to_string();
        let plan = session.deselect_cpv("20000", "81155").unwrap();
        assert_eq!(plan.data["clientModifiedCpvNavigatorJson"], json!(kept));
    }

    #[test]
    fn session_saved_cpv_consumed_once_by_funnel_only() {
        // Python round-34 scenario F.
        let mut session = started_session();
        session.save_cpv_selection_json(Some("20000,品牌,81155,佳能/Canon".to_string()));
        assert!(session.has_saved_cpv_selection());
        // next_page_plan is not a funnel send: the saved value is not
        // consumed (and not carried) there.
        let page_plan = session.next_page_plan().unwrap();
        assert!(page_plan
            .data
            .get("clientModifiedCpvNavigatorJson")
            .is_none());
        assert!(session.has_saved_cpv_selection());
        // The funnel send consumes it exactly once; it then persists
        // in the bean.
        let plan = session.apply_sort("create", "desc").unwrap();
        assert!(!session.has_saved_cpv_selection());
        let nav: Value = serde_json::from_str(
            plan.data["clientModifiedCpvNavigatorJson"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(nav["fromClient"], json!(true)); // Boolean, route 2
        assert_eq!(nav["tabList"][0]["pvTermList"][0]["vid"], json!("81155"));
        let plan2 = session.release_sort().unwrap();
        assert_eq!(
            plan2.data["clientModifiedCpvNavigatorJson"],
            plan.data["clientModifiedCpvNavigatorJson"]
        );
    }

    #[test]
    fn session_router_selected_cpv_list_injection() {
        let session = SearchSession::with_selected_cpv_list(
            Some("x"),
            None,
            Some("20000,品牌,81155,佳能/Canon"),
        );
        let nav: Value = serde_json::from_str(session.cpv_navigator_json().unwrap()).unwrap();
        assert_eq!(nav["fromClient"], json!(true));
        assert_eq!(nav["tabList"][0]["pvTermList"][0]["vid"], json!("81155"));
    }

    #[test]
    fn session_kun_assignment_and_saved_wins() {
        // Python round-34 scenario G.
        let mut session = started_session();
        let plan = session
            .apply_kun_quick_filter(
                Some("{\"tabList\":[]}".to_string()),
                Some("{\"searchFilter\":\"quickFilter:filterPersonal\"}".to_string()),
            )
            .unwrap();
        assert_eq!(
            plan.data["clientModifiedCpvNavigatorJson"],
            json!("{\"tabList\":[]}")
        );
        assert_eq!(
            plan.data["propValueStr"],
            json!("{\"searchFilter\":\"quickFilter:filterPersonal\"}")
        );
        // Clearing with None drops both keys.
        let plan = session.apply_kun_quick_filter(None, None).unwrap();
        assert!(plan.data.get("clientModifiedCpvNavigatorJson").is_none());
        assert!(plan.data.get("propValueStr").is_none());
        // A pending saved selection wins over the Kun assignment.
        session.save_cpv_selection_json(Some("20000,品牌,81155,佳能/Canon".to_string()));
        let plan = session
            .apply_kun_quick_filter(Some("{\"tabList\":[]}".to_string()), None)
            .unwrap();
        let nav: Value = serde_json::from_str(
            plan.data["clientModifiedCpvNavigatorJson"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(nav["fromClient"], json!(true));
    }

    #[test]
    fn session_quick_filter_toggle_flow() {
        // Python round-35: options are the delivered filter-group
        // toggles only (the sorter group's are not offered).
        let mut session = started_session();
        let options = session.quick_filter_options();
        let ids: Vec<&str> = options
            .iter()
            .map(|o| o["value_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["filterPersonal", "filterUserShopStatus"]);
        assert!(options.iter().all(|o| o["selected"] == json!(false)));
        assert_eq!(session.quick_filter_selection(), Vec::<String>::new());

        let generation = session.generation();
        let plan = session.toggle_quick_filter("filterPersonal").unwrap();
        assert_eq!(
            plan.data["propValueStr"],
            json!("{\"searchFilter\":\"quickFilter:filterPersonal\"}")
        );
        assert_eq!(plan.data["fromFilter"], json!(true));
        assert_eq!(plan.data["fromCombo"], json!("Filter"));
        assert_eq!(plan.data["pageNumber"], json!(1));
        assert_eq!(session.generation(), generation + 1);
        // The funnel resets paging for the new conditions.
        assert!(session.next_page_plan().is_err());
        assert_eq!(session.quick_filter_selection(), vec!["filterPersonal"]);
        // Second toggle appends in insertion order; toggling off the
        // last one nulls propValueStr.
        let plan = session.toggle_quick_filter("filterUserShopStatus").unwrap();
        assert_eq!(
            plan.data["propValueStr"],
            json!("{\"searchFilter\":\"quickFilter:filterPersonal,filterUserShopStatus\"}")
        );
        session.toggle_quick_filter("filterPersonal").unwrap();
        let plan = session.toggle_quick_filter("filterUserShopStatus").unwrap();
        assert!(plan.data.get("propValueStr").is_none());
        // Undelivered values are refused.
        assert!(session.toggle_quick_filter("sorterOnly").is_err());
        assert!(session.toggle_quick_filter("filterArea").is_err());
    }

    #[test]
    fn session_single_control_toggle_produces_no_send_but_rides_next() {
        // Python round-35: the display toggle flips state only.
        let mut session = started_session();
        assert_eq!(session.single_control(), Some(false));
        assert!(session.toggle_single_control());
        assert_eq!(session.single_control(), Some(true));
        let plan = session.apply_sort("create", "desc").unwrap();
        assert_eq!(plan.data["singleControl"], json!(true));
        // The next successful refresh response overwrites it from
        // resultInfo again ("false" stringified in the fixture).
        session.accept_response(&parsed_page(10, true), false, false);
        assert_eq!(session.single_control(), Some(false));
    }

    #[test]
    fn session_set_panel_price_range_uses_prop_value_str_only() {
        let mut session = started_session();
        let plan = session.set_panel_price_range(Some("100,500")).unwrap();
        assert_eq!(
            plan.data["propValueStr"],
            json!("{\"searchFilter\":\"priceRange:100,500\"}")
        );
        assert!(plan.data.get("startPrice").is_none());
        assert!(plan.data.get("endPrice").is_none());
        let plan = session.set_panel_price_range(None).unwrap();
        assert!(plan.data.get("propValueStr").is_none());
    }
}
